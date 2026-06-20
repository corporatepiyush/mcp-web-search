use super::scrape::{self};
use super::{get_str_array, text_content};
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use futures::stream::StreamExt;
use serde_json::Value;

/// `web_extract` — fetch and clean content from many URLs, concurrently
/// with bounded parallelism. Each URL passes the SSRF guard; failures are
/// reported per-URL without failing the whole call.
///
/// Concurrency is bounded over the **whole** fetch+parse pipeline (not just the
/// IO phase), so at most `concurrency` page bodies are resident at once. This
/// caps peak memory — bodies can be up to `max_response_bytes` each, and an
/// IO-only bound would let every fetched body pile up waiting for the CPU phase.
pub async fn web_extract(args: Option<&Value>, config: &Config) -> Result<Value> {
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

    let concurrency = (num_cpus::get() * 2).max(4);
    let sections: Vec<String> = futures::stream::iter(urls.iter().cloned())
        .map(|url| async move {
            match fetch_page(&url, config).await {
                Ok(page) => {
                    let body = page.body;
                    match tokio::task::spawn_blocking(move || scrape::main_to_markdown(&body)).await
                    {
                        Ok(md) => format!("## Content from {url}\n\n{md}"),
                        Err(_) => {
                            format!("## Failed to extract from {url}\n\nError: parse task panicked")
                        }
                    }
                }
                Err(e) => format!("## Failed to extract from {url}\n\nError: {e}"),
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    Ok(text_content(&sections.join("\n\n---\n\n")))
}

#[cfg_attr(not(test), allow(dead_code))]
async fn extract_one(url: &str, config: &Config) -> String {
    match scrape::extract_main_markdown(url, config).await {
        Ok(md) => format!("## Content from {url}\n\n{md}"),
        Err(e) => format!("## Failed to extract from {url}\n\nError: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_web_extract_requires_urls() {
        let config = Config::default();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_extract(None, &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::InvalidParams(_)
        ));
    }

    #[test]
    fn test_web_extract_empty_urls() {
        let config = Config::default();
        let args = serde_json::json!({"urls": []});
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_extract(Some(&args), &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::InvalidParams(_)
        ));
    }

    #[test]
    fn test_web_extract_rejects_too_many_urls() {
        let mut config = Config::default();
        config.server.max_extract_urls = 2;
        let args = serde_json::json!({"urls": ["https://a.com", "https://b.com", "https://c.com"]});
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_extract(Some(&args), &config));
        assert!(result.is_err());
        if let Err(WebSearchError::InvalidParams(msg)) = result {
            assert!(msg.contains("Too many URLs"));
        } else {
            panic!("Expected InvalidParams");
        }
    }

    #[tokio::test]
    async fn test_extract_one_private_url() {
        let config = Config::default();
        let result = extract_one("http://127.0.0.1:8080/", &config).await;
        // Should report failure gracefully, not panic
        assert!(result.contains("Failed to extract"));
        assert!(result.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn test_extract_one_invalid_url() {
        let config = Config::default();
        let result = extract_one("not a url", &config).await;
        assert!(result.contains("Failed to extract"));
    }
}
