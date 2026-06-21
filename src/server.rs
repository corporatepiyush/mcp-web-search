use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

use crate::actions;
use crate::config::Config;
use crate::errors::{Result as WsResult, WebSearchError};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::ratelimit::TokenBucket;
use crate::tools;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::sync::Semaphore;

const BUFFER_CAPACITY: usize = 4096;
const NEWLINE: &[u8] = b"\n";
const INITIAL_LINE_CAP: usize = 1024;
const INITIAL_RESP_BUF: usize = 65536;

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Line,
    Eof,
    TooLong,
}

/// Read one line from a buffered reader, capping at `max` bytes.
/// Reuses `buf` and `out` across calls to avoid per-line allocations.
///
/// Unlike `read_until`, this consumes the underlying stream incrementally and
/// aborts the moment the accumulated bytes would exceed `max`. A malicious peer
/// sending a very long line with no newline therefore cannot force the process
/// to buffer the whole thing in memory — peak growth is bounded by `max` plus
/// one fill-buffer chunk.
async fn read_line_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    out: &mut String,
    max: usize,
) -> std::io::Result<LineRead>
where
    R: AsyncBufReadExt + Unpin,
{
    buf.clear();
    out.clear();
    loop {
        let available = match reader.fill_buf().await {
            Ok(b) => b,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // EOF. A trailing chunk with no newline is still a complete line.
            if buf.is_empty() {
                return Ok(LineRead::Eof);
            }
            break;
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                // Including the newline, would this line exceed the cap?
                if buf.len() + idx + 1 > max {
                    reader.consume(idx + 1);
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(&available[..=idx]);
                reader.consume(idx + 1);
                break;
            }
            None => {
                let take = available.len();
                // No newline yet: refuse to grow past the cap rather than
                // buffering an unbounded line into memory.
                if buf.len() + take > max {
                    reader.consume(take);
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(available);
                reader.consume(take);
            }
        }
    }
    *out = String::from_utf8_lossy(buf).into_owned();
    Ok(LineRead::Line)
}

/// Read a line subject to both the byte cap and an idle timeout. Returns
/// `Ok(None)` if no complete line arrives within `idle` — used to drop
/// slow/stalled TCP peers (slowloris) that would otherwise hold a connection
/// slot open indefinitely without ever sending a full request.
async fn read_line_capped_timed<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    out: &mut String,
    max: usize,
    idle: std::time::Duration,
) -> std::io::Result<Option<LineRead>>
where
    R: AsyncBufReadExt + Unpin,
{
    match tokio::time::timeout(idle, read_line_capped(reader, buf, out, max)).await {
        Ok(res) => res.map(Some),
        Err(_) => Ok(None),
    }
}

/// Constant-time bearer-token check.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    let presented = presented.trim();
    let presented = presented
        .strip_prefix("Bearer ")
        .unwrap_or(presented)
        .trim();
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn parse_error(msg: String) -> JsonRpcResponse {
    let e = WebSearchError::ParseError(msg);
    JsonRpcResponse::error(None, e.error_code(), e.to_string())
}

fn parse_request(line: &str) -> std::result::Result<JsonRpcRequest, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("Empty request".to_string());
    }
    serde_json::from_str::<JsonRpcRequest>(trimmed).map_err(|e| e.to_string())
}

pub struct MCPServer {
    config: Arc<Config>,
}

impl MCPServer {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    #[must_use]
    pub const fn from_arc(config: Arc<Config>) -> Self {
        Self { config }
    }

    pub async fn run_stdio(&self) -> WsResult<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::with_capacity(BUFFER_CAPACITY, stdin);
        let mut stdout = tokio::io::stdout();
        let mut line = String::with_capacity(INITIAL_LINE_CAP);
        let mut read_buf = Vec::with_capacity(INITIAL_LINE_CAP);
        let mut response_buf = Vec::with_capacity(INITIAL_RESP_BUF);
        let max = self.config.server.max_request_bytes;

