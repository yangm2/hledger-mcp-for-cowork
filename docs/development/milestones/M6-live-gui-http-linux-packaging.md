# M6 — Live GUI, HTTP/Linux transport, packaging

> **Goal.** Land the deferred deployment targets: a **read-only `hledger-web --watch`** live
> inspection surface, the **HTTP/SSE transport** behind its seam (the networked Linux path),
> the **slim Linux container image**, and the **per-OS CI matrix** — without cross-compilation.

## Why now / depends on

Depends on a working server (M0) and the full feature set (M4/M5) worth deploying. This is the
last **feature/deployment** milestone — everything here is explicitly deferred by CLAUDE.md and
the docs until the deployments actually materialize, so it comes after the product is real.

Split into independently shippable sub-parts; do only the ones a real deployment needs.

## In scope

### 6a — Live GUI (`hledger-web --watch`, read-only)

- Daemon **manages** a read-only `hledger-web --watch` (`--capabilities=view`, no
  `add`/`manage`) that live-reloads on journal change and **coexists with the writer** (atomic
  appends, no lock).
- **All writes go through the daemon; the GUI only reads.** Explicitly **do not** enable
  `PUT /add` — it would be a second writer bypassing the single-serializing-writer invariant,
  the git-commit epoch/audit, the `idem:`/discipline conventions, and atomic-replace recovery,
  and its append could be clobbered by our rename (lost write).
- Borrow the process-management pattern (start/stop/list) from iiAtlas
  (`web-process-registry`), adapted to Rust.

### 6b — HTTP/SSE transport (behind the seam)

- A **transport abstraction** so stdio (M0) and HTTP/SSE share one server core; adding HTTP
  doesn't ripple. (`tokio` + `axum`/`hyper` per §15.)
- Serves the *networked Linux* deployment (long-lived daemon co-located with vLLM, reached over
  a socket; see [model-options.md](../model-options.md) Options B/D) and **multiple concurrent
  clients** — the boundary stdio can't cross.
- Protocol concerns from [mcp-protocol-versions.md](../mcp-protocol-versions.md) that only apply
  to HTTP: `MCP-Protocol-Version` header, **no JSON-RPC batching** (correct for 2025-06-18+),
  Origin checks / auth **only if** networked beyond localhost.
- M3's connection-level epoch CAS now genuinely serves *multiple simultaneous* connections —
  re-validate C-1/C-5 under real concurrency.

### 6c — Packaging & CI (native, no cross-compile)

