//! MCP 2025-11-25 spec-compliance tests, driven through `process_request`.

use mcp_web_search::config::Config;
use mcp_web_search::protocol::JsonRpcRequest;
use mcp_web_search::server::process_request;
use serde_json::{json, Value};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn call(method: &str, params: Value) -> Result<Value, String> {
    use mcp_web_search::tools::ToolCategory;
    let mut config = Config::default();
    config.server.enabled_categories = ToolCategory::ALL.to_vec();
    config.tools_list =
        std::sync::Arc::new(mcp_web_search::tools::build_tools_list(ToolCategory::ALL));
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: Some(params),
        id: Some(json!(1)),
    };
    rt().block_on(process_request(&req, &config))
        .map_err(|e| e.to_string())
}

fn tool_call(name: &str, args: Value) -> Result<Value, String> {
    call("tools/call", json!({ "name": name, "arguments": args }))
}

#[test]
fn initialize_negotiates_all_supported_versions() {
    for v in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let res = call("initialize", json!({ "protocolVersion": v })).unwrap();
        assert_eq!(res["protocolVersion"], v, "{v} should be echoed");
    }
}

#[test]
fn initialize_falls_back_to_latest_and_has_instructions() {
    let res = call("initialize", json!({ "protocolVersion": "1900-01-01" })).unwrap();
    assert_eq!(res["protocolVersion"], "2025-11-25");
    assert!(res["instructions"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn initialize_capabilities_match_implementation() {
    let res = call("initialize", json!({})).unwrap();
    // tools and logging ARE implemented (logging/setLevel is handled).
    assert!(res["capabilities"]["tools"].is_object());
    assert!(res["capabilities"]["logging"].is_object());
    // resources/prompts are NOT implemented and must not be advertised.
    assert!(res["capabilities"]["resources"].is_null());
    assert!(res["capabilities"]["prompts"].is_null());
}

#[test]
fn logging_set_level_is_handled() {
    // The advertised `logging` capability must back a real method, not a -32601.
    let res = call("logging/setLevel", json!({ "level": "debug" }));
    assert!(res.is_ok(), "logging/setLevel should be handled: {res:?}");
    assert!(res.unwrap().is_object());
}

#[test]
fn tool_failure_is_iserror_not_protocol_error() {
    // Unknown format fails synchronously (offline) — must come back as an
    // isError CallToolResult, not a JSON-RPC protocol error.
    let res = tool_call(
        "web_scrape",
        json!({ "url": "https://example.com", "formats": ["definitely-not-a-format"] }),
    )
    .expect("execution failure should be Ok(CallToolResult), not Err");

    assert_eq!(res["isError"], true);
    let content = res["content"].as_array().expect("content array");
    assert!(!content.is_empty());
    assert_eq!(content[0]["type"], "text");
}

#[test]
fn protocol_errors_stay_protocol_errors() {
    // Missing `name`.
    assert!(call("tools/call", json!({})).is_err());
    // Unknown tool.
    assert!(tool_call("no_such_tool", json!({})).is_err());
    // Unknown method.
    assert!(call("no/such/method", json!({})).is_err());
}
