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
  - **Debug verbosity** via `RUST_LOG` (env-filter); a `--debug`/`-v` flag bumps the default.
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
2. `main.rs`: parse CLI (`--debug`, future `--profile`/`--transport` placeholders documented
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
- **Logging verification (macOS):** after a handshake, read back the unified log via
  `apple_log::OSLogStore` (`CurrentProcessIdentifier` scope) and assert the `initialize`
  wire-log line is present with the expected subsystem/category — turning "logging works" into
  a checked assertion rather than a manual eyeball. (Gate this test on `cfg(target_os =
  "macos")`; skip gracefully elsewhere, like the smoke test skips when hledger is absent.)
- **Coverage:** informational this milestone (see the ramp in the [README](README.md)). Cover
  the pure negotiation + dispatch logic well; the transport/subscriber glue is exercised by the
  integration + log-readback tests rather than unit-counted.

## Exit criteria

- [ ] `mise run check` green (fmt + clippy zero-warnings + tests).
- [ ] `#![forbid(unsafe_code)]` holds (or a single documented, reviewed exception is recorded).
- [ ] The binary speaks a full `initialize → tools/list → tools/call` cycle over stdio
      (integration test proves it).
- [ ] Protocol-version negotiation obeys the lifecycle rule (unit table green).
- [ ] **Claude Cowork registers the connector AND successfully invokes `echo`** (the headline
      MVP proof) — captured as a log excerpt of the `initialize` line + the `tools/call`.
- [ ] macOS logs are visible via `log stream` / Console.app under the project subsystem, and
      the `OSLogStore` readback test asserts the handshake line.
- [ ] Tool-argument errors come back as `isError` results, not `-32603` (unit test).
- [ ] `cargo doc` builds clean; new public items documented.
- [ ] No PII anywhere (subsystem/sample data are synthetic).

## Exit-criteria review

> Fill in when closing M0. Run `mise run check` (and `mise run cov` for the informational
> number), walk the checklist, tick only what's demonstrated, record any dated deferral, and
> write the one-paragraph verdict (*done / done-with-deferrals / not-done*). Pay special
> attention to the Cowork invoke proof — if Cowork registers but does not invoke, the captured
> `initialize` log line is the primary evidence for diagnosing why (version echo, missing
> capability, or no `tools/list`).
