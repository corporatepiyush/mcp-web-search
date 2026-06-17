use super::{get_opt_str, get_opt_u8, get_opt_usize, get_str_arg, text_content};
use crate::actions::scrape;
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::Result;
use crate::providers::{self, types::SearchRequest};
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use std::sync::Arc;

/// `web_search` — provider-dispatched SERP search.
pub async fn web_search(args: Option<&Value>, config: &Config) -> Result<Value> {
    let query = get_str_arg(args, "query")?;
    if query.trim().is_empty() {
        return Ok(text_content("No search query provided."));
    }

    let d = &config.search;

    let req = SearchRequest {
        query,
        limit: get_opt_usize(args, "limit").unwrap_or(d.limit).clamp(1, 100),
        language: get_opt_str(args, "language").unwrap_or_else(|| d.language.to_string()),
        categories: get_opt_str(args, "categories").unwrap_or_else(|| d.categories.to_string()),
        time_range: get_opt_str(args, "timeRange").unwrap_or_else(|| d.time_range.to_string()),
        safe_search: get_opt_u8(args, "safeSearch").unwrap_or(d.safe_search).min(2),
        engines: get_opt_str(args, "engines").unwrap_or_else(|| d.engines.to_string()),
        timeout_ms: d.timeout.as_millis() as u64,
        api_key: config.api_key.as_deref().unwrap_or("").to_string(),
        api_url: config.api_url.as_ref().map(|s| s.to_string()),
    };

    let resp = providers::search(config.provider, &req).await?;
    Ok(text_content(&format_results(&resp.results)))
}

// ─── web_search_scrape — compound search + scrape ─────────────────────

/// `web_search_scrape` — search the web and scrape the top N results into
/// markdown in a single MCP call. Saves round-trips vs sequential calls.
pub async fn web_search_scrape(args: Option<&Value>, config: &Config) -> Result<Value> {
    let query = get_str_arg(args, "query")?;
    let limit = super::get_opt_usize(args, "limit").unwrap_or(5).clamp(1, 20);
    let scrape_limit = super::get_opt_usize(args, "scrapeLimit").unwrap_or(3).clamp(1, 10);

    if query.trim().is_empty() {
        return Ok(text_content("No search query provided."));
    }

    let d = &config.search;

    let req = SearchRequest {
        query,
        limit,
        language: d.language.to_string(),
        categories: d.categories.to_string(),
        time_range: d.time_range.to_string(),
        safe_search: d.safe_search.min(2),
        engines: d.engines.to_string(),
        timeout_ms: d.timeout.as_millis() as u64,
        api_key: config.api_key.as_deref().unwrap_or("").to_string(),
        api_url: config.api_url.as_ref().map(|s| s.to_string()),
    };

    let resp = providers::search(config.provider, &req).await?;

    let results = resp.results;
    if results.is_empty() {
        return Ok(text_content("No results found."));
    }

    let to_scrape = results.len().min(scrape_limit);
    let cpus = num_cpus::get();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(cpus * 2));
    let mut sections: Vec<String> = Vec::with_capacity(to_scrape);
    let mut futures = FuturesUnordered::new();

    for r in &results[..to_scrape] {
        let url = r.url.clone();
        let sem = Arc::clone(&semaphore);
        futures.push(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let io_result = fetch_page(&url, config).await;
            drop(_permit);
            match io_result {
                Ok(page) => {
                    let body = page.body;
                    tokio::task::spawn_blocking(move || scrape::main_to_markdown(&body))
                        .await
                        .unwrap_or_default()
                }
                Err(e) => format!("[Failed to fetch: {e}]"),
            }
        });
    }

    while let Some(content) = futures.next().await {
        sections.push(content);
    }

    let mut output = String::new();
    for (i, r) in results[..to_scrape].iter().enumerate() {
        if i < sections.len() {
            output.push_str(&format!(
                "## {} — {}\n\n{}\n\n{}\n\n---\n\n",
                r.title, r.url, r.snippet, sections[i]
            ));
        }
    }

    // Append remaining (unscraped) results as plain SERP entries
    for r in &results[to_scrape..] {
        output.push_str(&format!("## {} — {}\n\n{}\n\n---\n\n", r.title, r.url, r.snippet));
    }

    Ok(text_content(if output.is_empty() {
        "No results found."
    } else {
        &output
    }))
}

