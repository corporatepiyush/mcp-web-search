use serde::Serialize;

/// Normalized search request passed to every provider backend.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub language: String,
    pub categories: String,
    pub time_range: String,
    pub safe_search: u8,
    pub engines: String,
    pub timeout_ms: u64,
    pub api_key: String,
    pub api_url: Option<String>,
}

/// Normalized single result.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    pub engine: String,
}

impl SearchResult {
    #[must_use]
    pub fn new(title: String, url: String, snippet: String, engine: &str) -> Self {
        Self {
            title,
            url,
            snippet,
            source: None,
            thumbnail_url: None,
            markdown: None,
            engine: engine.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_result_new() {
        let r = SearchResult::new(
            "Title".into(),
            "https://example.com".into(),
            "Snippet".into(),
            "test_engine",
        );
        assert_eq!(r.title, "Title");
        assert_eq!(r.url, "https://example.com");
        assert_eq!(r.snippet, "Snippet");
        assert_eq!(r.engine, "test_engine");
        assert!(r.source.is_none());
        assert!(r.thumbnail_url.is_none());
        assert!(r.markdown.is_none());
    }

    #[test]
    fn test_search_response_serialization() {
        let results = vec![
            SearchResult::new("A".into(), "https://a.com".into(), "Desc A".into(), "eng"),
            SearchResult::new("B".into(), "https://b.com".into(), "Desc B".into(), "eng"),
        ];
        let resp = SearchResponse {
            results,
            success: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""success":true"#));
        assert!(json.contains(r#""title":"A""#));
        assert!(json.contains(r#""title":"B""#));
    }

    #[test]
    fn test_search_result_serialization_omits_optionals() {
        let r = SearchResult::new("X".into(), "https://x.com".into(), "X".into(), "eng");
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("source"));
        assert!(!json.contains("thumbnail_url"));
        assert!(!json.contains("markdown"));
    }
}
