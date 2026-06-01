# Appendix F — MCP Protocol Versions & Upgrade Notes

How the Swift proxy negotiates the MCP `protocolVersion`, what it implements, and
what a client may legitimately use against it. Written while debugging why Claude
Cowork registered the connector but never invoked its tools (see the "Cowork"
section below and README "Debugging in Claude Cowork").

---

## Version negotiation

`protocolVersion` strings are dated spec revisions. The client sends the version
it implements in `initialize`; the server replies with a version it supports.

| Revision | Notes relevant to this proxy |
|---|---|
| `2024-11-05` | Original baseline. Proxy historically hardcoded this. |
| `2025-03-26` | Streamable HTTP, OAuth 2.1, JSON-RPC **batching added**, tool annotations. |
| `2025-06-18` | JSON-RPC **batching removed** (breaking), structured tool output, elicitation, `MCP-Protocol-Version` header for HTTP. |
| `2025-11-25` | Latest stable. Tasks, tool icons, sampling tool-calls, OIDC discovery, JSON Schema 2020-12 default. |
| `2026-07-28` | Release candidate only — do not target. |

**Negotiation rule (lifecycle MUST):** if the server supports the client's
requested version, it responds with the *same* version; otherwise it responds with
the highest version it supports, and the client decides whether to proceed.

**Current behavior:** `initializeResult(clientProtocol:)` in
`MCPStdioTransport.swift` echoes the client's requested version, falling back to
`2024-11-05` when none is sent. Echoing is wire-safe (see below) but technically
asserts support for whatever is echoed. The stricter form is to **cap** the echo
to the newest revision actually validated (currently `2025-06-18`) and fall back
to the proxy's own baseline otherwise. Left as a blind echo for now to unblock
newer clients; revisit if a future revision changes wire framing.

---

## What the proxy implements

Declared capabilities: `{ tools: {}, resources: {} }` (no `listChanged`, no
`subscribe`).

| Method / behavior | Status |
|---|---|
| `initialize` | Static; echoes `protocolVersion`, logs `clientInfo` + version + roots |
| `tools/list` | Static, from `ToolCatalog` (no pagination — single page, no cursor) |
| `tools/call` | Forwarded to the Python worker container |
| `resources/list`, `resources/read` | Static + container fallback |
| `roots/list` (server→client) | Sent after first tool call to discover the sparsebundle path |
| `notifications/initialized`, `notifications/roots/list_changed` | Handled |

One JSON object per line on stdin/stdout; **no batch-array support** — which is
correct for `2025-06-18` and later.

---

## Capability negotiation fences off everything else

A server is only obligated to support a feature it **declares**. Because the proxy
advertises only `tools` and `resources`, a conformant client MUST NOT invoke
anything else. The 2025-11-25 additions therefore fall into three buckets, none of
which is a requirement for this proxy:

**Not applicable — HTTP/OAuth transport only** (proxy is stdio, no auth):
OpenID Connect discovery, incremental scope consent, OAuth Client ID Metadata
Documents, HTTP 403 on bad Origin, SSE polling/resumption, RFC 9728 metadata.

**Capability-gated — never declared, so never invoked:**
Tasks (`tasks`), tool icons, sampling tool-calls, elicitation, prompts,
completions, logging.

**Optional/SHOULD-level — safe to skip:**
tool/resource icons, `title`/`description` metadata, JSON Schema 2020-12 dialect
(plain `type`/`properties`/`required` schemas already qualify).

### Relevant gaps (optional, non-blocking)

1. **Cap the protocol echo** to a validated revision rather than echoing blindly
   (lifecycle MUST, strictly speaking).
2. **Tool-argument errors as `isError` results** rather than JSON-RPC `-32603`
   protocol errors (SHOULD, new in 2025-11-25 — lets the model self-correct).
   Current code returns dispatch failures as `-32603`.

### The one breaking change across the 2024→2025 gap

JSON-RPC **batching was removed in `2025-06-18`**. The proxy never assembled
batches (it reads one object per line), so it is already on the correct side of
the only wire-format break. A pre-2025-06-18 client that sent a batch array would
fail — not a concern for Desktop/Cowork, which are current.

---

## Cowork: why echoing matters

Claude Cowork runs in a sandboxed VM and bridges `claude_desktop_config.json`
stdio servers in via Desktop's SDK layer (`"type": "sdk"`). Its bundled MCP SDK
initializes with a newer `protocolVersion` than the proxy's old hardcoded
`2024-11-05`. A client that registers the connector but then **skips tool
discovery** is the classic signature of a version it did not expect being echoed
back. Echoing the client's version removes that mismatch.

The exact version Cowork sends is **not publicly documented** and changes across
Desktop builds — read it off the wire log instead. The `slog` line

```
gnucash-mcp: initialize from <client>/<version> protocol=<X> roots=<bool>
```

records it (and is mirrored to the unified log). See README "Debugging in Claude
Cowork" for the `sudo log stream` capture workflow.

### Observed on the wire (hledger MCP, Rust/`rmcp`, 2026-06-01)

Captured from a live Cowork project via `mise run debug-log` (the `tracing`
`initialize` line + `rmcp`'s `peer_info`), this is what Cowork's bridge actually
sent against the Rust server:

| Field | Value |
|---|---|
| `protocolVersion` | `2025-11-25` (current stable; our negotiator echoes it) |
| `clientInfo.name` | `local-agent-mode-<connector-name>` (e.g. `local-agent-mode-hledger-mcp` — derived from the `mcpServers` key, **not** a generic `claude-*`) |
| `clientInfo.version` | `1.0.0` |
| capabilities | `roots` (`listChanged: true`) and the `io.modelcontextprotocol/ui` **extension** (`{"mimeTypes": ["text/html;profile=mcp-app"]}`); no `sampling`/`elicitation`/`tasks` |

Takeaways: (1) Cowork is on `2025-11-25`, so capping the echo there (vs. blind-echo)
is correct and sufficient; (2) it sends `roots` + a `notifications/roots/list_changed`
shortly after `initialize`, so a server should tolerate that notification (we do —
`rmcp`'s default handler); (3) don't key behavior off `clientInfo.name` matching
`claude` — Cowork presents a connector-derived `local-agent-mode-*` name.

---

## References

- MCP 2025-11-25 changelog — <https://modelcontextprotocol.io/specification/2025-11-25/changelog>
- MCP lifecycle / version negotiation — <https://modelcontextprotocol.io/specification/2025-11-25>
- 2025-06-18 changes (batching removal) — <https://auth0.com/blog/mcp-specs-update-all-about-auth/>
- Cowork local MCP bridging — <https://dev.to/murat-a-a/how-we-got-local-mcp-servers-working-in-claude-cowork-the-missing-guide-nbc>
