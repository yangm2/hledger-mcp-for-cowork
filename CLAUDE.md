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
- **Async:** `tokio`; **hledger** runs as a subprocess via `tokio::process`. **git is in-process
  via the [`git2`](https://docs.rs/git2) crate** (libgit2, `vendored-libgit2` — statically linked,
  no `git` binary or system libgit2 needed at runtime); `#![forbid(unsafe_code)]` still holds (the
  FFI lives in `libgit2-sys`). The write path (M2) uses git2 for the commit-as-epoch.
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
mise run init-settings-local   # reconstitute the sandbox allowlist (takes effect next session)
nix develop                    # builds the pinned env (hledger 1.52, Rust, git, hledger-web)
mise run init-env              # writes .env.local pinning the hledger store path
```

After that the everyday loop runs **outside nix, sandboxed** — `mise run <task>` reads
`.env.local` to find hledger, so you don't re-enter `nix develop` for normal work.

- **nix flake** (`flake.nix` + `flake.lock`) owns the pinned runtime/contract deps: hledger
  1.52, the Rust toolchain (via `rust-toolchain.toml`, channel `stable`), `git`, `hledger-web`.
- **mise `[tools]`** owns the cargo dev tools (`cargo-nextest`, `cargo-llvm-cov`), pinned exact
  so they're available in the outside-nix loop. `mise install` provisions them.
- **`.env.local`** (gitignored) carries `HLEDGER_EXECUTABLE_PATH` → the pinned hledger binary.
- **`.claude/settings.local.json`** (gitignored) holds machine-local **sandbox-allowlist paths**
  under your `$HOME` (cargo/rustup/mise caches) that enable prompt-free sandboxed work. Those
  absolute home paths are PII, so they are **never committed**; `mise run init-settings-local`
  reconstitutes them per clone. Sandbox settings load at session start — re-open the session
  for them to apply.

**Tasks:** `build`, `fmt`, `lint`, `test`, `e2e`, `cov`, `check` (the fmt+clippy+test gate),
`run`, `clean` (cargo artifacts), `clean-more` (+ generated dev-env files), and the per-clone
setup `init` (= `init-env` + `init-settings-local`). `test`/`check` use nextest when on PATH,
else fall back to `cargo test`.

**Linux portability is covered by the native CI matrix, not a local cross-lint.** There is no
`check-cross` task: the per-OS CI matrix (see *Platform targets*) compiles and tests natively on
Linux every push/PR, which is what catches Linux-only breakage (e.g. `apple-log` being
macOS-scoped). A local macOS→Linux cross-clippy was retired — it's redundant with that matrix
and, once `git2`/`vendored-libgit2` lands (M2), would demand a cross C toolchain to compile
libgit2 for the foreign target.

**Stop-hook quality gate** (`.claude/hooks/rust-quality-gate.sh`): on any turn that changed
`.rs`, runs fmt + clippy + tests and **blocks finishing on failure**. It sources `.env.local`,
so its e2e runs against real hledger. (Runs `cargo test`, not nextest — the hook's PATH lacks
mise's tool dir; functionally equivalent for gating.)

**Subagent delegation — all mise tasks run fully sandboxed (verified 2026-06-09):**

A Haiku subagent clean-compiled `apple-log` (the `swift build` bridge) sandboxed with no
permission prompt, and the full `check` gate (fmt + clippy + 107 tests incl. real-hledger e2e)
passes sandboxed. Compiling tasks **may be delegated to subagents**. Historical context: the
sandbox blockers and their fixes (all three allowlist/wrapper pieces are required):
- **Swift PM's own subprocess sandbox** (`sandbox-exec: sandbox_apply: Operation not
  permitted` — nested sandboxing is blocked): fixed by `scripts/bin/swift` (prepended to PATH
  by mise), which injects `--disable-sandbox` into `swift build`.
- **Foundation atomic writes** (swift-driver writes `output-file-map.json` atomically; the
  temp file stages in the destination *volume's* item-replacement dir, `<volume
  root>/.TemporaryItems` — kernel log: `deny(1) file-read-data /Volumes/Work/.TemporaryItems`):
  fixed by allowlisting `<volume root>/.TemporaryItems` (added by `init-settings-local`; on a
  root-volume clone staging goes to `/var/folders`, already covered).
- **Swift PM caches**: `~/Library/Caches/org.swift.swiftpm` and `~/Library/org.swift.swiftpm`
  are in the allowlist.

If a sandboxed compile regresses, check the kernel sandbox-violation log:
`sudo /usr/bin/log show --last 5m --predicate 'sender == "Sandbox"'` (requires sudo, so the
user must run it — and note plain `log` is shadowed by a zsh builtin).

**Delegation rules (apply to every subagent prompt):**
- mise auto-loads `.env.local` — never tell a subagent to `source` it; `mise run <task>` is
  the complete command.
- **No deletion in subagents**: `clean`/`clean-more` (or any `rm`) triggers a top-level
  permission prompt that stalls the subagent. Run cleans in the parent first.
- A subagent's `run_in_background` process is killed when its turn ends — long jobs must be
  blocking calls inside the subagent.

**Delegatable task prompt** (build / lint / test / e2e / check / fmt / init-*):
```
Working directory: /Volumes/Work/prj/hledger-mcp-for-cowork
Run sandboxed (do NOT use dangerouslyDisableSandbox): mise run <task>
mise auto-loads .env.local — no need to source it. Allow up to 5 minutes (a cold build
compiles a Swift bridge). Do not run any clean/delete commands.
Report: PASS or FAIL, plus the last ~15 lines of output.
```

**Keep in the parent, foreground:**
- `cov` — llvm-cov only instruments spawned subprocesses when run foreground; run as a
  blocking tool call in the parent (sandbox status in a subagent untested).
- `mutants` — runtime, not sandbox: scope with
  `mise exec -- cargo mutants -f <file> --re <fn_name>` (a full-file run is 30+ min).

**Explore-agent subagents** (pure grep/read/search) remain the best choice for any code search
taking more than 3 direct queries — no compilation involved.

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

- **macOS → the system unified logger** (`os_log`), via a `tracing` → oslog layer built on
  **[`apple-log`](https://docs.rs/apple-log/0.6.0/apple_log) 0.6.0** (a thin
  `tracing_subscriber::Layer` forwarding events to `apple_log` under a fixed subsystem +
  category). Inspect with `log stream` / Console.app; tests assert log lines via
  `apple_log::OSLogStore`. (`apple-log` is the pinned choice — not `tracing-oslog`.)
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

Single serializing writer — two layers, since stdio multi-client = multi-*process* (in-process
async mutex + cross-process `flock` beside the journal); **the git commit IS the epoch** (one
validated write = one commit). Idempotency via a write-once `; idem:<uuid>` tag. Consequential
"decide" calls are epoch-checked (reject `STALE` if a client's last-seen HEAD ≠ current HEAD →
re-read → retry, checked *inside* the write locks); append-only "record" calls are not. To be
formally checked with a TLA+ model via the Rust `tla-checker` (`mise tla`; spec kept
TLC-compatible).

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
- **Large static resources live in `.md` files, compiled in via `include_str!`** — not inline
  string literals. Authoring prose (server instructions, `ledger://` session-context + guides)
  as real markdown keeps it diffable/lintable/reviewable, while `include_str!` reads it **at
  compile time** into a `&'static str` baked into the binary's `.rodata`. The result is a
  **single self-contained binary** with no runtime files to ship and no extra linked objects —
  exactly the property the stdio/Desktop and slim-container deployments want. Keep the files in
  a `resources/` dir beside the module (e.g. `include_str!("resources/session-context.md")`).
  (Small one-liners may stay inline; the rule targets multi-line/large content.)
