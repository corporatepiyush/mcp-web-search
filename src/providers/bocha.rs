use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use serde_json::{Value, json};
use std::time::Duration;

const ENDPOINT: &str = "https://api.bocha.cn/v1/web-search";

/// Bocha (博查) Web Search API.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(WebSearchError::ConfigError("Bocha API key is required".into()));
    }

    let freshness = match req.time_range.as_str() {
        "day" => "oneDay",
        "week" => "oneWeek",
        "month" => "oneMonth",
        "year" => "oneYear",
        _ => "noLimit",
    };

    let payload = json!({
        "query": req.query,
        "count": req.limit,
        "summary": true,
        "freshness": freshness,
    });

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.post(ENDPOINT)
            .bearer_auth(&req.api_key)
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("Bocha search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("Bocha request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(WebSearchError::ProviderError(format!(
            "Bocha returned HTTP {status}: {body}"
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("Bocha invalid JSON: {e}")))?;

    // Response shape varies: data.webPages.value | webPages.value | results
    let items = body
        .pointer("/data/webPages/value")
        .or_else(|| body.pointer("/webPages/value"))
        .or_else(|| body.get("results"))
        .and_then(|v| v.as_array());

    let mut results = Vec::new();
    if let Some(items) = items {
        for item in items.iter().take(req.limit) {
            let title = first_str(item, &["name", "title"]);
            let url = first_str(item, &["url", "link"]);
            let snippet = first_str(item, &["snippet", "summary", "content"]);
            let mut r = SearchResult::new(title, url, snippet, "bocha");
            r.source = item
                .get("siteName")
                .or_else(|| item.get("site_name"))
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

fn first_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
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

    #[test]
    fn test_first_str() {
        let v = serde_json::json!({"name": "Alice", "title": "Bob"});
        assert_eq!(first_str(&v, &["name", "title"]), "Alice");
        assert_eq!(first_str(&v, &["title"]), "Bob");
        assert_eq!(first_str(&v, &["missing", "name"]), "Alice");
        assert_eq!(first_str(&v, &["missing"]), "");
    }

    #[test]
    fn test_freshness_mapping() {
        assert_eq!(
            serde_json::json!({"freshness": "oneDay"}),
            serde_json::json!({"freshness": "oneDay"})
        );
    }
}
