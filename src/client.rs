use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use crate::validation::validate_url;
use bytes::Bytes;
use futures::StreamExt;
use std::sync::LazyLock;
use std::time::Duration;
use url::Url;

const USER_AGENT: &str = concat!("mcp-web-search/", env!("CARGO_PKG_VERSION"));

/// Shared HTTP client for all providers and page fetches.
/// Connection pool (HTTP/2), keep-alive, and TLS session reuse across requests.
/// `pool_max_idle_per_host` scales with available CPU cores so high-core-count
/// machines can maintain more concurrent idle connections without port exhaustion.
/// Automatic redirects are disabled so every hop can be re-validated.
pub static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let cpus = num_cpus::get();
    let pool_max = (cpus * 4).max(32);
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(pool_max)
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client")
});

#[derive(Debug)]
pub struct FetchedPage {
    pub final_url: url::Url,
    pub body: String,
}

/// Fetch a page as text, enforcing SSRF validation on every hop, a bounded
/// number of manually-followed redirects, a per-request timeout, and a hard cap
/// on the decompressed body size.
pub async fn fetch_page(start: &str, config: &Config) -> Result<FetchedPage> {
    let mut current = validate_url(start, config.allow_private_hosts).await?;

    for hop in 0..=config.max_redirects {
        let client = if config.dns_pin {
            let pinned = build_pinned_client(&current, config).await?;
            ClientOrPinned::Pinned(pinned)
        } else {
            ClientOrPinned::Shared(&HTTP)
        };
        let client_ref = client.as_ref();

        let resp = tokio::time::timeout(
            config.server.request_timeout,
            client_ref.get(current.clone()).send(),
        )
        .await
        .map_err(|_| WebSearchError::Timeout(format!("fetching {current}")))?
        .map_err(|e| {
            if e.is_timeout() {
                WebSearchError::Timeout(format!("request to {current} timed out"))
            } else if e.is_connect() {
                WebSearchError::HttpError(format!("connection to {current} failed: {e}"))
            } else {
                WebSearchError::HttpError(format!("request to {current} failed: {e}"))
            }
        })?;

        let status = resp.status();
        if status.is_redirection() {
            if hop == config.max_redirects {
                return Err(WebSearchError::HttpError(format!(
                    "exceeded {max} redirects from {start}",
                    max = config.max_redirects
                )));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    WebSearchError::HttpError(format!(
                        "redirect #{hop} from {current} without Location header"
                    ))
                })?;
            // Resolve relative redirects against the current URL, then re-validate.
            let next = current.join(location).map_err(|e| {
                WebSearchError::HttpError(format!(
                    "invalid redirect target '{location}': {e}"
                ))
            })?;
            current = validate_url(next.as_str(), config.allow_private_hosts).await?;
            continue;
        }

        if !status.is_success() {
            let status_code = status.as_u16();
            let body_preview = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            return Err(WebSearchError::HttpError(format!(
                "{current} returned HTTP {status_code}: {body_preview}"
            )));
        }

        let body = read_capped(resp, config.max_response_bytes).await?;
        return Ok(FetchedPage {
            final_url: current,
            body,
        });
    }

    Err(WebSearchError::HttpError(format!(
        "exceeded {} redirects from {start}",
        config.max_redirects
    )))
}

/// Stream a response body, aborting if it exceeds `max` bytes.
/// Accumulates `Bytes` chunks without copying until the final concatenation
/// so IO and accounting can proceed without per-chunk memcpy.
async fn read_capped(resp: reqwest::Response, max: usize) -> Result<String> {
    let mut stream = resp.bytes_stream();
    let mut chunks: Vec<Bytes> = Vec::with_capacity(16);
    let mut total: usize = 0;
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes =
            chunk.map_err(|e| WebSearchError::HttpError(format!("body stream error: {e}")))?;
        total = total.saturating_add(chunk.len());
        if total > max {
            return Err(WebSearchError::ResponseTooLarge(format!(
                "response body exceeds {max} bytes"
            )));
        }
        chunks.push(chunk);
    }
    // Single pass: concatenate all accumulated chunks.
    let mut buf: Vec<u8> = Vec::with_capacity(total.min(max));
    for c in &chunks {
        buf.extend_from_slice(c);
    }
    Ok(String::from_utf8(buf).unwrap_or_else(|e| {
        let s = String::from_utf8_lossy(e.as_bytes()).into_owned();
        tracing::warn!(
            bytes_lost = e.utf8_error().valid_up_to(),
            "Response body contained invalid UTF-8, using lossy replacement"
        );
        s
    }))
}

enum ClientOrPinned {
    Shared(&'static reqwest::Client),
    Pinned(reqwest::Client),
}

impl AsRef<reqwest::Client> for ClientOrPinned {
    fn as_ref(&self) -> &reqwest::Client {
        match self {
            ClientOrPinned::Shared(c) => c,
            ClientOrPinned::Pinned(c) => c,
        }
    }
}

/// Resolve a URL's domain, validate every resolved IP against the SSRF guard,
/// and build a temporary `reqwest::Client` that pins the domain to those IPs.
/// This eliminates the DNS rebinding TOCTOU window between validation and the
/// actual HTTP connection, at the cost of losing connection-pool reuse for that
/// request (typically ~10ms of additional connection setup).
async fn build_pinned_client(url: &Url, config: &Config) -> Result<reqwest::Client> {
    use std::net::SocketAddr;
    use url::Host;

    let domain = match url.host() {
        Some(Host::Domain(d)) => d.to_string(),
        _ => return Err(WebSearchError::UrlNotAllowed(
            "DNS pinning only applies to domain hosts".into(),
        )),
    };

    let port = url.port_or_known_default().unwrap_or(0);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain.as_str(), port))
        .await
        .map_err(|e| WebSearchError::UrlNotAllowed(format!(
            "DNS pinning: cannot resolve '{domain}': {e}"
        )))?
        .collect();

    for addr in &addrs {
        crate::validation::validate_ip(addr.ip(), config.allow_private_hosts)?;
    }

    let cpus = num_cpus::get();
    let pool_max = (cpus * 4).max(32);
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(pool_max)
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(30));

    for addr in &addrs {
        builder = builder.resolve(&domain, *addr);
    }

    builder
        .build()
        .map_err(|e| WebSearchError::HttpError(format!("failed to build pinned client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_agent() {
        assert!(USER_AGENT.starts_with("mcp-web-search/"));
    }

    #[tokio::test]
    async fn test_fetch_page_rejects_bad_url() {
        let config = Config::default();
        let result = fetch_page("not a url", &config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_page_rejects_private() {
        let config = Config::default();
        let result = fetch_page("http://127.0.0.1:8080/", &config).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::UrlNotAllowed(_)
        ));
    }

    #[tokio::test]
    async fn test_fetch_page_allows_private_when_opted_in() {
        // This will fail to connect since nothing is listening, but should
        // pass URL validation. The error should be HttpError (connection refused),
        // not UrlNotAllowed.
        let mut config = Config::default();
        config.allow_private_hosts = true;
        let result = fetch_page("http://127.0.0.1:1/", &config).await;
        match result {
            Err(WebSearchError::HttpError(_)) => {} // expected
            Err(e) => panic!("Expected HttpError, got: {e}"),
            Ok(_) => panic!("Expected error, got Ok"),
        }
    }
}
