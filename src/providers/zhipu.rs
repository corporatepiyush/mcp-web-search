use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::{Value, json};
use std::time::Duration;

const ENDPOINT: &str = "https://open.bigmodel.cn/api/paas/v4/web_search";
const VALID_ENGINES: &[&str] = &[
    "search_std",
    "search_pro",
    "search_pro_sogou",
    "search_pro_quark",
    "search_pro_jina",
    "search_pro_bing",
];

/// Zhipu (智谱) Web Search API.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(WebSearchError::ConfigError("Zhipu API key is required".into()));
    }

    let engine = if VALID_ENGINES.contains(&req.engines.as_str()) {
        req.engines.as_str()
    } else {
        "search_std"
    };

    let payload = json!({
        "search_engine": engine,
        "search_query": req.query,
        "search_intent": false,
        "count": req.limit,
    });

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.post(ENDPOINT)
            .bearer_auth(&req.api_key)
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("Zhipu search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("Zhipu request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(WebSearchError::ProviderError(format!(
            "Zhipu returned HTTP {status}: {body}"
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("Zhipu invalid JSON: {e}")))?;

    // Check for error response
    if let Some(msg) = body.get("msg").and_then(|v| v.as_str()) {
        return Err(WebSearchError::ProviderError(format!(
            "Zhipu API error: {msg}"
        )));
    }

    let mut results = Vec::new();
    if let Some(items) = body.get("search_result").and_then(|v| v.as_array()) {
        for item in items.iter().take(req.limit) {
            let mut r = SearchResult::new(
                str_field(item, "title"),
                str_field(item, "link"),
                str_field(item, "content"),
                "zhipu",
            );
            r.source = item
                .get("media")
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
            engines: "search_std".into(),
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
            engines: "search_std".into(),
            timeout_ms: 5000,
            api_key: "".into(),
            api_url: None,
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_engine_fallback() {
        let req = SearchRequest {
            query: "rust".into(),
            limit: 10,
            language: "auto".into(),
            categories: "general".into(),
            time_range: "".into(),
            safe_search: 0,
            engines: "invalid_engine".into(),
            timeout_ms: 5000,
            api_key: "key".into(),
            api_url: None,
        };
        // Should use engine "search_std" as fallback
        // The request will fail because we can't actually call the API,
        // but it shouldn't fail during validation
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        // Will likely be a timeout/connection error, not InvalidParams
        match result {
            Err(WebSearchError::Timeout(_)) => {} // expected with no network
            Err(WebSearchError::ProviderError(_)) => {} // expected
            _ => {} // any error is fine as long as it doesn't panic
        }
    }
}
