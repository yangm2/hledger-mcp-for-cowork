# M0 — Walking-skeleton MCP + observability (the MVP)

> **Goal.** Stand up a minimal `rmcp` stdio MCP server that **Claude Cowork registers and
> actually invokes a tool on**, with logging + debug wired in from the first commit so every
> later milestone is diagnosable. No hledger backend yet — the tools are synthetic.

## Why now / depends on

Depends only on the dev environment already in place (nix + mise + quality gate). This is the
**MVP**: it alone satisfies the headline goal — *prove Claude Cowork can discover and call
tools this server registers* — and de-risks the single most documented failure mode before any
domain logic exists: a client that **registers the connector but never invokes its tools**
(see [mcp-protocol-versions.md](../mcp-protocol-versions.md) "Cowork: why echoing matters").

Observability lands **here, not later**, by explicit requirement: the only way to diagnose a
silent Cowork no-invoke is to read the `initialize` handshake off a log.

Unlocks: M1 (a live server to hang the first real tool on).

## In scope

- **`rmcp` stdio server** (the official Rust MCP SDK), launched by `cargo run` / `mise run run`.
- **Lifecycle:** `initialize` → `notifications/initialized` → `tools/list` → `tools/call`.
- **`protocolVersion` negotiation** per the lifecycle MUST: if we support the client's
  requested version, echo it; else return our newest validated revision. Cap the echo to a
  validated version (`2025-11-25` target; baseline fallback `2024-11-05`) rather than blind-
  echoing — closing the gap [mcp-protocol-versions.md](../mcp-protocol-versions.md) flags.
- **`server_instructions`** in the `initialize` result (a short static string; the
  `ledger://session-context` resource it will eventually point at arrives in M5).
- **Declared capabilities:** `{ tools: {} }` only (no `resources`/`listChanged`/`subscribe`
  yet — those are M5). Capability-fencing means a conformant client won't probe the rest.
- **Two synthetic tools** (no backend, deterministic, trivial to verify):
  - `status` — returns server name, version, active protocol version, uptime.
  - `echo` — takes `{ message: string }`, returns it back. The minimal "Cowork called our
    tool and got our output" proof.
