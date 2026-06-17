use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

use crate::actions;
use crate::config::Config;
use crate::errors::{Result as WsResult, WebSearchError};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
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
/// Uses `read_until` for a single-copy path: the data moves directly from
/// the BufReader's internal buffer into the line buffer without the
/// fill_buf → scan → extend dance of the previous implementation.
async fn read_line_capped<R>(reader: &mut R, out: &mut String, max: usize) -> std::io::Result<LineRead>
where
    R: AsyncBufReadExt + Unpin,
{
    out.clear();
    let mut buf: Vec<u8> = Vec::with_capacity(INITIAL_LINE_CAP);
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if buf.len() > max {
        return Ok(LineRead::TooLong);
    }
    *out = String::from_utf8_lossy(&buf).into_owned();
    Ok(LineRead::Line)
}

/// Constant-time bearer-token check.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    let presented = presented.trim();
    let presented = presented
        .strip_prefix("Bearer ")
        .unwrap_or(presented)
        .trim();
    let h_presented = Sha256::digest(presented.as_bytes());
    let h_expected = Sha256::digest(expected.as_bytes());
    h_presented.ct_eq(&h_expected).into()
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
        let mut response_buf = Vec::with_capacity(INITIAL_RESP_BUF);
        let max = self.config.server.max_request_bytes;

        loop {
            match read_line_capped(&mut reader, &mut line, max).await {
                Ok(LineRead::Eof) => break,
                Ok(LineRead::Line) => {
                    process_one_line(&line, &self.config, &mut response_buf, &mut stdout)
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

        let limiter = Arc::new(Semaphore::new(self.config.server.max_connections));
        let accept_shards = num_cpus::get().clamp(1, 8);

        info!(
            accept_shards,
            max_connections = self.config.server.max_connections,
            "Starting TCP accept tasks"
        );

        let mut handles = Vec::with_capacity(accept_shards);
        for _ in 0..accept_shards {
            let listener = Arc::clone(&listener);
            let limiter = Arc::clone(&limiter);
            let config = Arc::clone(&self.config);
            handles.push(tokio::spawn(async move {
                accept_loop(listener, limiter, config).await;
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
    limiter: Arc<Semaphore>,
    config: Arc<Config>,
) {
    loop {
        let permit = match Arc::clone(&limiter).acquire_owned().await {
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
        tokio::spawn(async move {
            if let Err(e) = handle_client(socket, config).await {
                error!("Client {peer_addr} error: {e}");
            }
            drop(permit);
        });
    }
}

async fn handle_client(socket: TcpStream, config: Arc<Config>) -> WsResult<()> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::with_capacity(BUFFER_CAPACITY, reader);
    let mut line = String::with_capacity(INITIAL_LINE_CAP);
    let mut response_buf = Vec::with_capacity(INITIAL_RESP_BUF);
    let max = config.server.max_request_bytes;

    if let Some(ref expected) = config.server.auth_token {
        match read_line_capped(&mut reader, &mut line, max).await {
            Ok(LineRead::Line) if token_matches(&line, expected) => {
                info!("Client authenticated successfully");
            }
            Ok(LineRead::Eof) => {
                warn!("Client disconnected before sending auth token");
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
        match read_line_capped(&mut reader, &mut line, max).await {
            Ok(LineRead::Eof) => break,
            Ok(LineRead::Line) => {
                if line.trim().is_empty() {
                    continue;
                }
                process_one_line(&line, &config, &mut response_buf, &mut writer).await?;
            }
            Ok(LineRead::TooLong) => {
                write_oversize_error(&mut response_buf, &mut writer, max).await?;
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
    response_buf: &mut Vec<u8>,
    writer: &mut W,
) -> WsResult<()> {
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
                Err(_) => {
                    let err_msg = format!(
                        "request timed out after {}s",
                        config.server.request_timeout.as_secs()
                    );
                    (
                        JsonRpcResponse::error(
                            req.id,
                            WebSearchError::Timeout(String::new()).error_code(),
                            err_msg,
                        ),
                        is_notif,
                    )
                }
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
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(req, config).await,
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
        Err(_) => JsonRpcResponse::error(
            req.id.clone(),
            WebSearchError::Timeout(String::new()).error_code(),
            format!(
                "request timed out after {}s",
                config.server.request_timeout.as_secs()
            ),
        ),
    }
}

fn handle_initialize() -> WsResult<Value> {
    Ok(json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false },
            "logging": {}
        },
        "serverInfo": {
            "name": "mcp-web-search",
            "version": env!("CARGO_PKG_VERSION")
        }
    }))
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
        other => Err(WebSearchError::MethodNotFound(other.to_string())),
    };

    if let Err(ref e) = result {
        error!("Tool '{tool_name}' error: {e}");
    }
    result
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
        let v = handle_initialize().unwrap();
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(v["serverInfo"]["name"], "mcp-web-search");
        assert_eq!(v["capabilities"]["tools"]["listChanged"], false);
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
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::Line);
        assert_eq!(line, "hello\n");
        let _ = read_line_capped(&mut reader, &mut line, 100).await.unwrap();
        assert_eq!(line, "world\n");
        let res = read_line_capped(&mut reader, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::Eof);
    }

    #[tokio::test]
    async fn test_read_line_capped_oversize() {
        let data = vec![b'a'; 1024];
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut line, 100).await.unwrap();
        assert_eq!(res, LineRead::TooLong);
    }

    #[tokio::test]
    async fn test_read_line_capped_exact_fit() {
        let data = b"hello\n";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut line, 6).await.unwrap();
        assert_eq!(res, LineRead::Line);
        assert_eq!(line, "hello\n");
    }

    #[tokio::test]
    async fn test_read_line_capped_empty() {
        let data = b"";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let res = read_line_capped(&mut reader, &mut line, 100).await.unwrap();
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
}