        loop {
            match read_line_capped(&mut reader, &mut read_buf, &mut line, max).await {
                Ok(LineRead::Eof) => break,
                Ok(LineRead::Line) => {
                    process_one_line(&line, &self.config, &None, &mut response_buf, &mut stdout)
                        .await?;
                }
                Ok(LineRead::TooLong) => {
                    write_oversize_error(&mut response_buf, &mut stdout, max).await?;
                    break;
                }
                Err(e) => {
                    error!("IO error on stdio: {e}");
                    break;
                }
            }
        }
        Ok(())
    }

    pub async fn run(&self) -> WsResult<()> {
        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);
        let listener = Arc::new(
            TcpListener::bind(&addr)
                .await
                .map_err(|e| WebSearchError::ConfigError(format!("bind {addr} failed: {e}")))?,
        );
        info!("MCP web-search TCP server listening on {addr}");

        let conn_limiter = Arc::new(Semaphore::new(self.config.server.max_connections));
        let rate_limiter = if self.config.server.rate_limit > 0.0 {
            Some(Arc::new(TokenBucket::new(self.config.server.rate_limit)))
        } else {
            None
        };
        let accept_shards = num_cpus::get().clamp(1, 8);

        info!(
            accept_shards,
            max_connections = self.config.server.max_connections,
            rate_limit = self.config.server.rate_limit,
            "Starting TCP accept tasks"
        );

        let mut handles = Vec::with_capacity(accept_shards);
        for _ in 0..accept_shards {
            let listener = Arc::clone(&listener);
            let conn_limiter = Arc::clone(&conn_limiter);
            let rate_limiter = rate_limiter.as_ref().map(Arc::clone);
            let config = Arc::clone(&self.config);
            handles.push(tokio::spawn(async move {
                accept_loop(listener, conn_limiter, rate_limiter, config).await;
            }));
        }

        // Wait for any accept task to exit (should not happen in normal operation)
        for h in handles {
            h.await.ok();
        }

        Ok(())
    }
}

/// Per-shard accept loop. Multiple copies of this function run concurrently,
/// each accepting connections from the same shared `TcpListener`. Tokio's IO
/// driver distributes incoming connections across the waiting tasks, providing
/// near-kernel-level accept scaling without `SO_REUSEPORT`.
async fn accept_loop(
    listener: Arc<TcpListener>,
    conn_limiter: Arc<Semaphore>,
    rate_limiter: Option<Arc<TokenBucket>>,
    config: Arc<Config>,
) {
    loop {
        let permit = match Arc::clone(&conn_limiter).acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                error!("Connection semaphore closed, exiting accept loop");
                return;
            }
        };
        let (socket, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Accept failed: {e}");
                continue;
            }
        };
        if let Err(e) = socket.set_nodelay(true) {
            warn!("Failed to set TCP_NODELAY: {e}");
        }
        let config = Arc::clone(&config);
        let rate_limiter = rate_limiter.as_ref().map(Arc::clone);
        tokio::spawn(async move {
            if let Err(e) = handle_client(socket, config, rate_limiter).await {
                error!("Client {peer_addr} error: {e}");
            }
            drop(permit);
        });
    }
}

async fn handle_client(
    socket: TcpStream,
    config: Arc<Config>,
    rate_limiter: Option<Arc<TokenBucket>>,
) -> WsResult<()> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::with_capacity(BUFFER_CAPACITY, reader);
    let mut line = String::with_capacity(INITIAL_LINE_CAP);
    let mut read_buf = Vec::with_capacity(INITIAL_LINE_CAP);
    let mut response_buf = Vec::with_capacity(INITIAL_RESP_BUF);
    let max = config.server.max_request_bytes;
    // Idle/read timeout: a peer that opens a connection but never sends a
    // complete line is dropped instead of holding the slot forever (slowloris).
    let idle = config.server.request_timeout;

    if let Some(ref expected) = config.server.auth_token {
        match read_line_capped_timed(&mut reader, &mut read_buf, &mut line, max, idle).await {
            Ok(Some(LineRead::Line)) if token_matches(&line, expected) => {
                info!("Client authenticated successfully");
            }
            Ok(Some(LineRead::Eof)) => {
                warn!("Client disconnected before sending auth token");
                return Ok(());
            }
            Ok(None) => {
                warn!("Client idle timeout before sending auth token");
                return Ok(());
            }
            _ => {
                warn!("Authentication failed (invalid token)");
                let err = WebSearchError::InvalidParams(
                    "Authentication required: send the bearer token as the first line".into(),
                );
                let response =
                    JsonRpcResponse::error(None, err.error_code(), err.to_string());
                response_buf.clear();
                serde_json::to_writer(&mut response_buf, &response)?;
                response_buf.extend_from_slice(NEWLINE);
                writer.write_all(&response_buf).await.ok();
                writer.flush().await.ok();
                return Ok(());
            }
        }
    }

    loop {
        match read_line_capped_timed(&mut reader, &mut read_buf, &mut line, max, idle).await {
            Ok(Some(LineRead::Eof)) => break,
            Ok(Some(LineRead::Line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                process_one_line(&line, &config, &rate_limiter, &mut response_buf, &mut writer).await?;
            }
            Ok(Some(LineRead::TooLong)) => {
                write_oversize_error(&mut response_buf, &mut writer, max).await?;
                break;
            }
            Ok(None) => {
                warn!("Client idle timeout; closing connection");
                break;
            }
            Err(e) => {
                error!("IO error reading from client: {e}");
                break;
            }
        }
    }
    Ok(())
}

