# mcp-web-search

High-performance, SSRF-hardened MCP server for web search, scraping, URL discovery, and multi-URL content extraction — a secure Rust alternative to `one-search-mcp`.

> **Tools are opt-in (2.3.0+).** No tools are exposed by default. Enable them one
> *category* at a time with `--enable-<category>` flags (or `--enable-all`). See
> [Tool Exposure](#tool-exposure-opt-in-by-category).

> **TCP removed (2.3.0+).** The line-delimited TCP transport has been dropped;
> use **stdio** (for MCP clients) or **HTTP**.

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
- **Headless Browser** — `browser_scrape` and `browser_screenshot` tools that run real Chrome/Chromium (CDP), executing JavaScript for SPAs and lazy-loaded content
- **SSRF Guard** — blocks private/meta/link-local addresses by default, with DNS-rebinding protection (pinned connections) on every outbound request path
- **Constant-time auth** — bearer token compared with `subtle` (constant-time), loadable from a file via `--auth-token-file` (fails closed if the file is missing or empty)
- **CPU-core scaling** — auto-detects core count and scales thread pool, connection limits, and concurrency
- **Async DNS** — non-blocking DNS resolution via tokio's async resolver

## Usage

Tools are opt-in — pass one or more `--enable-<category>` flags (or
`--enable-all`). Without them the server exposes no tools.

```bash
# Run in stdio mode (for MCP clients) — search + fetch tools
mcp-web-search --stdio --enable-search --enable-fetch

# Run as HTTP server with every tool
mcp-web-search --http-port 3001 --enable-all

# With auth
mcp-web-search --http-port 3001 --enable-all --auth-token "my-secret-token"

# Serve the HTTP transport over TLS (HTTPS)
mcp-web-search --http-port 3001 --enable-all --tls-cert ./cert.pem --tls-key ./key.pem

# Explicit Chrome binary path (headless browser tools are in the `scrape` category)
mcp-web-search --stdio --enable-scrape --browser-path /usr/bin/chromium

# Disable headless browser tools (browser_scrape / browser_screenshot return errors)
mcp-web-search --stdio --enable-scrape --browser-disable
```

### Tool Exposure (opt-in by category)

Every tool belongs to one of **4 categories**. **Nothing is exposed until you
enable its category** — disabled tools are hidden from `tools/list` and rejected
from `tools/call` as if they did not exist.

| Flag | Category | Tools |
|------|----------|-------|
| `--enable-search` | **Search** | `web_search`, `web_search_scrape` |
| `--enable-scrape` | **Scrape** | `web_scrape`, `web_extract`, `browser_scrape`, `browser_screenshot` |
| `--enable-fetch` | **Fetch** | `web_fetch`, `web_fetch_text`, `web_fetch_headers` |
| `--enable-crawl` | **Crawl** | `web_map`, `web_sitemap`, `web_check_links` |
| `--enable-all` | *(all)* | Every category. Overrides the individual flags. |

### Headless Browser

`browser_scrape` and `browser_screenshot` use Chrome/Chromium via the Chrome DevTools
Protocol (CDP). This enables JavaScript execution, SPA support, lazy-loaded content, and
visual screenshots.

**Requirements:** Chrome or Chromium must be installed. The binary is auto-detected from
PATH, or set explicitly with `--browser-path` / `BROWSER_PATH`.

**Resource bounds:** Each concurrent browser page uses roughly 50–200 MB RAM. Concurrency
is capped at `--browser-max-pages` (default: `2×num_cpus`, min 4). The browser process is
launched lazily on the first browser tool call and reused across requests.

**Security:** URLs are validated by the same SSRF guard as all other tools before being
passed to the browser. Only `http://` and `https://` URLs are accepted. The browser is
launched with `--block-new-web-contents` to prevent malicious pages from opening secondary
navigations to internal hosts. `--no-sandbox` is required in containerised environments
(standard practice for headless Chrome in Docker/Kubernetes).

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--browser-path` | `BROWSER_PATH` | auto | Path to Chrome/Chromium binary |
| `--browser-max-pages` | `BROWSER_MAX_PAGES` | `2×cpus` (min 4) | Max concurrent pages |
| `--browser-nav-timeout-ms` | `BROWSER_NAV_TIMEOUT_MS` | 30000 | Navigation timeout (ms) |
| `--browser-disable` | `BROWSER_DISABLE` | false | Disable browser tools entirely |

### TLS (HTTPS)

The HTTP transport can be served over TLS (rustls, `ring` provider — the same
backend as the reqwest search client). Provide a PEM certificate chain and
private key via `--tls-cert`/`--tls-key` or the `MCP_TLS_CERT`/`MCP_TLS_KEY`
environment variables and the HTTP server speaks HTTPS instead of plaintext. The
two must be supplied together or startup is refused; when neither is set the HTTP
transport stays plaintext (the default).

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SEARCH_PROVIDER` | `duckduckgo` | Search backend |
| `SEARCH_API_KEY` | — | API key for providers that require one |
| `SEARCH_API_URL` | — | API URL (SearXNG, Google) |
| `AUTH_TOKEN` | — | Bearer token for authentication |
| `MCP_TLS_CERT` | — | PEM certificate chain to serve HTTP over TLS |
| `MCP_TLS_KEY` | — | PEM private key matching `MCP_TLS_CERT` |

## MCP Compliance

Implements the [Model Context Protocol](https://modelcontextprotocol.io) revision **`2025-11-25`** over JSON-RPC 2.0, via stdio, TCP, or HTTP.

| Area | Support |
|---|---|
| Transports | stdio, TCP, HTTP (`POST /rpc`) |
| Protocol version | `2025-11-25`, negotiates down to `2025-06-18` / `2025-03-26` / `2024-11-05` |
| `initialize` | ✅ version negotiation + `instructions` |
| `tools/list`, `tools/call` | ✅ (12 tools) |
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
