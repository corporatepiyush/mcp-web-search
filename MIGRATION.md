# Migration Guide — mcp-web-search 1.x → 2.0.0

**Release theme:** MCP specification compliance (protocol revision **`2025-11-25`**).

Version 2.0.0 is a **major** release because **tool failures changed from
JSON-RPC protocol errors to `isError` results**. Successful tool results are
unchanged — they were already returned as spec-compliant `content` arrays — so
most clients need no changes.

---

## Breaking changes

### 1. Tool failures are returned as results, not protocol errors

Provider errors, timeouts, and HTTP 429s from a `tools/call` previously came back
as JSON-RPC `error` objects (sometimes with `error.data`). They are now
`CallToolResult`s with `isError: true`:

**Before (1.x):**

```jsonc
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32010, "message": "provider returned 429", "data": { "retryAfter": 30 } } }
```

**After (2.0.0):**

```jsonc
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { "content": [{ "type": "text", "text": "provider returned 429" }], "isError": true }
}
```

This lets the model read the failure and back off / retry on its own.
Protocol-level errors (malformed request, missing `name`, unknown tool/method)
are still JSON-RPC `error` objects.

**Migration:** check `result.isError` on every `tools/call` response. If you
relied on `error.data` for rate-limit metadata, parse the message text instead.

### 2. Negotiated protocol version is now `2025-11-25`

`initialize` returns `"protocolVersion": "2025-11-25"` by default and negotiates:
a supported requested revision (`2025-11-25`, `2025-06-18`, `2025-03-26`,
`2024-11-05`) is echoed back; otherwise the latest is offered. Clients pinned to
`2024-11-05` keep working.

---

## New in 2.0.0

- **`logging/setLevel`** is now handled (accepts and acknowledges the level).
  The `logging` capability was advertised before but the method returned
  `-32601`; it is now a real no-op endpoint.
- **`instructions`** field in `InitializeResult`.
- **Protocol version negotiation** in `initialize`.

---

## Not yet implemented (roadmap)

Intentionally **not** advertised as capabilities until implemented:

| Feature | Notes |
|---|---|
| `resources/*` (`web://…` URIs) | fetched pages / search results as readable resources |
| `prompts/*` | `research-topic`, `verify-facts`, `compare-sources` |
| Streamable HTTP transport | current HTTP transport is POST `/rpc` |
| `notifications/message` (log streaming) | `logging/setLevel` is accepted but logs are not yet streamed |
| `completion/complete`, progress, cancellation | — |

---

## Upgrade checklist

- [ ] Reinstall: `cargo install mcp-web-search`.
- [ ] Check `result.isError` on `tools/call` responses (provider errors are no
      longer JSON-RPC errors).
- [ ] Replace any use of `error.data` rate-limit metadata with the message text.
- [ ] Verify your client tolerates `protocolVersion: "2025-11-25"` (or pin an
      older supported revision in `initialize`).