async fn write_oversize_error<W: AsyncWriteExt + Unpin>(
    response_buf: &mut Vec<u8>,
    writer: &mut W,
    max: usize,
) -> WsResult<()> {
    let err = WebSearchError::InvalidParams(format!(
        "Request exceeds maximum size of {max} bytes"
    ));
    let response = JsonRpcResponse::error(None, err.error_code(), err.to_string());
    response_buf.clear();
    serde_json::to_writer(&mut *response_buf, &response)?;
    response_buf.extend_from_slice(NEWLINE);
    writer.write_all(response_buf).await.ok();
    writer.flush().await.ok();
    Ok(())
}

async fn process_one_line<W: AsyncWriteExt + Unpin>(
    line: &str,
    config: &Arc<Config>,
    rate_limiter: &Option<Arc<TokenBucket>>,
    response_buf: &mut Vec<u8>,
    writer: &mut W,
) -> WsResult<()> {
    if let Some(limiter) = rate_limiter {
        if !limiter.try_acquire() {
            let err = WebSearchError::RateLimited("Rate limit exceeded. Try again later.".into());
            let response = JsonRpcResponse::error_with_data(
                None,
                err.error_code(),
                err.to_string(),
                err.error_data().unwrap_or_default(),
            );
            response_buf.clear();
            serde_json::to_writer(&mut *response_buf, &response)?;
            response_buf.extend_from_slice(NEWLINE);
            writer.write_all(response_buf).await.ok();
            writer.flush().await.ok();
            return Ok(());
        }
    }
    let (response, is_notification) = match parse_request(line) {
        Ok(req) => {
            let is_notif = req.id.is_none();
            match tokio::time::timeout(
                config.server.request_timeout,
                process_request(&req, config),
            )
            .await
            {
                Ok(Ok(result)) => (JsonRpcResponse::success(req.id, result), is_notif),
                Ok(Err(ref e)) => {
                    let data = e.error_data();
                    let resp = if let Some(data) = data {
                        JsonRpcResponse::error_with_data(
                            req.id,
                            e.error_code(),
                            e.to_string(),
                            data,
                        )
                    } else {
                        JsonRpcResponse::error(req.id, e.error_code(), e.to_string())
                    };
                    (resp, is_notif)
                }
                Err(_) => (
                    timeout_response(&req, config.server.request_timeout.as_secs()),
                    is_notif,
                ),
            }
        }
        Err(e) => (parse_error(e), false),
    };

    if is_notification {
        return Ok(());
    }

    response_buf.clear();
    serde_json::to_writer(&mut *response_buf, &response)?;
    response_buf.extend_from_slice(NEWLINE);
    writer.write_all(response_buf).await.ok();
    writer.flush().await.ok();
    Ok(())
}

pub async fn process_request(req: &JsonRpcRequest, config: &Config) -> WsResult<Value> {
    match req.method.as_str() {
        "initialize" => handle_initialize(req),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(req, config).await,
        // Accept and acknowledge the client's desired log level. We advertise the
        // `logging` capability; this makes the method real rather than a 404.
        "logging/setLevel" => Ok(json!({})),
        "ping" => Ok(Value::Null),
        method if method.starts_with("notifications/") => {
            tracing::trace!("notification: {method}");
            Ok(Value::Null)
        }
        _ => Err(WebSearchError::MethodNotFound(req.method.clone())),
    }
}

pub async fn process_request_http(req: &JsonRpcRequest, config: &Config) -> JsonRpcResponse {
    match tokio::time::timeout(config.server.request_timeout, process_request(req, config)).await {
        Ok(Ok(result)) => JsonRpcResponse::success(req.id.clone(), result),
        Ok(Err(ref e)) => {
            let data = e.error_data();
            if let Some(data) = data {
                JsonRpcResponse::error_with_data(
                    req.id.clone(),
                    e.error_code(),
                    e.to_string(),
                    data,
                )
            } else {
                JsonRpcResponse::error(req.id.clone(), e.error_code(), e.to_string())
            }
        }
        Err(_) => timeout_response(req, config.server.request_timeout.as_secs()),
    }
}

