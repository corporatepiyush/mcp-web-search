use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::Value;
use std::time::Duration;

const VALID_TIME_RANGES: &[&str] = &["day", "month", "year"];

/// SearXNG JSON search API.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    let base = req
        .api_url
        .as_deref()
        .ok_or_else(|| WebSearchError::ConfigError("SearXNG API URL is required".into()))?;

    let safesearch_str = req.safe_search.to_string();
    let mut params = vec![
        ("q", req.query.as_str()),
        ("pageno", "1"),
        ("categories", req.categories.as_str()),
        ("format", "json"),
        ("safesearch", safesearch_str.as_str()),
        ("language", req.language.as_str()),
        ("engines", req.engines.as_str()),
    ];
    let time_range_val;
    if VALID_TIME_RANGES.contains(&req.time_range.as_str()) {
        time_range_val = req.time_range.as_str();
        params.push(("time_range", time_range_val));
    }

    let url = format!("{}/search", base.trim_end_matches('/'));
    let mut builder = HTTP.get(&url).query(&params);
    if !req.api_key.is_empty() {
        builder = builder.bearer_auth(&req.api_key);
    }

    let resp = tokio::time::timeout(Duration::from_millis(req.timeout_ms), builder.send())
        .await
        .map_err(|_| WebSearchError::Timeout("SearXNG search timeout".into()))?
        .map_err(|e| WebSearchError::ProviderError(format!("SearXNG request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(WebSearchError::ProviderError(format!(
            "SearXNG returned HTTP {}",
            resp.status()
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("SearXNG invalid JSON: {e}")))?;

    // Check for API-level errors
    if let Some(msg) = body.get("error").and_then(|e| e.as_str()) {
        return Err(WebSearchError::ProviderError(format!(
            "SearXNG API error: {msg}"
        )));
    }

    let mut results = Vec::new();
    if let Some(items) = body.get("results").and_then(|v| v.as_array()) {
        for item in items.iter().take(req.limit) {
            let mut r = SearchResult::new(
                str_field(item, "title"),
                str_field(item, "url"),
                str_field(item, "content"),
                item.get("engine")
                    .and_then(|v| v.as_str())
                    .unwrap_or("searxng"),
            );
            r.source = item.get("source").and_then(|v| v.as_str()).map(str::to_string);
            r.thumbnail_url = item
                .get("thumbnail_src")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            results.push(r);
        }
    }

    Ok(SearchResponse {
        success: true,
        results,
    })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_query() {
        let req = SearchRequest {
            query: "".into(),
            limit: 10,
            language: "auto".into(),
            categories: "general".into(),
            time_range: "".into(),
            safe_search: 0,
            engines: "all".into(),
            timeout_ms: 5000,
            api_key: "".into(),
            api_url: Some("http://localhost:8888".into()),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_str_field() {
        let v = serde_json::json!({"title": "Hello", "empty": "", "num": 42});
        assert_eq!(str_field(&v, "title"), "Hello");
        assert_eq!(str_field(&v, "empty"), "");
        assert_eq!(str_field(&v, "num"), "");
        assert_eq!(str_field(&v, "missing"), "");
    }
}