fn format_results(results: &[crate::providers::types::SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }
    results
        .iter()
        .map(|r| {
            let mut block = format!(
                "Title: {}\nURL: {}\nDescription: {}",
                r.title, r.url, r.snippet
            );
            if let Some(ref md) = r.markdown
                && !md.is_empty()
            {
                block.push_str("\nContent: ");
                block.push_str(md);
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::SearchResult;

    #[test]
    fn test_format_empty() {
        assert_eq!(format_results(&[]), "No results found.");
    }

    #[test]
    fn test_format_results() {
        let r = SearchResult::new(
            "Rust".into(),
            "https://rust-lang.org".into(),
            "A language".into(),
            "duckduckgo",
        );
        let out = format_results(&[r]);
        assert!(out.contains("Title: Rust"));
        assert!(out.contains("URL: https://rust-lang.org"));
        assert!(out.contains("Description: A language"));
    }

    #[test]
    fn test_format_results_multiple() {
        let results = vec![
            SearchResult::new(
                "First".into(),
                "https://first.com".into(),
                "First result".into(),
                "ddg",
            ),
            SearchResult::new(
                "Second".into(),
                "https://second.com".into(),
                "Second result".into(),
                "ddg",
            ),
        ];
        let out = format_results(&results);
        assert!(out.contains("Title: First"));
        assert!(out.contains("Title: Second"));
        assert!(out.contains("\n\n")); // separator between results
    }

    #[test]
    fn test_format_results_with_markdown() {
        let mut r = SearchResult::new(
            "Page".into(),
            "https://page.com".into(),
            "A page".into(),
            "exa",
        );
        r.markdown = Some("# Full Content".into());
        let out = format_results(&[r]);
        assert!(out.contains("Content: # Full Content"));
    }

    #[test]
    fn test_format_results_empty_markdown() {
        let mut r = SearchResult::new(
            "Page".into(),
            "https://page.com".into(),
            "A page".into(),
            "exa",
        );
        r.markdown = Some("".into());
        let out = format_results(&[r]);
        assert!(!out.contains("Content:"));
    }

    #[test]
    fn test_web_search_empty_query() {
        let config = Config::default();
        let args = serde_json::json!({"query": ""});
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_search(Some(&args), &config));
        assert!(result.is_ok());
        let text = result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert_eq!(text, "No search query provided.");
    }

    #[test]
    fn test_web_search_requires_query() {
        let config = Config::default();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_search(None, &config));
        assert!(result.is_err());
    }

    // ─── web_search_scrape tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_web_search_scrape_requires_query() {
        let config = Config::default();
        let result = web_search_scrape(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), crate::errors::WebSearchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_web_search_scrape_empty_query() {
        let config = Config::default();
        let args = serde_json::json!({"query": ""});
        let result = web_search_scrape(Some(&args), &config).await;
        assert!(result.is_ok());
        let text = result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert_eq!(text, "No search query provided.");
    }

    #[test]
    fn test_web_search_scrape_limits_clamped() {
        let config = Config::default();
        let args = serde_json::json!({"query": "test", "limit": 50, "scrapeLimit": 50});
        // The function should accept the clamped values without error
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_search_scrape(Some(&args), &config));
        // May fail with provider error (no API key), but not with InvalidParams
        if let Err(e) = result {
            assert!(!matches!(e, crate::errors::WebSearchError::InvalidParams(_)));
        }
    }

    #[test]
    fn test_web_search_scrape_default_params() {
        let config = Config::default();
        let args = serde_json::json!({"query": "test"});
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_search_scrape(Some(&args), &config));
        // Should not fail with param errors (may fail with provider error)
        if let Err(e) = result {
            assert!(!matches!(e, crate::errors::WebSearchError::InvalidParams(_)));
        }
    }
}
