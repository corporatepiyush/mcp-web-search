# mcp-web-search

High-performance, SSRF-hardened MCP server for web search, scraping, URL discovery, and multi-URL content extraction — a secure Rust alternative to `one-search-mcp`.

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

## Protocol

Implements the [Model Context Protocol](https://spec.modelcontextprotocol.io) via TCP line-delimited JSON-RPC or HTTP/REST.

## License

Apache-2.0
