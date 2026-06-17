use serde_json::Value;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WebSearchError {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Method not found: {0}")]
    MethodNotFound(String),

    #[error("Invalid params: {0}")]
    InvalidParams(String),

    #[error("Search provider error: {0}")]
    ProviderError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("URL not allowed: {0}")]
    UrlNotAllowed(String),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[error("Response too large: {0}")]
    ResponseTooLarge(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Rate limited: {0}")]
    RateLimited(String),
}

impl WebSearchError {
    #[must_use]
    pub const fn error_code(&self) -> i64 {
        match self {
            WebSearchError::ParseError(_) => -32700,
            WebSearchError::MethodNotFound(_) => -32601,
            WebSearchError::InvalidParams(_) => -32602,
            WebSearchError::JsonError(_) => -32700,
            WebSearchError::ProviderError(_) => -32000,
            WebSearchError::ConfigError(_) => -32001,
            WebSearchError::UrlNotAllowed(_) => -32002,
            WebSearchError::HttpError(_) => -32003,
            WebSearchError::ResponseTooLarge(_) => -32004,
            WebSearchError::Timeout(_) => -32005,
            WebSearchError::RateLimited(_) => -32006,
        }
    }

    #[must_use]
    pub fn error_data(&self) -> Option<Value> {
        match self {
            WebSearchError::RateLimited(_) => Some(serde_json::json!({ "retryAfter": 1 })),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, WebSearchError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_codes() {
        assert_eq!(WebSearchError::ParseError("".into()).error_code(), -32700);
        assert_eq!(
            WebSearchError::MethodNotFound("".into()).error_code(),
            -32601
        );
        assert_eq!(
            WebSearchError::InvalidParams("".into()).error_code(),
            -32602
        );
        assert_eq!(
            WebSearchError::UrlNotAllowed("".into()).error_code(),
            -32002
        );
        assert_eq!(WebSearchError::Timeout("".into()).error_code(), -32005);
        assert_eq!(
            WebSearchError::RateLimited("".into()).error_code(),
            -32006
        );
    }

    #[test]
    fn test_error_data_rate_limited() {
        let e = WebSearchError::RateLimited("too many".into());
        let data = e.error_data();
        assert!(data.is_some());
        assert_eq!(data.unwrap()["retryAfter"], 1);
    }

    #[test]
    fn test_error_data_other() {
        let e = WebSearchError::Timeout("timeout".into());
        assert!(e.error_data().is_none());
    }

    #[test]
    fn test_error_display() {
        let e = WebSearchError::InvalidParams("bad input".into());
        assert_eq!(e.to_string(), "Invalid params: bad input");
    }

    #[test]
    fn test_json_error_conversion() {
        let json_err = serde_json::from_str::<serde_json::Value>("{bad json}").unwrap_err();
        let ws_err = WebSearchError::from(json_err);
        assert!(matches!(ws_err, WebSearchError::JsonError(_)));
    }
}
