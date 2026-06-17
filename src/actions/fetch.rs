use super::{get_str_arg, get_str_array, text_content};
use crate::client::{client_for, fetch_page};
use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use crate::validation::validate_url;
use futures::stream::{FuturesUnordered, StreamExt};
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use serde_json::Value;
use std::sync::Arc;

/// `web_fetch_headers` — fetch only the HTTP response headers and status code
/// for a URL without downloading the body. Uses HEAD requests with manual
/// redirect following (bounded by `config.max_redirects`).
pub async fn web_fetch_headers(args: Option<&Value>, config: &Config) -> Result<Value> {
    let url = get_str_arg(args, "url")?;
    let mut current = validate_url(&url, config.allow_private_hosts).await?;

    for hop in 0..=config.max_redirects {
        // Route through the DNS-pinned client so the connection cannot be
        // redirected to an internal IP after validation (DNS rebinding).
        let client = client_for(&current, config).await?;
        let resp = tokio::time::timeout(
            config.server.request_timeout,
            client.as_ref().head(current.clone()).send(),
        )
        .await
        .map_err(|_| WebSearchError::Timeout(format!("HEAD {current}")))?
        .map_err(|e| {
            if e.is_timeout() {
                WebSearchError::Timeout(format!("HEAD {current} timed out"))
            } else if e.is_connect() {
                WebSearchError::HttpError(format!("connection to {current} failed: {e}"))
            } else {
                WebSearchError::HttpError(format!("HEAD {current} failed: {e}"))
            }
        })?;

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    WebSearchError::HttpError(format!(
                        "redirect #{hop} from {current} without Location header"
                    ))
                })?;
            let next = current.join(location).map_err(|e| {
                WebSearchError::HttpError(format!("invalid redirect target '{location}': {e}"))
            })?;
            current = validate_url(next.as_str(), config.allow_private_hosts).await?;
            continue;
        }

        let status_code = status.as_u16();
        let status_text = status.canonical_reason().unwrap_or("Unknown");

        let mut lines = format!("URL: {current}\nStatus: {status_code} {status_text}\n\nHeaders:\n");
        for (name, value) in resp.headers() {
            if let Ok(v) = value.to_str() {
                use std::fmt::Write;
                let _ = write!(lines, "{}: {}\n", name.as_str(), v);
            }
        }

        return Ok(text_content(&lines));
    }

    Err(WebSearchError::HttpError(format!(
        "exceeded {} redirects from {url}",
        config.max_redirects
    )))
}

/// `web_check_links` — check whether a list of URLs are reachable by sending
/// HEAD requests in parallel. Returns HTTP status code and final URL for each.
pub async fn web_check_links(args: Option<&Value>, config: &Config) -> Result<Value> {
    let urls = get_str_array(args, "urls")
        .filter(|u| !u.is_empty())
        .ok_or_else(|| WebSearchError::InvalidParams("Missing 'urls' parameter".into()))?;

    let max_urls = config.server.max_extract_urls;
    if urls.len() > max_urls {
        return Err(WebSearchError::InvalidParams(format!(
            "Too many URLs: provided {}, maximum is {max_urls}",
            urls.len()
        )));
    }

    let cpus = num_cpus::get();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(cpus * 2));
    let mut sections = Vec::with_capacity(urls.len());
    let mut futures = FuturesUnordered::new();

    for url in &urls {
        let sem = Arc::clone(&semaphore);
        let url = url.clone();
        futures.push(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            check_one_link(&url, config).await
        });
    }

    while let Some(result) = futures.next().await {
        sections.push(result);
    }

    Ok(text_content(&sections.join("\n")))
}

async fn check_one_link(url: &str, config: &Config) -> String {
    let current = match validate_url(url, config.allow_private_hosts).await {
        Ok(u) => u,
        Err(e) => return format!("✗ {url} — {e}"),
    };

    // Route through the DNS-pinned client (defeats rebinding between the
    // validation above and the actual connection below).
    let client = match client_for(&current, config).await {
        Ok(c) => c,
        Err(e) => return format!("✗ {url} — {e}"),
    };

    match tokio::time::timeout(
        config.server.request_timeout,
        client.as_ref().head(current.clone()).send(),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let status = resp.status();
            let code = status.as_u16();
            let reason = status.canonical_reason().unwrap_or("Unknown");
            let icon = if code < 400 { "✓" } else { "✗" };
            format!("{icon} {url} — {code} {reason}")
        }
        Ok(Err(e)) => {
            if e.is_timeout() {
                format!("⚠ {url} — Timeout")
            } else if e.is_connect() {
                format!("✗ {url} — Connection failed")
            } else {
                format!("✗ {url} — {e}")
            }
        }
        Err(_) => format!("⚠ {url} — Timeout"),
    }
}

