use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::Value;
use std::time::Duration;

const ENDPOINT: &str = "https://www.googleapis.com/customsearch/v1";

/// Google Custom Search JSON API.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(WebSearchError::ConfigError("Google API key is required".into()));
    }
    let cx = req.api_url.as_deref().ok_or_else(|| {
        WebSearchError::ConfigError("Google requires SEARCH_API_URL set to the CSE id".into())
    })?;

    let num = req.limit.min(10).to_string();
    let mut query = vec![
        ("key", req.api_key.as_str()),
        ("cx", cx),
        ("q", req.query.as_str()),
        ("num", num.as_str()),
    ];
    let lang;
    if req.language != "auto" && !req.language.is_empty() {
        lang = format!("lang_{}", req.language);
        query.push(("lr", lang.as_str()));
    }

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.get(ENDPOINT).query(&query).send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("Google search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("Google request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(WebSearchError::ProviderError(format!(
            "Google returned HTTP {status}: {body}"
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("Google invalid JSON: {e}")))?;

    if let Some(msg) = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(WebSearchError::ProviderError(format!("Google API error: {msg}")));
    }

    let mut results = Vec::new();
    if let Some(items) = body.get("items").and_then(|v| v.as_array()) {
        for item in items.iter().take(req.limit) {
            let mut r = SearchResult::new(
                str_field(item, "title"),
                str_field(item, "link"),
                str_field(item, "snippet"),
                "google",
            );
            r.source = item
                .get("displayLink")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(pagemap) = item.get("pagemap")
                && let Some(og) = pagemap.get("og").and_then(|v| v.as_array()) {
                    let _ = og; // reserved for future thumbnail extraction
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
            api_url: Some("cx_id".into()),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_cx() {
        let req = SearchRequest {
            query: "rust".into(),
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
}