/// MCP protocol revisions this server can speak, newest first (for `initialize`
/// version negotiation).
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Newest revision we implement; offered when the client requests an unknown one.
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// `instructions` surfaced to the client and appended to the model's system prompt.
const SERVER_INSTRUCTIONS: &str = "Web search and fetch MCP server. Use `web_search` to find pages \
(results are ranked by relevance) and `web_scrape`/`web_fetch` for full page content as markdown. \
These tools reach the live internet. Tool failures (provider errors, timeouts, HTTP 429) are returned \
with `isError: true` rather than as protocol errors — read the message and, for rate limits, back off \
and retry.";

fn handle_initialize(req: &JsonRpcRequest) -> WsResult<Value> {
    // Version negotiation: echo a supported requested revision, else offer latest.
    let protocol_version = req
        .params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    Ok(json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false },
            "logging": {}
        },
        "serverInfo": {
            "name": "mcp-web-search",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": SERVER_INSTRUCTIONS
    }))
}

/// Wrap a tool execution failure as an MCP `CallToolResult` with `isError: true`
/// so the model sees the message and can self-correct (e.g. back off on HTTP
/// 429), instead of receiving an opaque JSON-RPC protocol error. Successful
/// results are already content-wrapped by the action handlers.
#[inline]
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

/// Build the response for a request that exceeded the server timeout. For
/// `tools/call` this is a `CallToolResult` with `isError: true` so the model can
/// read it and back off — consistent with how tool-internal timeouts are
/// surfaced. Other methods get a JSON-RPC timeout error.
fn timeout_response(req: &JsonRpcRequest, timeout_secs: u64) -> JsonRpcResponse {
    let msg = format!("request timed out after {timeout_secs}s");
    if req.method == "tools/call" {
        JsonRpcResponse::success(req.id.clone(), tool_error(&msg))
    } else {
        JsonRpcResponse::error(
            req.id.clone(),
            WebSearchError::Timeout(String::new()).error_code(),
            msg,
        )
    }
}

fn handle_tools_list() -> WsResult<Value> {
    Ok(tools::tools_list_response().clone())
}

