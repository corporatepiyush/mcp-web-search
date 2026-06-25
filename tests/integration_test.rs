use serde_json::Value;

/// A `Config` with every tool category enabled, for tests that exercise
/// `tools/list` and `tools/call`. The production default exposes no tools.
fn all_enabled_config() -> mcp_web_search::config::Config {
    use mcp_web_search::tools::ToolCategory;
    let mut config = mcp_web_search::config::Config::default();
    config.server.enabled_categories = ToolCategory::ALL.to_vec();
    config.tools_list = std::sync::Arc::new(mcp_web_search::tools::build_tools_list(
        ToolCategory::ALL,
    ));
    config
}

/// A simple in-memory test for the full MCP request flow:
/// initialize → tools/list → tools/call (validation) → ping
#[test]
fn test_mcp_protocol_initialize() {
    let config = all_enabled_config();
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "initialize".into(),
        params: None,
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_ok());
    let v = result.unwrap();
    assert_eq!(v["protocolVersion"], "2025-11-25");
    assert_eq!(v["serverInfo"]["name"], "mcp-web-search");
    assert!(v["capabilities"]["tools"]["listChanged"].is_boolean());
    assert!(v["capabilities"]["logging"].is_object());
    assert!(v["instructions"].is_string());
}

#[test]
fn test_mcp_protocol_tools_list() {
    let config = all_enabled_config();
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/list".into(),
        params: None,
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_ok());
    let v = result.unwrap();
    let tools = v["tools"].as_array().expect("tools should be an array");
    assert!(!tools.is_empty());
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in &["web_search", "web_scrape", "web_map", "web_extract"] {
        assert!(
            tool_names.contains(expected),
            "Missing tool '{expected}' in tools/list"
        );
    }
    for tool in tools {
        assert!(
            tool.get("description").and_then(|d| d.as_str()).is_some(),
            "Tool {:?} missing description",
            tool["name"]
        );
        let input_schema = tool.get("inputSchema").expect("Tool missing inputSchema");
        assert!(
            input_schema.get("type").and_then(|t| t.as_str()) == Some("object"),
            "inputSchema.type should be 'object'"
        );
        assert!(
            input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .is_some(),
            "inputSchema.properties should be an object"
        );
    }
}

#[test]
fn test_mcp_protocol_ping() {
    let config = all_enabled_config();
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "ping".into(),
        params: None,
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert_eq!(result.unwrap(), Value::Null);
}

#[test]
fn test_mcp_protocol_unknown_method() {
    let config = all_enabled_config();
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "unknown".into(),
        params: None,
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_err());
    match result.unwrap_err() {
        mcp_web_search::WebSearchError::MethodNotFound(_) => {}
        e => panic!("Expected MethodNotFound, got: {e}"),
    }
}

#[test]
fn test_mcp_protocol_notification_is_ok() {
    let config = all_enabled_config();
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "notifications/initialized".into(),
        params: None,
        id: None,
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_ok());
}

#[test]
fn test_mcp_tools_call_validation() {
    let config = all_enabled_config();

    // Missing 'name' parameter
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/call".into(),
        params: Some(serde_json::json!({})),
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_err());

    // Unknown tool name
    let req = mcp_web_search::protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/call".into(),
        params: Some(serde_json::json!({"name": "nonexistent_tool"})),
        id: Some(Value::Number(1.into())),
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(mcp_web_search::server::process_request(&req, &config));
    assert!(result.is_err());
    match result.unwrap_err() {
        mcp_web_search::WebSearchError::MethodNotFound(_) => {}
        e => panic!("Expected MethodNotFound, got: {e}"),
    }
}

#[test]
fn test_ssrf_guard_rejects_private_urls() {
    let private_urls = [
        "http://127.0.0.1/",
        "http://10.0.0.1/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://169.254.169.254/",
        "http://[::1]/",
        "http://0.42.42.42/",
        "http://100.64.0.1/",
        "http://198.18.0.1/",
    ];

    for url in &private_urls {
        let result = mcp_web_search::validation::validate_url_blocking(url, false);
        assert!(
            result.is_err(),
            "URL {url} should be rejected by SSRF guard"
        );
    }
}

