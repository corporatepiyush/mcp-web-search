use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::Value;
use std::time::Duration;

const ENDPOINT: &str = "https://api.bing.microsoft.com/v7.0/search";

/// Bing Web Search API v7.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(WebSearchError::ConfigError("Bing API key is required".into()));
    }

    let safe = match req.safe_search {
        2 => "Strict",
        1 => "Moderate",
        _ => "Off",
    };
    let count = req.limit.to_string();
    let mut query = vec![
        ("q", req.query.as_str()),
        ("count", count.as_str()),
        ("safeSearch", safe),
    ];
    if req.language != "auto" && !req.language.is_empty() {
        query.push(("mkt", req.language.as_str()));
    }

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.get(ENDPOINT)
            .header("Ocp-Apim-Subscription-Key", &req.api_key)
            .query(&query)
            .send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("Bing search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("Bing request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(WebSearchError::ProviderError(format!(
            "Bing returned HTTP {status}: {body}"
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("Bing invalid JSON: {e}")))?;

    let mut results = Vec::new();
    if let Some(items) = body
        .get("webPages")
        .and_then(|w| w.get("value"))
        .and_then(|v| v.as_array())
    {
        for item in items.iter().take(req.limit) {
            let mut r = SearchResult::new(
                str_field(item, "name"),
                str_field(item, "url"),
                str_field(item, "snippet"),
                "bing",
            );
            r.source = item.get("siteName").and_then(|v| v.as_str()).map(str::to_string);
            r.thumbnail_url = item
                .get("thumbnailUrl")
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
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::ConfigError(_)
        ));
    }
}
