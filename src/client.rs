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
        let client = client_for(&current, config).await?;
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
            return Err(WebSearchError::HttpError(format!(
                "{current} returned HTTP {status_code}"
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

/// Stream a response body into a single buffer, aborting as soon as the
/// accumulated size exceeds `max` bytes. Bounds peak memory to roughly the cap
/// (one buffer), rather than holding every chunk plus a concatenated copy.
async fn read_capped(resp: reqwest::Response, max: usize) -> Result<String> {
    let mut stream = resp.bytes_stream();
    // Reserve a modest floor to avoid repeated early reallocations on typical
    // pages, without over-allocating `max` (which may be multiple MiB) for a
    // tiny response.
    let mut buf: Vec<u8> = Vec::with_capacity(max.min(64 * 1024));
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
        buf.extend_from_slice(&chunk);
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

/// An HTTP client to use for a single validated request: either the shared
/// connection-pooled client, or a per-request client that pins the target
/// domain to the IPs we validated (closing the DNS-rebinding TOCTOU window).
#[derive(Debug)]
pub enum ClientOrPinned {
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

/// Select the HTTP client to use for an already-validated URL.
///
/// When `dns_pin` is enabled and the host is a domain, this re-resolves the
/// domain, re-validates every resolved IP against the SSRF guard, and returns a
/// client that pins the domain to exactly those IPs. This is the single choke
/// point every outbound request path must go through so that DNS rebinding
/// cannot redirect a connection to an internal address *after* validation.
///
/// IP-literal hosts cannot rebind, so they use the shared pooled client. When
/// pinning is disabled the shared client is used and a (documented) rebinding
/// window remains.
pub async fn client_for(url: &Url, config: &Config) -> Result<ClientOrPinned> {
    if config.dns_pin && matches!(url.host(), Some(url::Host::Domain(_))) {
        Ok(ClientOrPinned::Pinned(build_pinned_client(url, config).await?))
    } else {
        Ok(ClientOrPinned::Shared(&HTTP))
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

    // EXPLOIT REGRESSION (#2): every outbound path now selects its client via
    // `client_for`. An IP-literal host can't rebind, so it uses the shared pool.
    #[tokio::test]
    async fn test_client_for_ip_literal_uses_shared() {
        let config = Config::default();
        let url = Url::parse("http://1.2.3.4/").unwrap();
        let c = client_for(&url, &config).await.unwrap();
        assert!(matches!(c, ClientOrPinned::Shared(_)));
    }

    // A domain that resolves to an internal address must be rejected at
    // client-construction time (pinning re-resolves + re-validates), closing the
    // DNS-rebinding TOCTOU window for HEAD-based tools (web_check_links,
    // web_fetch_headers) that previously used the shared client directly.
    #[tokio::test]
    async fn test_client_for_domain_resolving_internal_is_rejected() {
        let config = Config::default(); // dns_pin = true, allow_private = false
        let url = Url::parse("http://localhost/").unwrap();
        let res = client_for(&url, &config).await;
        assert!(
            matches!(res, Err(WebSearchError::UrlNotAllowed(_))),
            "pinned client should reject a domain resolving to loopback, got {res:?}"
        );
    }

    // With pinning enabled, an internal-resolving domain is rejected; with
    // --allow-private-hosts it is permitted (operator opt-in).
    #[tokio::test]
    async fn test_client_for_allow_private_permits_internal_domain() {
        let mut config = Config::default();
        config.allow_private_hosts = true;
        let url = Url::parse("http://localhost/").unwrap();
        let res = client_for(&url, &config).await;
        assert!(res.is_ok(), "allow_private should permit localhost pinning");
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