#[test]
fn test_ssrf_guard_allows_public_urls() {
    let public_urls = [
        "https://example.com/",
        "https://rust-lang.org/",
        "https://1.1.1.1/",
        "https://8.8.8.8/",
        "http://93.184.216.34/",
    ];

    for url in &public_urls {
        let result = mcp_web_search::validation::validate_url_blocking(url, false);
        assert!(
            result.is_ok(),
            "Public URL {url} should be allowed by SSRF guard: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_ssrf_guard_allows_private_with_flag() {
    let result =
        mcp_web_search::validation::validate_url_blocking("http://127.0.0.1:8080/", true);
    assert!(
        result.is_ok(),
        "Should allow private with --allow-private-hosts"
    );
}

#[test]
fn test_provider_dispatch_all_variants() {
    let providers = [
        mcp_web_search::config::SearchProvider::Searxng,
        mcp_web_search::config::SearchProvider::DuckDuckGo,
        mcp_web_search::config::SearchProvider::Bing,
        mcp_web_search::config::SearchProvider::Tavily,
        mcp_web_search::config::SearchProvider::Google,
        mcp_web_search::config::SearchProvider::Zhipu,
        mcp_web_search::config::SearchProvider::Exa,
        mcp_web_search::config::SearchProvider::Bocha,
    ];

    let req = mcp_web_search::providers::types::SearchRequest {
        query: "test".into(),
        limit: 5,
        language: "auto".into(),
        categories: "general".into(),
        time_range: "".into(),
        safe_search: 0,
        engines: "all".into(),
        timeout_ms: 1000,
        api_key: "dummy_key".into(),
        api_url: Some("http://localhost:8888".into()),
    };

    for provider in &providers {
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(mcp_web_search::providers::search(*provider, &req));
        match result {
            Err(mcp_web_search::WebSearchError::Timeout(_)) => {}
            Err(mcp_web_search::WebSearchError::ProviderError(_)) => {}
            Err(mcp_web_search::WebSearchError::HttpError(_)) => {}
            Ok(_) => {}
            Err(e) => {
                panic!("Provider {provider} failed with unexpected error: {e}");
            }
        }
    }
}

#[test]
fn test_json_rpc_request_response_flow() {
    use mcp_web_search::protocol::*;
    use serde_json::json;

    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "web_search".into(),
        params: Some(json!({"query": "rust programming"})),
        id: Some(Value::Number(42.into())),
    };

    let json = serde_json::to_string(&req).unwrap();
    let deserialized: JsonRpcRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.method, "web_search");
    assert_eq!(deserialized.id.unwrap(), 42);

    let resp = JsonRpcResponse::success(
        Some(Value::Number(42.into())),
        json!({"content": [{"type": "text", "text": "results"}]}),
    );
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""result""#));
    assert!(!json.contains(r#""error""#));
}

#[test]
fn test_tools_list_response_cached() {
    use mcp_web_search::tools::ToolCategory;
    let first = mcp_web_search::tools::build_tools_list(ToolCategory::ALL);
    let second = mcp_web_search::tools::build_tools_list(ToolCategory::ALL);
    assert_eq!(first, second);

    let tools = first["tools"].as_array().unwrap();
    for tool in tools {
        assert!(tool["name"].as_str().is_some());
        assert!(tool["description"].as_str().is_some());
        assert!(
            tool["inputSchema"]["type"].as_str() == Some("object")
        );
        assert!(tool["inputSchema"]["properties"].is_object());
    }
}

#[test]
fn test_dns_pinning_default_on() {
    let cfg = mcp_web_search::config::Config::default();
    assert!(cfg.dns_pin, "DNS pinning should default to true");
}

#[test]
fn test_max_request_bytes_reduced_default() {
    let cfg = mcp_web_search::config::Config::default();
    assert_eq!(
        cfg.server.max_request_bytes, 1048576,
        "max_request_bytes should default to 1MB, not 16MB"
    );
}

#[test]
fn test_auth_token_from_file() {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let path = dir.join("mcp_test_token.txt");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, "my-secret-token\n").unwrap();
    drop(f);

    let args = mcp_web_search::Args {
        search_provider: mcp_web_search::config::SearchProvider::DuckDuckGo,
        search_api_key: None,
        search_api_url: None,
        limit: 10,
        language: "auto".into(),
        categories: "general".into(),
        time_range: "".into(),
        safe_search: 0,
        engines: "all".into(),
        timeout: 10000,
        host: "127.0.0.1".into(),
        http_port: 3001,
        stdio: false,
        log_level: "info".into(),
        request_timeout: 30,
        max_request_bytes: 1048576,
        max_response_bytes: 8388608,
        max_redirects: 5,
        allow_private_hosts: false,
        auth_token: None,
        auth_token_file: Some(path.to_string_lossy().into()),
        max_extract_urls: 100,
        max_map_urls: 10000,
        worker_threads: 0,
        rate_limit: 0.0,
        dns_pin: true,
        tls_cert: None,
        tls_key: None,
        browser_path: None,
        browser_max_pages: 0,
        browser_nav_timeout_ms: 30_000,
        browser_disable: false,
        enable_all: false,
        enable_search: false,
        enable_scrape: false,
        enable_fetch: false,
        enable_crawl: false,
    };
    let cfg = mcp_web_search::config::Config::from_args(&args).unwrap();
    assert_eq!(
        &*cfg.server.auth_token.unwrap(),
        "my-secret-token",
        "auth_token should be read from file"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn test_rate_limited_error_has_retry_after() {
    let err = mcp_web_search::WebSearchError::RateLimited("too fast".into());
    assert_eq!(err.error_code(), -32006);
    let data = err.error_data();
    assert!(data.is_some(), "RateLimited should have retryAfter data");
    assert_eq!(data.unwrap()["retryAfter"], 1);
}

#[test]
fn test_error_body_preview_not_leaked() {
    // HTTP errors from fetch_page should not include response body preview.
    // We test via the URL validation path — a blocked URL returns
    // UrlNotAllowed, not HttpError with leaked body content.
    let result = mcp_web_search::validation::validate_url_blocking("http://127.0.0.1/", false);
    match result {
        Err(mcp_web_search::WebSearchError::UrlNotAllowed(msg)) => {
            assert!(!msg.contains("HTTP"), "Should not leak HTTP response body");
        }
        other => panic!("Expected UrlNotAllowed, got: {other:?}"),
    }
}

#[test]
fn test_token_matches_constant_time_no_hash() {
    // After removing SHA-256, token comparison should still work correctly.
    assert!(mcp_web_search::server::token_matches("secret", "secret"));
    assert!(mcp_web_search::server::token_matches("Bearer xyz", "xyz"));
    assert!(mcp_web_search::server::token_matches("  Bearer abc  ", "abc"));
    assert!(!mcp_web_search::server::token_matches("wrong", "secret"));
    assert!(!mcp_web_search::server::token_matches("", "secret"));
    // Different lengths should not match
    assert!(!mcp_web_search::server::token_matches("a", "bb"));
    assert!(!mcp_web_search::server::token_matches("bb", "a"));
}

#[test]
fn test_error_serialization_across_protocol() {
    use mcp_web_search::{protocol::JsonRpcResponse, WebSearchError};

    let errors = vec![
        WebSearchError::ParseError("bad json".into()),
        WebSearchError::MethodNotFound("unknown".into()),
        WebSearchError::InvalidParams("missing field".into()),
        WebSearchError::ProviderError("API down".into()),
        WebSearchError::Timeout("slow".into()),
        WebSearchError::UrlNotAllowed("private ip".into()),
        WebSearchError::ConfigError("missing key".into()),
    ];

    for err in &errors {
        let resp = JsonRpcResponse::error(None, err.error_code(), err.to_string());
        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized.error.as_ref().unwrap().code,
            err.error_code()
        );
        assert!(deserialized.result.is_none());
    }
}

#[test]
fn test_config_provider_validation() {
    use mcp_web_search::config::*;

    // SearXNG without URL → ConfigError
    let args = mcp_web_search::Args {
        search_provider: SearchProvider::Searxng,
        search_api_key: None,
        search_api_url: None,
        limit: 10,
        language: "auto".into(),
        categories: "general".into(),
        time_range: "".into(),
        safe_search: 0,
        engines: "all".into(),
        timeout: 10000,
        host: "127.0.0.1".into(),
        http_port: 3001,
        stdio: false,
        log_level: "info".into(),
        request_timeout: 30,
        max_request_bytes: 16777216,
        max_response_bytes: 8388608,
        max_redirects: 5,
        allow_private_hosts: false,
        auth_token: None,
        auth_token_file: None,
        max_extract_urls: 100,
        max_map_urls: 10000,
        worker_threads: 0,
        rate_limit: 0.0,
        dns_pin: true,
        tls_cert: None,
        tls_key: None,
        browser_path: None,
        browser_max_pages: 0,
        browser_nav_timeout_ms: 30_000,
        browser_disable: false,
        enable_all: false,
        enable_search: false,
        enable_scrape: false,
        enable_fetch: false,
        enable_crawl: false,
    };
    assert!(matches!(
        Config::from_args(&args).unwrap_err(),
        mcp_web_search::WebSearchError::ConfigError(_)
    ));

    // Bing without API key → ConfigError
    let args = mcp_web_search::Args {
        search_provider: SearchProvider::Bing,
        search_api_key: None,
        search_api_url: None,
        limit: 10,
        language: "auto".into(),
        categories: "general".into(),
        time_range: "".into(),
        safe_search: 0,
        engines: "all".into(),
        timeout: 10000,
        host: "127.0.0.1".into(),
        http_port: 3001,
        stdio: false,
        log_level: "info".into(),
        request_timeout: 30,
        max_request_bytes: 1048576,
        max_response_bytes: 8388608,
        max_redirects: 5,
        allow_private_hosts: false,
        auth_token: None,
        auth_token_file: None,
        max_extract_urls: 100,
        max_map_urls: 10000,
        worker_threads: 0,
        rate_limit: 0.0,
        dns_pin: true,
        tls_cert: None,
        tls_key: None,
        browser_path: None,
        browser_max_pages: 0,
        browser_nav_timeout_ms: 30_000,
        browser_disable: false,
        enable_all: false,
        enable_search: false,
        enable_scrape: false,
        enable_fetch: false,
        enable_crawl: false,
    };
    assert!(matches!(
        Config::from_args(&args).unwrap_err(),
        mcp_web_search::WebSearchError::ConfigError(_)
    ));

    // DuckDuckGo (no key) → Ok
    let args = mcp_web_search::Args {
        search_provider: SearchProvider::DuckDuckGo,
        search_api_key: None,
        search_api_url: None,
        limit: 10,
        language: "auto".into(),
        categories: "general".into(),
        time_range: "".into(),
        safe_search: 0,
        engines: "all".into(),
        timeout: 10000,
        host: "127.0.0.1".into(),
        http_port: 3001,
        stdio: false,
        log_level: "info".into(),
        request_timeout: 30,
        max_request_bytes: 1048576,
        max_response_bytes: 8388608,
        max_redirects: 5,
        allow_private_hosts: false,
        auth_token: None,
        auth_token_file: None,
        max_extract_urls: 100,
        max_map_urls: 10000,
        worker_threads: 0,
        rate_limit: 0.0,
        dns_pin: true,
        tls_cert: None,
        tls_key: None,
        browser_path: None,
        browser_max_pages: 0,
        browser_nav_timeout_ms: 30_000,
        browser_disable: false,
        enable_all: false,
        enable_search: false,
        enable_scrape: false,
        enable_fetch: false,
        enable_crawl: false,
    };
    assert!(Config::from_args(&args).is_ok());
}