/// `web_sitemap` — parse a website's sitemap.xml and return the discovered URLs.
pub async fn web_sitemap(args: Option<&Value>, config: &Config) -> Result<Value> {
    let url = get_str_arg(args, "url")?;
    let limit = super::get_opt_usize(args, "limit")
        .unwrap_or(config.server.max_map_urls)
        .min(config.server.max_map_urls);

    let start_url = validate_url(&url, config.allow_private_hosts).await?;

    // Derive sitemap URL from origin
    let origin = start_url
        .origin()
        .ascii_serialization()
        .parse::<url::Url>()
        .map_err(|_| WebSearchError::HttpError("invalid origin URL".into()))?;

    let sitemap_url = origin
        .join("/sitemap.xml")
        .map_err(|_| WebSearchError::HttpError("invalid sitemap URL".into()))?;

    let page = match fetch_page(sitemap_url.as_str(), config).await {
        Ok(p) => p,
        Err(e) => {
            return Ok(text_content(&format!("Failed to fetch sitemap: {e}")));
        }
    };

    let body = page.body;

    tokio::task::spawn_blocking(move || -> String {
        let mut reader = XmlReader::from_str(&body);
        reader.config_mut().trim_text(true);
        let mut out = Vec::new();
        let mut in_loc = false;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    if e.name().as_ref() == b"loc" {
                        in_loc = true;
                    }
                }
                Ok(Event::Text(ref t)) if in_loc => {
                    let text = t.unescape().unwrap_or_default().to_string();
                    if !text.is_empty() && out.len() < limit {
                        out.push(text);
                    }
                }
                Ok(Event::End(_)) => {
                    in_loc = false;
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        if out.is_empty() {
            "No URLs found in sitemap".to_string()
        } else {
            out.join("\n")
        }
    })
    .await
    .map_err(|e| WebSearchError::ProviderError(format!("sitemap parse task failed: {e}")))
    .map(|text| text_content(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_web_fetch_headers_requires_url() {
        let config = Config::default();
        let result = web_fetch_headers(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_web_fetch_headers_rejects_private() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://127.0.0.1:8080/"});
        let result = web_fetch_headers(Some(&args), &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::UrlNotAllowed(_)));
    }

    #[tokio::test]
    async fn test_web_check_links_requires_urls() {
        let config = Config::default();
        let result = web_check_links(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_web_check_links_empty_urls() {
        let config = Config::default();
        let args = serde_json::json!({"urls": []});
        let result = web_check_links(Some(&args), &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    // EXPLOIT REGRESSION (#2): web_check_links must refuse private/metadata
    // targets. (Validation rejects literal internal IPs up front; the pinned
    // client closes the rebinding window for domains — see client.rs tests.)
    #[tokio::test]
    async fn test_web_check_links_blocks_private_and_metadata() {
        let config = Config::default();
        let args = serde_json::json!({"urls": [
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
        ]});
        let result = web_check_links(Some(&args), &config).await.unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        // Every line is a failure, and none report a successful HTTP status.
        assert_eq!(text.matches('✗').count(), 3, "all three must be blocked: {text}");
        assert!(text.contains("URL not allowed"), "expected SSRF rejection: {text}");
        assert!(!text.contains('✓'), "no target should have connected: {text}");
    }

    // EXPLOIT REGRESSION (#2): web_fetch_headers must refuse the cloud metadata
    // endpoint just like the body-fetching tools.
    #[tokio::test]
    async fn test_web_fetch_headers_blocks_metadata() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://169.254.169.254/latest/meta-data/"});
        let result = web_fetch_headers(Some(&args), &config).await;
        assert!(matches!(result.unwrap_err(), WebSearchError::UrlNotAllowed(_)));
    }

    #[tokio::test]
    async fn test_web_sitemap_requires_url() {
        let config = Config::default();
        let result = web_sitemap(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    #[test]
    fn test_sitemap_xml_parsing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/</loc></url>
  <url><loc>https://example.com/about</loc></url>
</urlset>"#;

        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut urls = Vec::new();
        let mut in_loc = false;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    if e.name().as_ref() == b"loc" {
                        in_loc = true;
                    }
                }
                Ok(Event::Text(ref t)) if in_loc => {
                    let text = t.unescape().unwrap_or_default().to_string();
                    if !text.is_empty() {
                        urls.push(text);
                    }
                }
                Ok(Event::End(_)) => in_loc = false,
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://example.com/".to_string()));
        assert!(urls.contains(&"https://example.com/about".to_string()));
    }

    #[test]
    fn test_sitemap_xml_empty() {
        let xml = r#"<?xml version="1.0"?><urlset></urlset>"#;
        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut urls = Vec::new();
        let mut in_loc = false;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    if e.name().as_ref() == b"loc" {
                        in_loc = true;
                    }
                }
                Ok(Event::Text(ref t)) if in_loc => {
                    let text = t.unescape().unwrap_or_default().to_string();
                    if !text.is_empty() {
                        urls.push(text);
                    }
                }
                Ok(Event::End(_)) => in_loc = false,
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        assert!(urls.is_empty());
    }

    #[test]
    fn test_sitemap_xml_sitemapindex() {
        let xml = r#"<?xml version="1.0"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/sitemap1.xml</loc></sitemap>
</sitemapindex>"#;

        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut urls = Vec::new();
        let mut in_loc = false;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    if e.name().as_ref() == b"loc" {
                        in_loc = true;
                    }
                }
                Ok(Event::Text(ref t)) if in_loc => {
                    let text = t.unescape().unwrap_or_default().to_string();
                    if !text.is_empty() {
                        urls.push(text);
                    }
                }
                Ok(Event::End(_)) => in_loc = false,
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        assert_eq!(urls.len(), 1);
        assert!(urls.contains(&"https://example.com/sitemap1.xml".to_string()));
    }
}
