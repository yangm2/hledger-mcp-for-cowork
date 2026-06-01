# CLAUDE.md — hledger-mcp-for-cowork

An **MCP (Model Context Protocol) server** that exposes an [hledger](https://hledger.org)
plain-text ledger to Claude, designed to run inside a **Claude Cowork** project. Greenfield
**Rust** rewrite of the GnuCash-backed predecessor; hledger replaces GnuCash because
append-only / immutable / diffable / git-backed is hledger's native idiom.

> **Current phase: foundations in place, server not yet built.** The dev environment
> (nix + mise + quality gate) is set up — see *Dev environment & workflow*. The architecture
> below (concurrency, epoch CAS, domain tools, TLA+) is the design north star captured in
> `docs/development/`, **not yet implemented**. Don't let unbuilt architecture gate work —
> build foundations so it slots in cleanly.

> ⚠️ **This is a PUBLIC repository — never leak PII.** No real names, emails, account
> numbers, addresses, vendor identities, balances, or other personal/financial data in
> code, tests, fixtures, comments, commit messages, or PR descriptions. Use synthetic
> placeholders (`assets:checking`, `vendor:Acme`, `test@example.com`, fake amounts). Real
> ledger data lives only in the local journal/`.env.local` (gitignored), never committed.

## Stack

- **Language:** Rust, edition 2024 (toolchain pinned via `rust-toolchain.toml`, stable).
- **MCP SDK:** official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk).
- **Async:** `tokio`; subprocesses via `tokio::process` (shells out to `hledger` and `git`).
- **Orchestrator:** **mise** is the single entrypoint (`mise run <task>`); it delegates to
  cargo and wraps the polyglot/cross-cutting steps cargo doesn't own (nix env, hledger pin,
  container image, golden-fixture regen).
- **Reproducible env:** a **nix flake** pins **hledger 1.52** + the Rust toolchain + `git`,
  giving identical dev shells on macOS and Linux. mise delegates to the nix shell so the
  hledger contract the adapter/golden tests depend on is the same everywhere.
- **hledger version: pinned 1.52.** Do **not** target 1.99/2.0 — the `ptype`→`preal` rename
  changes `-O json` output (the read path), nix/distros ship 1.52, and 2.0's lot/cost-basis
  features are irrelevant to this domain. Verify the version at startup. (nixpkgs unstable
  currently ships exactly 1.52; `flake.lock` pins the revision.)

## Dev environment & workflow

The environment is set up. **One-time per clone:**

```
nix develop          # builds the pinned env (hledger 1.52, Rust, git, hledger-web)
mise run init-env    # writes .env.local pinning the hledger store path
```

After that the everyday loop runs **outside nix, sandboxed** — `mise run <task>` reads
`.env.local` to find hledger, so you don't re-enter `nix develop` for normal work.

- **nix flake** (`flake.nix` + `flake.lock`) owns the pinned runtime/contract deps: hledger
  1.52, the Rust toolchain (via `rust-toolchain.toml`, channel `stable`), `git`, `hledger-web`.
- **mise `[tools]`** owns the cargo dev tools (`cargo-nextest`, `cargo-llvm-cov`), pinned exact
  so they're available in the outside-nix loop. `mise install` provisions them.
- **`.env.local`** (gitignored) carries `HLEDGER_EXECUTABLE_PATH` → the pinned hledger binary.

**Tasks:** `build`, `fmt`, `lint`, `test`, `e2e`, `cov`, `check` (the fmt+clippy+test gate),
`run`, `init-env`. `test`/`check` use nextest when on PATH, else fall back to `cargo test`.