- **Per-OS CI matrix** (CLAUDE.md *Platform targets*): a macOS runner builds the native macOS
  binary; a Linux runner builds the Linux binary + container image **natively**. **No
  `cross`/musl cross-build** (this supersedes §15/§17's cross→musl guidance).
  - **Done (landed early, M0):** [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)
    runs the native build+test gate across `macos-latest` (aarch64-apple-darwin),
    `ubuntu-26.04` (x86_64), and `ubuntu-26.04-arm` (aarch64) — each runner `fmt`/`clippy`/
    `build --release`/`nextest run` on its own target, with hledger 1.52 + toolchain from the
    nix flake. A coverage job runs informationally (flip `--summary-only` → `--fail-under-lines
    85` at M1).
  - **Still 6c:** the container **image build** (`mise run image`) + an in-image container
    smoke test on the Linux leg.
- **Slim Linux image** (debian-slim/alpine) carrying the Rust binary **+ hledger 1.52 + git**
  (+ optional `hledger-web`); a `mise run image` task. Not truly `scratch` — the image ships
  hledger/git too (§17).
- macOS deploy: native binary + hledger + git on PATH, registered as a stdio `command` in
  Claude Desktop config. Linux deploy: the image + a persistent journal-git volume + exposed
  HTTP/SSE port; **no GPU** needed by this service.
- The Linux logging subscriber (JSON stdout, M0) is what the container runtime collects.
- **`--release` install variant.** M1's `install`/`uninstall` register the **debug** binary
  (`target/debug/…`) for the edit→test loop. Add a **release** install path for real use:
  `cargo build --release` and register `target/release/…`. Options to decide here: a flag on
  the existing task (`mise run install -- --release`) vs. a distinct `install-release` task; and
  whether release uses a **separate `mcpServers` key** (e.g. `hledger-mcp` for release vs.
  `hledger-mcp-dev` for debug) so a deployed install and a dev install can coexist without one
  clobbering the other. Recommend the separate-key approach — it makes "am I testing the dev or
  the deployed server?" unambiguous in Claude/Cowork. Drop `--debug` from the release entry's
  `args` (release is for use, not diagnosis).

## Out of scope (and where it lands)

- Cross-compilation to `x86_64-unknown-linux-musl` → **explicitly not done** (native per-OS
  builds; supersedes the docs' older guidance).
- The native **GnuCash GUI projection** / GnuCash migration → separate effort (§8/§11), not
  part of this arc.
- `tools/listChanged` profile promotion → still deferred (M5 note).
- Buck2 / `reindeer` / local-REAPI → only if a polyglot monorepo materializes (§18);
  **cargo + mise remains the default.**
- The hybrid local-model / subagent architecture ([model-options.md](../model-options.md)
  Option D) → downstream integration, not this server's build.

## Design references

- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §6 (architecture w/ hledger-web),
  §8 (live GUI, read-only rationale), §15 (Rust/transport/HTTP first-class), §17 (deps,
  image-not-scratch), §18 (cargo default, Buck2 later).
- CLAUDE.md — *Platform targets* (native, no cross-compile; per-OS CI matrix), *Transports*
  (HTTP/SSE deferred behind a seam), *Logging* (Linux JSON stdout).
- [model-options.md](../model-options.md) — the networked/vLLM client architectures HTTP serves.
- [mcp-protocol-versions.md](../mcp-protocol-versions.md) — HTTP-only protocol concerns.

## Work items

1. **6a:** hledger-web process manager (start/stop/list), read-only/view-only flags; ensure it
   coexists with a concurrent writer; document the no-`/add` rationale in code.
2. **6b:** extract the transport trait; implement HTTP/SSE (axum/hyper) sharing the M0 server
   core; header/batching/Origin handling; re-test epoch CAS under concurrent connections.
3. **6c:** `mise run image` (native Linux build + slim image with hledger/git); per-OS CI
   matrix (macOS native + Linux native + image); Desktop stdio registration task for macOS.
4. Deployment docs: macOS stdio config; Linux container + persistent journal volume + HTTP port.

## Testing & coverage

- **6a:** live-reload test — a daemon write is reflected in `hledger-web --watch`; assert the
  GUI cannot write (no `/add`) and a concurrent writer + watching reader don't corrupt
  (carries the §14 "hledger-web --watch live-reload against a concurrent writer" test).
- **6b:** the full lifecycle integration test (M0's) runs over **HTTP/SSE** as well as stdio;
  **C-1/C-5 re-validated under genuinely concurrent connections** (not just serialized).
- **6c:** CI matrix builds both natively and produces the image; a container smoke test runs
  `status` + a read tool inside the image against a journal on a mounted volume.
- **Coverage: ≥ 85% lines** (transport-shared core counts once; platform-specific glue covered
  by integration/container tests).

## Exit criteria

- [ ] `mise run check` green on **both** macOS and Linux CI runners; **cov ≥ 85%**.
- [ ] Per-OS CI matrix builds native macOS + native Linux + image; **no cross-compile step**.
- [ ] Read-only `hledger-web --watch` live-reloads on daemon writes and **cannot** write;
      concurrent writer + watcher don't corrupt the journal.
- [ ] HTTP/SSE transport works behind the seam; the lifecycle test passes over **both** stdio
      and HTTP; **C-1/C-5 hold under concurrent connections**.
- [ ] Slim Linux image carries the binary + hledger 1.52 + git; container smoke test green.
- [ ] **`--release` install variant** registers `target/release/…` (separate `mcpServers` key
      from the M1 debug install, so dev + deployed coexist without clobbering); uninstall
      removes the right one. Round-trip verified.
- [ ] Deployment documented for macOS (stdio/Desktop) and Linux (container + volume + HTTP).
- [ ] No PII in images, CI logs, or deployment docs.

## Exit-criteria review

> Fill in when closing M6. Do this per sub-part — 6a/6b/6c are independently shippable, so the
> verdict may legitimately be "6a+6c done, 6b deferred until the vLLM deployment exists." The
> load-bearing checks: the GUI genuinely cannot write (no second-writer), and the epoch CAS
> survives **real** concurrency over HTTP (not just the serialized stdio case M3 modeled).
> Record the verdict and any sub-part deferrals with the trigger that will close them.
