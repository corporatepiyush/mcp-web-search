use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    pub id: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    #[must_use]
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    #[must_use]
    pub fn error(id: Option<Value>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
            id,
        }
    }

    #[must_use]
    pub fn error_with_data(id: Option<Value>, code: i64, message: String, data: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: Some(data),
            }),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serde() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "web_search".to_string(),
            params: Some(json!({"query": "rust"})),
            id: Some(Value::Number(1.into())),
        };
        let json = serde_json::to_string(&req).unwrap();
        let de: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(de.method, "web_search");
        assert_eq!(de.params.unwrap()["query"], "rust");
    }

    #[test]
    fn test_request_no_params() {
        let json = r#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "ping");
        assert!(req.params.is_none());
    }

    #[test]
    fn test_request_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn test_response_success() {
        let resp = JsonRpcResponse::success(Some(Value::Number(1.into())), json!({"ok": true}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""result""#));
        assert!(!json.contains(r#""error""#));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_response_error() {
        let resp =
            JsonRpcResponse::error(Some(Value::Number(1.into())), -32602, "bad params".into());
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "bad params");
        assert!(err.data.is_none());
    }

    #[test]
    fn test_response_error_with_data() {
        let resp = JsonRpcResponse::error_with_data(
            Some(Value::Number(1.into())),
            -32006,
            "rate limited".into(),
            json!({"retryAfter": 1}),
        );
        let err = resp.error.unwrap();
        assert_eq!(err.data.unwrap()["retryAfter"], 1);
    }

    #[test]
    fn test_response_serialize_omits_empty() {
        let resp =
            JsonRpcResponse::error(None, -32601, "not found".into());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains(r#""data""#));
    }
}
