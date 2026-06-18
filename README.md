# mcp-web-search

High-performance, SSRF-hardened MCP server for web search, scraping, URL discovery, and multi-URL content extraction — a secure Rust alternative to `one-search-mcp`.

> **MCP suite.** One of four high-performance MCP servers written in Rust —
> [mcp-postgres](https://github.com/corporatepiyush/mcp-pg-rust) ·
> [mcp-filesystem](https://github.com/corporatepiyush/mcp-filesystem-rust) ·
> [mcp-memory](https://github.com/corporatepiyush/mcp-memory) ·
> [mcp-web-search](https://github.com/corporatepiyush/mcp-web-search).
> All implement MCP protocol revision **`2025-11-25`**.

## Features

- **Web Search** — multi-provider search (DuckDuckGo, SearXNG, Bing, Tavily, Google, Zhipu, Exa, Bocha)
- **Web Scrape** — fetch and clean page content with optional main-content extraction
- **Web Map** — site URL discovery via sitemap.xml + in-page link crawling
- **Web Extract** — mass parallel scrape of 100s of URLs with bounded concurrency
- **SSRF Guard** — blocks private/meta/link-local addresses by default, with DNS-rebinding protection (pinned connections) on every outbound request path
- **Constant-time auth** — bearer token compared with `subtle` (constant-time), loadable from a file via `--auth-token-file` (fails closed if the file is missing or empty)
- **CPU-core scaling** — auto-detects core count and scales thread pool, connection limits, and concurrency
- **Async DNS** — non-blocking DNS resolution via tokio's async resolver

## Usage

```bash
# Run in stdio mode (for MCP clients)
mcp-web-search --stdio

# Run as TCP server
mcp-web-search --host 127.0.0.1 --port 3000

# Run as HTTP server
mcp-web-search --http-port 3001

# With auth
mcp-web-search --auth-token "my-secret-token"
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SEARCH_PROVIDER` | `duckduckgo` | Search backend |
| `SEARCH_API_KEY` | — | API key for providers that require one |
| `SEARCH_API_URL` | — | API URL (SearXNG, Google) |
| `AUTH_TOKEN` | — | Bearer token for authentication |

## MCP Compliance

Implements the [Model Context Protocol](https://modelcontextprotocol.io) revision **`2025-11-25`** over JSON-RPC 2.0, via stdio, TCP, or HTTP.

| Area | Support |
|---|---|
| Transports | stdio, TCP, HTTP (`POST /rpc`) |
| Protocol version | `2025-11-25`, negotiates down to `2025-06-18` / `2025-03-26` / `2024-11-05` |
| `initialize` | ✅ version negotiation + `instructions` |
| `tools/list`, `tools/call` | ✅ (10 tools) |
| `CallToolResult` | ✅ `content[]` + `isError` |
| `logging/setLevel` | ✅ accepted (level acknowledged) |
| Auth | ✅ optional bearer token (constant-time, `--auth-token` / `--auth-token-file`) |
| Capabilities advertised | `tools`, `logging` |
| `resources` · `prompts` · Streamable HTTP | ❌ roadmap — see [MIGRATION.md](./MIGRATION.md) |

Tool failures (provider errors, timeouts, HTTP 429) are returned as
`CallToolResult`s with `isError: true` (not as JSON-RPC protocol errors) so the
model can back off and retry. Upgrading from 1.x? See **[MIGRATION.md](./MIGRATION.md)**.

## Versioning & Compatibility

Follows [Semantic Versioning](https://semver.org). The current line is **2.x**,
targeting MCP revision `2025-11-25`. The `2.0.0` release changed tool failures
from JSON-RPC protocol errors to `isError` results — see **[MIGRATION.md](./MIGRATION.md)**
and the [CHANGELOG](./CHANGELOG.md).

| mcp-web-search | MCP revision (default) | Negotiates |
|---|---|---|
| 2.x | `2025-11-25` | `2025-06-18`, `2025-03-26`, `2024-11-05` |
| ≤ 1.x | `2024-11-05` | — |

## License

Apache-2.0
