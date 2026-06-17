use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
};
use tracing::debug;

use crate::config::Config;
use crate::protocol::JsonRpcRequest;
use crate::ratelimit::TokenBucket;

#[derive(Clone)]
pub struct HttpState {
    pub config: Arc<Config>,
    pub rate_limiter: Option<Arc<TokenBucket>>,
}

pub async fn create_http_server(
    config: Arc<Config>,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = config.server.host.clone();
    let max_body = config.server.max_request_bytes;
    let rate_limiter = if config.server.rate_limit > 0.0 {
        Some(Arc::new(TokenBucket::new(config.server.rate_limit)))
    } else {
        None
    };
    let state = HttpState { config, rate_limiter };

    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::GET])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .max_age(Duration::from_secs(3600));

    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/health", get(handle_health))
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(cors)
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(ref limiter) = state.rate_limiter {
        if !limiter.try_acquire() {
            tracing::warn!("HTTP rate limit exceeded");
            let err = crate::errors::WebSearchError::RateLimited(
                "Rate limit exceeded. Try again later.".into(),
            );
            let resp = crate::protocol::JsonRpcResponse::error_with_data(
                None,
                err.error_code(),
                err.to_string(),
                err.error_data().unwrap_or_default(),
            );
            return (StatusCode::TOO_MANY_REQUESTS, Json(resp)).into_response();
        }
    }

    if let Some(ref expected) = state.config.server.auth_token {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !crate::server::token_matches(presented, expected) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON-RPC request: {e}"),
            )
                .into_response();
        }
    };

    debug!("HTTP RPC: {} (id={:?})", req.method, req.id);
    let response = crate::server::process_request_http(&req, &state.config).await;
    Json(response).into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({
        "status": "healthy",
        "service": "mcp-web-search",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_health_endpoint() {
        let config = Arc::new(Config::default());
        let state = HttpState { config, rate_limiter: None };
        let app = Router::new()
            .route("/health", get(handle_health))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["service"], "mcp-web-search");
    }

    #[tokio::test]
    async fn test_auth_required() {
        let mut config = Config::default();
        config.server.auth_token = Some(Arc::from("secret123"));
        let config = Arc::new(config);
        let state = HttpState { config: Arc::clone(&config), rate_limiter: None };

        let app = Router::new()
            .route("/rpc", post(handle_rpc))
            .with_state(state);

        // Request without auth token
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let body = serde_json::to_vec(&req).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/rpc")
                    .header(header::CONTENT_TYPE, "application/json")
                    .method(Method::POST)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_with_valid_token() {
        let mut config = Config::default();
        config.server.auth_token = Some(Arc::from("secret123"));
        let config = Arc::new(config);
        let state = HttpState { config, rate_limiter: None };

        let app = Router::new()
            .route("/rpc", post(handle_rpc))
            .with_state(state);

        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let body = serde_json::to_vec(&req).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/rpc")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret123")
                    .method(Method::POST)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["result"], Value::Null);
    }

    #[tokio::test]
    async fn test_body_size_limit() {
        // Config with small limit
        let mut config = Config::default();
        config.server.max_request_bytes = 100;
        let config = Arc::new(config);
        let state = HttpState { config: Arc::clone(&config), rate_limiter: None };

        let app = Router::new()
            .route("/rpc", post(handle_rpc))
            .layer(RequestBodyLimitLayer::new(100))
            .with_state(state);

        // Send a request that exceeds the body limit
        let oversized = "a".repeat(200);
        let body_str = format!(
            r#"{{"jsonrpc":"2.0","method":"ping","id":1,"extra":"{oversized}"}}"#
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/rpc")
                    .header(header::CONTENT_TYPE, "application/json")
                    .method(Method::POST)
                    .body(Body::from(body_str))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