- **Observability (the core of this milestone):**
  - `tracing` as the facade everywhere.
  - **macOS subscriber → Apple unified logging via [`apple-log`](https://docs.rs/apple-log/0.6.0/apple_log)
    0.6.0.** A thin `tracing_subscriber::Layer` forwards events to `apple_log` (`Logger` /
    `os_log::log`) under a fixed subsystem + category. (`apple-log` 0.6.0 is the pinned choice
    per CLAUDE.md "Logging" — not `tracing-oslog`.)
  - **Linux subscriber → `tracing-subscriber` JSON on stdout/stderr** (12-factor), selected at
    runtime by platform.
  - **Debug verbosity** via `RUST_LOG` (env-filter); a repeatable `-v` flag bumps the default
    when `RUST_LOG` is unset (`-v` = debug, `-vv` = trace).
  - **Handshake wire-log (the diagnostic):** on `initialize`, log at INFO a single structured
    line capturing `clientInfo` (name/version), the requested + negotiated `protocolVersion`,
    and whether `roots` was sent — the exact signal that distinguishes "Cowork never sent
    initialize" from "version mismatch" from "tools/list never called." Mirror the
    protocol-versions doc's `initialize from <client>/<version> protocol=<X> roots=<bool>`.
  - **Never log secrets or journal contents** (none exist yet; the discipline starts now).
- `#![forbid(unsafe_code)]` at the crate root. (Verify `apple-log` is used through its safe
  API; if any path needs `unsafe`, isolate it behind a documented, reviewed exception module —
  do not relax the crate-root forbid silently.)

## Out of scope (and where it lands)

- Any hledger subprocess call, including the startup version-pin check → **M1**.
- The `-O json` read adapter, golden fixtures → **M1**.
- Writes, git, idempotency → **M2**.
- Epoch CAS, per-connection state → **M3**.
- Domain tools, resources, profiles → **M4/M5**.
- HTTP/SSE transport, Linux container, `hledger-web` → **M6**. (M0 ships stdio only; the
  platform-split subscriber is M0 because macOS Cowork is the target, but the *transport* seam
  is deferred.)

## Design references

- [mcp-protocol-versions.md](../mcp-protocol-versions.md) — negotiation rule, capability
  fencing, the Cowork echo issue, the wire-log line format.
- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §15 (Rust, `rmcp`, `tokio`), §6
  (daemon shape).
- CLAUDE.md — *Transports — stdio first*, *Logging — platform-conventional*, *Quality bar*.

## Work items

1. Add deps: `rmcp`, `tokio`, `tracing`, `tracing-subscriber` (env-filter, json), `clap`,
   `serde`/`serde_json`, `anyhow`/`thiserror`; `apple-log = "0.6"` under a `cfg(target_os =
   "macos")` dependency. Pin exact where the sandbox needs it (mirror `mise.toml` convention).
2. `main.rs`: parse CLI (`-v` verbosity count, future `--profile`/`--transport` placeholders documented
   but inert), install the platform subscriber, start the stdio server, handle SIGINT/SIGTERM
   for clean shutdown.
3. `logging` module: the platform-selected subscriber. macOS → `apple-log` layer (fixed
   subsystem `io.github.yangm2.hledger-mcp-for-cowork`, category `mcp`); Linux → JSON.
   `RUST_LOG` filter.
4. `server` module: `rmcp` handler implementing `initialize` (negotiation + `server_instructions`
   + the handshake wire-log), `tools/list`, `tools/call` dispatch.
5. `tools` module: `status` + `echo`, each with a JSON schema and a doc comment.
6. **Tool-argument errors return as `isError` tool results, not JSON-RPC `-32603`** (the SHOULD
   from [mcp-protocol-versions.md](../mcp-protocol-versions.md) "Relevant gaps" #2) — lets the
   model self-correct. Establish this pattern now so every later tool inherits it.
7. A `mise run`-able way to tail logs on macOS documented in the milestone (`log stream
   --predicate 'subsystem == "io.github.yangm2.hledger-mcp-for-cowork"'` / Console.app).
8. **Registering the server for the Cowork-invoke proof:** M0 may register manually — add an
   `mcpServers` entry pointing at the built binary in `claude_desktop_config.json`
   (`~/Library/Application Support/Claude/…` on macOS; Cowork bridges Desktop's stdio servers
   via the SDK layer). The automated `mise run install` / `uninstall` tasks land in **M1**; a
   one-time hand edit is acceptable here just to capture the invoke proof.

## Testing & coverage

- **Unit:** protocol-version negotiation (echo when supported; cap/fallback otherwise) as a
  pure function over (requested, supported-set) → negotiated. Table-driven, including the
  `2024-11-05` fallback and an unknown future version.
- **Unit:** `echo`/`status` tool dispatch — argument deserialization, the `isError` path for a
  malformed argument (asserts we return an error *result*, not a protocol error).
- **Integration (`tests/`):** spawn the built binary, drive a scripted stdio session
  (`initialize` → `initialized` → `tools/list` → `tools/call echo`), assert: `tools/list`
  advertises exactly `status` + `echo`; `echo` round-trips; the negotiated `protocolVersion`
  matches the rule.
- **Logging verification (macOS):** reading the unified log back **programmatically is not
  automatable** — `OSLogStore` enumeration requires `logd` access that the dev sandbox blocks
  and that needs elevated privileges generally (the GnuCash-MCP predecessor hit the same wall
  and relied on an operator-run privileged `log` redirect). So the automated macOS test asserts
  only the **deterministic write half**: the `apple-log` bridge constructs a `Logger` for the
  project subsystem, and events flow through the `OsLogLayer` without panicking. The end-to-end
  emit path is additionally exercised by the stdio integration test (the spawned server logs
  its handshake through this layer). **Operator step (manual, privileged):** run
  **`mise run debug-log`** (a sudo `log stream` filtered to the project subsystem at
  `--level debug`, teed to `.debug/*.ndjson`) — or Console.app — to confirm entries and capture
  them for diagnosis. Tests gated on `cfg(target_os = "macos")`.
- **Coverage:** informational this milestone (see the ramp in the [README](README.md)). Cover
  the pure negotiation + dispatch logic well; the transport/subscriber glue is exercised by the
  integration + smoke path rather than unit-counted (the binary entrypoint runs as a subprocess
  under the integration test, so it does not show up in `llvm-cov`).

## Exit criteria

- [ ] `mise run check` green (fmt + clippy zero-warnings + tests).
- [ ] `#![forbid(unsafe_code)]` holds (or a single documented, reviewed exception is recorded).
- [ ] The binary speaks a full `initialize → tools/list → tools/call` cycle over stdio
      (integration test proves it).
- [ ] Protocol-version negotiation obeys the lifecycle rule (unit table green).
- [x] **Claude Cowork registers the connector AND successfully invokes a tool** (the headline
      MVP proof) — captured 2026-06-01 from a live Cowork project: the client (Cowork reports
      its `clientInfo.name` as `local-agent-mode-hledger-mcp` v1.0.0) ran
      `initialize`→`tools/list`→`tools/call status`→`tools/call echo`, both results
      `is_error:false`.
- [x] Automated macOS logging test green (bridge constructs a `Logger`; events flow through
      `OsLogLayer` without panic). **Operator-confirmed:** `mise run debug-log` captured the
      handshake + tool calls under the project subsystem in cleartext (programmatic
      `OSLogStore` readback is *not* automatable — privileged `log` stream instead).
- [ ] Tool-argument errors come back as `isError` results, not `-32603` (unit test).
- [ ] `cargo doc` builds clean; new public items documented.
- [ ] No PII anywhere (subsystem/sample data are synthetic).

## Exit-criteria review

**Reviewed 2026-06-01.** Verdict: **done** — the implementation is complete, the automated gate
is green, and the two operator/interactive items have now been confirmed against a live client
(see *Operator-confirmed* below).

Evidence walked against the checklist:

- ✅ **Gate green.** `cargo fmt --check` clean; `cargo clippy --all-targets --all-features -- -D
  warnings` zero warnings; `cargo test` = 12 lib + 3 stdio-integration + 1 smoke, all passing.
- ✅ **`#![forbid(unsafe_code)]`** holds at both crate roots (`lib.rs`, `main.rs`); no exception
  needed (`apple-log`'s `unsafe` is contained in the dependency).
- ✅ **Full stdio lifecycle** proven by `tests/mcp_stdio.rs::full_lifecycle_lists_tools_and_echoes`
  (`initialize → initialized → tools/list → tools/call echo`); `tools/list` advertises exactly
  `status` + `echo`; `resources` capability absent.
- ✅ **Negotiation rule** — `src/protocol.rs` unit table (echo supported; cap unknown
  future/legacy to `2025-11-25`; baseline `2024-11-05` echoed), reinforced over the wire by
  `unknown_protocol_version_is_capped_not_echoed`.
- ✅ **`isError` not `-32603`** — `bad_tool_args_return_iserror_not_protocol_error` (integration)
  + three `server::tests` dispatch cases. Note: `rmcp` maps `Parameters<T>` failures to
  JSON-RPC `invalid_params`, so `echo` reads a lenient `JsonObject` and validates internally to
  return an `isError` *result*.
- ✅ **Automated macOS logging test** — `logging::macos::tests` (bridge constructs a `Logger`;
  events flow through `OsLogLayer` without panic).
- ✅ **`cargo doc`** builds clean; public items documented.
- ✅ **No PII** — synthetic subsystem/sample data only.
- ⚠️ **Coverage 71.98% lines** (`cargo llvm-cov`): `protocol.rs` 100%, `server.rs` 82%,
  `logging.rs` 78%, `main.rs` 0% (binary entrypoint exercised only as a subprocess by the
  integration test, which `llvm-cov` does not instrument). **Informational** for M0; the ≥85%
  gate begins at M1, where the pure adapter is the bulk of the code.

**Operator-confirmed (2026-06-01, from a live Claude Cowork project, via `mise run debug-log`):**

Inside a Cowork project, asking the assistant to show the registered tools (and then to call
them) drove the full lifecycle, captured in the unified log under subsystem
`io.github.yangm2.hledger-mcp-for-cowork`. Cowork's bridge reported `clientInfo`
`name="local-agent-mode-hledger-mcp"`, `version="1.0.0"`, capabilities
`{ io.modelcontextprotocol/ui extension, roots.listChanged }`:

- `initialize` — `protocol.requested=2025-11-25 → negotiated=2025-11-25`, `roots=true` (our
  handshake wire-log).
- `tools/list` → advertised `echo` + `status`.
- `tools/call status` → `"hledger-mcp 0.1.0 — protocol 2025-11-25, uptime 24s"`, `is_error:false`.
- `tools/call echo {"message":"test"}` → `"test"`, `is_error:false`.

This closes both previously-deferred items: **Claude Cowork invokes** the tools, and the
**os_log pipeline is confirmed** (lines emitted `Public`/cleartext; captured via the privileged
`log stream`). M0 is complete and unblocks **M1**.
