use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::{Value, json};
use std::time::Duration;

const ENDPOINT: &str = "https://api.tavily.com/search";
const VALID_TOPICS: &[&str] = &["general", "news", "finance"];

/// Tavily Search API.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(WebSearchError::ConfigError("Tavily API key is required".into()));
    }

    let mut payload = json!({
        "query": req.query,
        "max_results": req.limit,
    });
    if VALID_TOPICS.contains(&req.categories.as_str()) {
        payload["topic"] = json!(req.categories);
    }
    if !req.time_range.is_empty() && req.time_range != "all" {
        payload["time_range"] = json!(req.time_range);
    }

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.post(ENDPOINT)
            .bearer_auth(&req.api_key)
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("Tavily search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("Tavily request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(WebSearchError::ProviderError(format!(
            "Tavily returned HTTP {status}: {body}"
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("Tavily invalid JSON: {e}")))?;

    let mut results = Vec::new();
    if let Some(items) = body.get("results").and_then(|v| v.as_array()) {
        for item in items.iter().take(req.limit) {
            let mut r = SearchResult::new(
                str_field(item, "title"),
                str_field(item, "url"),
                str_field(item, "content"),
                "tavily",
            );
            // Tavily can return a score
            if let Some(score) = item.get("score").and_then(|v| v.as_f64()) {
                r.snippet = format!("[Score: {score:.2}] {}", r.snippet);
            }
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
            api_key: "key".into(),
            api_url: None,
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_api_key() {
        let req = SearchRequest {
            query: "rust".into(),
            limit: 10,
            language: "auto".into(),
            categories: "general".into(),
            time_range: "".into(),
            safe_search: 0,
            engines: "all".into(),
            timeout_ms: 5000,
            api_key: "".into(),
            api_url: None,
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        assert!(result.is_err());
    }
}