async fn handle_tools_call(req: &JsonRpcRequest, config: &Config) -> WsResult<Value> {
    let tool_name = req
        .params
        .as_ref()
        .and_then(|p| p.get("name").and_then(|v| v.as_str()))
        .ok_or_else(|| WebSearchError::InvalidParams("Missing 'name' parameter".into()))?;

    let tool_args = req.params.as_ref().and_then(|p| p.get("arguments"));

    if !tools::tool_exists(tool_name) {
        return Err(WebSearchError::MethodNotFound(tool_name.to_string()));
    }

    let result = match tool_name {
        "web_search" => actions::search::web_search(tool_args, config).await,
        "web_scrape" => actions::scrape::web_scrape(tool_args, config).await,
        "web_map" => actions::map::web_map(tool_args, config).await,
        "web_extract" => actions::extract::web_extract(tool_args, config).await,
        "web_fetch" => actions::scrape::web_fetch(tool_args, config).await,
        "web_fetch_text" => actions::scrape::web_fetch_text(tool_args, config).await,
        "web_fetch_headers" => actions::fetch::web_fetch_headers(tool_args, config).await,
        "web_search_scrape" => actions::search::web_search_scrape(tool_args, config).await,
        "web_sitemap" => actions::fetch::web_sitemap(tool_args, config).await,
        "web_check_links" => actions::fetch::web_check_links(tool_args, config).await,
        "browser_scrape" => actions::browser::browser_scrape(tool_args, config).await,
        "browser_screenshot" => actions::browser::browser_screenshot(tool_args, config).await,
        other => Err(WebSearchError::MethodNotFound(other.to_string())),
    };

    // Execution failures become isError CallToolResults so the model can read
    // the message and self-correct; successes (already content-wrapped) get an
    // explicit isError:false.
    match result {
        Ok(mut value) => {
            if value.get("content").is_some() && value.get("isError").is_none() {
                value["isError"] = Value::Bool(false);
            }
            Ok(value)
        }
        Err(e) => {
            error!("Tool '{tool_name}' error: {e}");
            Ok(tool_error(&e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_valid_request() {
        let line = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;
        assert_eq!(parse_request(line).unwrap().method, "initialize");
    }

    #[test]
    fn test_parse_request_with_params() {
        let line = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"web_search","arguments":{"query":"test"}},"id":2}"#;
        let req = parse_request(line).unwrap();
        assert_eq!(req.method, "tools/call");
        assert!(req.params.is_some());
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_request("{bad}").is_err());
    }

    #[test]
    fn test_parse_empty() {
        assert!(parse_request("").is_err());
        assert!(parse_request("   ").is_err());
    }

    #[test]
    fn test_parse_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req = parse_request(line).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn test_token_matches() {
        assert!(token_matches("secret", "secret"));
        assert!(token_matches("Bearer secret", "secret"));
        assert!(token_matches("  Bearer secret  ", "secret"));
        assert!(!token_matches("wrong", "secret"));
        assert!(!token_matches("", "secret"));
        assert!(!token_matches("Bearer wrong", "secret"));
    }

    #[test]
    fn test_initialize_response() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "initialize".to_string(),
            params: None,
            id: Some(json!(1)),
        };
        let v = handle_initialize(&req).unwrap();
        assert_eq!(v["protocolVersion"], LATEST_PROTOCOL_VERSION);
        assert_eq!(v["serverInfo"]["name"], "mcp-web-search");
        assert_eq!(v["capabilities"]["tools"]["listChanged"], false);
        assert!(v["instructions"].is_string());
    }

    #[test]
    fn test_initialize_version_negotiation() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "initialize".to_string(),
            params: Some(json!({ "protocolVersion": "2025-03-26" })),
            id: Some(json!(1)),
        };
        assert_eq!(
            handle_initialize(&req).unwrap()["protocolVersion"],
            "2025-03-26"
        );
    }

    #[test]
    fn test_tool_error_is_iserror_result() {
        // Unknown tool inside the dispatch becomes an isError result, not an Err.
        let v = tool_error("provider returned 429");
        assert_eq!(v["isError"], true);
        assert_eq!(v["content"][0]["text"], "provider returned 429");
    }

    #[test]
    fn test_tools_list_response() {
        let v = handle_tools_list().unwrap();
        assert!(v.get("tools").is_some());
        let tools = v["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
    }

    #[test]
    fn test_process_request_unknown_method() {
        let config = Config::default();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "unknown_method".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(process_request(&req, &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::MethodNotFound(_)
        ));
    }

    #[test]
    fn test_process_request_ping() {
        let config = Config::default();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(process_request(&req, &config));
        assert_eq!(result.unwrap(), Value::Null);
    }

    #[test]
    fn test_process_request_notification() {
        let config = Config::default();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "notifications/initialized".into(),
            params: None,
            id: None,
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(process_request(&req, &config));
        assert_eq!(result.unwrap(), Value::Null);
    }

    #[test]
    fn test_process_request_tools_call_no_name() {
        let config = Config::default();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "tools/call".into(),
            params: Some(json!({})),
            id: Some(Value::Number(1.into())),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(process_request(&req, &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::InvalidParams(_)
        ));
    }

    #[test]
    fn test_process_request_tools_call_unknown_tool() {
        let config = Config::default();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "tools/call".into(),
            params: Some(json!({"name": "nonexistent"})),
            id: Some(Value::Number(1.into())),
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(process_request(&req, &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::MethodNotFound(_)
        ));
    }

    #[tokio::test]
    async fn test_read_line_capped_normal() {
        let data = b"hello\nworld\n";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::Line);
        assert_eq!(line, "hello\n");
        let _ = read_line_capped(&mut reader, &mut buf, &mut line, 100).await.unwrap();
        assert_eq!(line, "world\n");
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::Eof);
    }

    #[tokio::test]
    async fn test_read_line_capped_oversize() {
        let data = vec![b'a'; 1024];
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::TooLong);
    }

    // EXPLOIT REGRESSION (#1): a newline-less payload far larger than `max`
    // must be rejected as TooLong *without* ever buffering the whole thing.
    // The old `read_until` implementation would allocate all of `data` before
    // checking the cap. Here `buf` must never grow past `max`.
    #[tokio::test]
    async fn test_read_line_capped_unbounded_line_does_not_buffer_everything() {
        // 16 MiB with no newline, cap of 64 bytes.
        let data = vec![b'a'; 16 * 1024 * 1024];
        let mut reader = tokio::io::BufReader::with_capacity(4096, &data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 64).await.unwrap();
        assert_eq!(res, LineRead::TooLong);
        // Bounded memory: we never accumulated more than the cap (plus headroom
        // for a single fill chunk; with a 4096 buffer it stays tiny).
        assert!(
            buf.len() <= 64,
            "buffer grew to {} bytes — cap was not enforced incrementally",
            buf.len()
        );
    }

    // A line whose newline sits just past the cap is TooLong (boundary check).
    #[tokio::test]
    async fn test_read_line_capped_newline_past_cap() {
        let data = b"aaaaaa\n"; // 6 'a' + newline = 7 bytes
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 6).await.unwrap();
        assert_eq!(res, LineRead::TooLong);
    }

    // Reads spanning multiple fill_buf chunks reassemble correctly.
    #[tokio::test]
    async fn test_read_line_capped_multi_chunk() {
        let data = b"hello world this is one line\n";
        // Tiny BufReader forces several fill_buf/consume cycles per line.
        let mut reader = tokio::io::BufReader::with_capacity(4, &data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 1024).await.unwrap();
        assert_eq!(res, LineRead::Line);
        assert_eq!(line, "hello world this is one line\n");
    }

    // EXPLOIT REGRESSION (#4 slowloris): a peer that connects and never sends a
    // complete line must be dropped via the idle timeout, not held forever.
    #[tokio::test]
    async fn test_idle_timeout_on_silent_peer() {
        let (_client, server) = tokio::io::duplex(64);
        let mut reader = tokio::io::BufReader::new(server);
        let mut buf = Vec::new();
        let mut line = String::new();
        // _client stays open but sends nothing → fill_buf pends → timeout fires.
        let res = read_line_capped_timed(
            &mut reader,
            &mut buf,
            &mut line,
            1024,
            std::time::Duration::from_millis(80),
        )
        .await
        .unwrap();
        assert!(res.is_none(), "expected idle timeout (None), got {res:?}");
    }

    // Slowloris variant: a partial line that stalls before its newline is also
    // dropped rather than holding the connection slot indefinitely.
    #[tokio::test]
    async fn test_idle_timeout_on_partial_then_stall() {
        use tokio::io::AsyncWriteExt;
        let (mut client, server) = tokio::io::duplex(64);
        let mut reader = tokio::io::BufReader::new(server);
        let mut buf = Vec::new();
        let mut line = String::new();

        // Send a partial line (no newline) then keep the connection open without
        // sending more. `client` stays in scope, so the reader sees a stall —
        // not EOF — and the idle timeout must fire.
        client.write_all(b"abc").await.unwrap();
        let res = read_line_capped_timed(
            &mut reader,
            &mut buf,
            &mut line,
            1024,
            std::time::Duration::from_millis(80),
        )
        .await
        .unwrap();
        assert!(res.is_none(), "partial-then-stall should time out, got {res:?}");
        drop(client);
    }

    #[tokio::test]
    async fn test_read_line_capped_exact_fit() {
        let data = b"hello\n";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 6).await.unwrap();
        assert_eq!(res, LineRead::Line);
        assert_eq!(line, "hello\n");
    }

    #[tokio::test]
    async fn test_read_line_capped_empty() {
        let data = b"";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut buf, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::Eof);
    }

    #[test]
    fn test_parse_error_response() {
        let resp = parse_error("bad json".into());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    #[test]
    fn test_timeout_error_code() {
        let err = WebSearchError::Timeout("request timed out".into());
        assert_eq!(err.error_code(), -32005);
    }

    // REGRESSION (#8): a tools/call that hits the request-level timeout is a
    // CallToolResult with isError:true (model-readable), not a protocol error.
    #[test]
    fn test_timeout_response_tools_call_is_iserror() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "tools/call".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let resp = timeout_response(&req, 30);
        assert!(resp.error.is_none(), "should not be a protocol error");
        let result = resp.result.expect("result present");
        assert_eq!(result["isError"], Value::Bool(true));
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
    }

    // Non-tool methods keep the JSON-RPC protocol timeout error.
    #[test]
    fn test_timeout_response_non_tool_is_protocol_error() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: None,
            id: Some(Value::Number(1.into())),
        };
        let resp = timeout_response(&req, 30);
        assert!(resp.result.is_none());
        assert_eq!(
            resp.error.unwrap().code,
            WebSearchError::Timeout(String::new()).error_code()
        );
    }
}
