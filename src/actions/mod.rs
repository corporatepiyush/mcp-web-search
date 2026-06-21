pub mod browser;
pub mod extract;
pub mod fetch;
pub mod map;
pub mod scrape;
pub mod search;

use crate::errors::{Result, WebSearchError};
use serde_json::{Value, json};

pub fn text_content(text: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ]
    })
}

pub fn get_str_arg(args: Option<&Value>, key: &str) -> Result<String> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| WebSearchError::InvalidParams(format!("Missing '{key}' parameter")))
}

pub fn get_opt_str(args: Option<&Value>, key: &str) -> Option<String> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn get_opt_usize(args: Option<&Value>, key: &str) -> Option<usize> {
    args.and_then(|a| a.get(key))
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as usize)
}

pub fn get_opt_u8(args: Option<&Value>, key: &str) -> Option<u8> {
    args.and_then(|a| a.get(key))
        .and_then(serde_json::Value::as_u64)
        .map(|v| v.min(u64::from(u8::MAX)) as u8)
}

pub fn get_opt_bool(args: Option<&Value>, key: &str) -> Option<bool> {
    args.and_then(|a| a.get(key)).and_then(|v| v.as_bool())
}

pub fn get_str_array(args: Option<&Value>, key: &str) -> Option<Vec<String>> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_get_str_arg_present() {
        let args = json!({"query": "hello"});
        assert_eq!(get_str_arg(Some(&args), "query").unwrap(), "hello");
    }

    #[test]
    fn test_get_str_arg_missing() {
        let args = json!({"other": "value"});
        let result = get_str_arg(Some(&args), "query");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::InvalidParams(_)
        ));
    }

    #[test]
    fn test_get_str_arg_none() {
        let result = get_str_arg(None, "query");
        assert!(result.is_err());
    }

    #[test]
    fn test_get_opt_str() {
        let args = json!({"key": "value"});
        assert_eq!(get_opt_str(Some(&args), "key").unwrap(), "value");
        assert!(get_opt_str(Some(&args), "missing").is_none());
        assert!(get_opt_str(None, "key").is_none());
    }

    #[test]
    fn test_get_opt_usize() {
        let args = json!({"num": 42});
        assert_eq!(get_opt_usize(Some(&args), "num").unwrap(), 42);
        assert!(get_opt_usize(Some(&args), "missing").is_none());
    }

    #[test]
    fn test_get_opt_u8() {
        let args = json!({"small": 5, "large": 300});
        assert_eq!(get_opt_u8(Some(&args), "small").unwrap(), 5);
        // Should clamp to u8::MAX
        assert_eq!(
            get_opt_u8(Some(&args), "large").unwrap(),
            u8::MAX
        );
    }

    #[test]
    fn test_get_opt_bool() {
        let args = json!({"flag": true, "no": false});
        assert!(get_opt_bool(Some(&args), "flag").unwrap());
        assert!(!get_opt_bool(Some(&args), "no").unwrap());
        assert!(get_opt_bool(Some(&args), "missing").is_none());
    }

    #[test]
    fn test_get_str_array() {
        let args = json!({"items": ["a", "b", "c"]});
        let arr = get_str_array(Some(&args), "items").unwrap();
        assert_eq!(arr, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_get_str_array_missing() {
        let args = json!({});
        assert!(get_str_array(Some(&args), "items").is_none());
    }

    #[test]
    fn test_get_str_array_filters_non_strings() {
        let args = json!({"items": ["a", 1, true, "b"]});
        let arr = get_str_array(Some(&args), "items").unwrap();
        assert_eq!(arr, vec!["a", "b"]);
    }

    #[test]
    fn test_text_content() {
        let v = text_content("hello");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn test_text_content_empty() {
        let v = text_content("");
        assert_eq!(v["content"][0]["text"], "");
    }

    #[test]
    fn test_get_str_arg_invalid_type() {
        let args = json!({"query": 42});
        // as_str() returns None for non-string JSON values
        let result = get_str_arg(Some(&args), "query");
        assert!(result.is_err());
    }
}