**Stop-hook quality gate** (`.claude/hooks/rust-quality-gate.sh`): on any turn that changed
`.rs`, runs fmt + clippy + tests and **blocks finishing on failure**. It sources `.env.local`,
so its e2e runs against real hledger. (Runs `cargo test`, not nextest — the hook's PATH lacks
mise's tool dir; functionally equivalent for gating.)

## Platform targets — native, no cross-compilation

Both **macOS and Linux are first-class**. Deployment is **native (or native in a
container)**, so **there is no cross-compilation step.** Each target builds on its own host.

> This **supersedes** `docs/development/hledger-rearchitecture.md` §15/§17, which describe a
> `cross`→`x86_64-unknown-linux-musl` workflow. We do **not** cross-build to musl. CI uses a
> per-OS matrix: a macOS runner builds the native macOS binary; a Linux runner builds the
> Linux binary and the container image natively.

| | macOS | Linux |
|---|---|---|
| Build | native (`aarch64-apple-darwin`) on a macOS host | native on a Linux host |
| Deploy | native binary, registered as a stdio `command` in Claude Desktop | binary in a slim container (image also carries `hledger 1.52` + `git`) |
| Transport | stdio (Desktop / Cowork SDK bridge) | stdio in-container; HTTP/SSE only if/when networked (see below) |

## Logging — platform-conventional

Use **`tracing`** as the facade everywhere; select the subscriber at runtime by platform:

- **macOS → the system unified logger** (`os_log`), via a `tracing` → oslog layer (e.g.
  `tracing-oslog`). Inspect with `log stream` / Console.app.
- **Linux (container) → conventional structured logs on stdout/stderr** (12-factor:
  `tracing-subscriber` JSON, collected by the container runtime). No log files in the image.

Never log secrets or full journal contents. On a write-path failure, log **loudly** with the
`hledger check` output attached (see write-path discipline).

## Transports — stdio first

- **stdio is the day-one transport** — it's what Claude Desktop and Cowork use (Cowork bridges
  local stdio servers via Desktop's SDK layer). This alone satisfies the Cowork goal.
- **HTTP/SSE is deferred behind a transport seam.** Its only value is the *networked* Linux
  deployment (a long-lived daemon co-located with vLLM that a remote agent or the local-model
  hybrid connects to over a socket) and serving multiple concurrent clients — a boundary stdio
  can't cross. Implement it only when that deployment actually materializes; keep the transport
  abstracted so adding it doesn't ripple.

## Quality bar — strict (enforce before calling work done)

This is a money-adjacent ledger; craftsmanship is the point.

- `cargo fmt --check` — formatted, no drift.
- `cargo clippy --all-targets --all-features -- -D warnings` — **zero** warnings.
- `cargo nextest run` (or `cargo test`) — all tests green.
- **Coverage ≥ 85% lines** via `cargo llvm-cov` (`mise run cov`). Informational until real
  code exists (today coverage is ~0%: only a stub `main`, e2e exercises hledger via subprocess).
- **Property tests** (`proptest`) on anything parsing/formatting — especially the hledger
  text formatter and the `-O json` parser (round-trip + golden-file contract tests).
- **`#![forbid(unsafe_code)]`** at the crate root unless a documented, reviewed exception.
- Public items carry doc comments; `cargo doc` builds clean. Errors via `thiserror`
  (library) / `anyhow` (binary edges); no `unwrap`/`expect` on fallible paths outside tests.

Prefer `mise run <task>` over raw cargo so the pinned env is in scope (tasks listed under
*Dev environment & workflow*). `mise run check` runs the full gate locally.

## The hledger interface (design contract)

All hledger interaction lives behind a **single adapter module** (one CLI-command builder +
one `-O json` parser). Parse **only the fields used; ignore unknowns** so a version bump
touches one place. This seam is covered by golden-file tests against recorded real
`hledger 1.52 -O json` output.

- **Reads → CLI with structured output:** spawn `hledger <cmd> … -O json`, deserialize stdout.
- **Writes → direct text append** (hledger only *validates*, never authors): format the txn
  text → build a candidate journal → `hledger check --strict` → **atomically replace** the
  live journal → `git commit`. On check failure the live journal is **never touched**; that's
  an *internal* error (our formatter bug), logged loudly — not a "rephrase-and-retry".
- **Corrections are append-only:** post a **reversing transaction** (tag `; reverses:<id>`),
  never edit/remove a posted line. Accounts are soft-deleted (tombstoned), never hard-deleted.
- Keep balance assertions **out of routine postings** — reserve them for explicit
  reconciliation tools where a failure is meaningful signal.

## Concurrency model (planned — see docs, not yet built)

Single serializing writer; **the git commit IS the epoch** (one validated write = one commit).
Idempotency via a write-once `; idem:<uuid>` tag. Consequential "decide" calls are epoch-checked
(reject `STALE` if a client's last-seen HEAD ≠ current HEAD → re-read → retry); append-only
"record" calls are not. To be formally checked with a TLA+/TLC model (`mise tla`).

## Repository map

- `src/` — the Rust crate (currently a stub `main.rs`).
- `tests/smoke.rs` — real-hledger e2e (write → `check --strict` → read → `git commit`); skips
  gracefully when hledger is absent.
- Dev env: `flake.nix`/`flake.lock`, `rust-toolchain.toml`, `mise.toml`, `.env.local`
  (gitignored), `.claude/hooks/rust-quality-gate.sh` (Stop-hook gate).
- `docs/development/` — the design corpus. Start here for depth:
  - `hledger-rearchitecture.md` — the master plan (language choice, version pin, deployment,
    tooling). **Note its cross-compile/musl guidance is superseded by this file.**
  - `concurrency-model.md` — single-writer + git-commit-epoch CAS + the TLA+ spec.
  - `tool-design.md` — MCP tool tiering, lazy resources, `--profile` filtering.
  - `chart-of-accounts.md` — the construction-project domain account model.
  - `mcp-protocol-versions.md` — `protocolVersion` negotiation (latest stable `2025-11-25`).
  - `model-options.md` — client architectures (Claude / local-model hybrid / vLLM).

## Conventions

- **Commits/PRs only when asked.** Branch off `main` first if asked to commit.
- Match surrounding code style; reference code as `path:line`.
- When you touch the hledger adapter, update or add a golden fixture in the same change.
