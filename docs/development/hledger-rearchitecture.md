# Appendix — hledger Rearchitecture Evaluation & Fork Blueprint

> **Status — planning, not committed.** This appendix evaluates replacing the GnuCash
> backend with **hledger** (plain-text accounting) and lays out a concrete target for a
> **fork** of this repo. It reopens **MC-1** (backend choice) as a three-way:
> GnuCash-XML / GnuCash-SQLite / **hledger**.
>
> **Source repo:** this appendix originated in `gnucash-bindings-mcp`
> (https://github.com/yangm2/gnucash-bindings-mcp). References to "this repo", to source
> paths like `proxy/…` / `worker/…`, and to sibling docs resolve there. Cross-references:
> [multi-client.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/multi-client.md)
> (M10.2 snapshots, M10.4 concurrency) and MC-1/MC-6 in
> [00-overview.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/00-overview.md).

---

## 1. Why reconsider the backend

The CoWork blocker that motivated Phase 10 — a second `gnucash-mcp` process colliding
with the `SingletonLock` — is **an artifact of GnuCash's stateful architecture**, not of
MCP. The lock exists only because GnuCash's Python bindings need a mounted sparsebundle
inside a Linux container (one writer, one mount, one lock). Meanwhile the M10.2/M10.4
design kept converging on properties — append-only, immutable, diffable, git-friendly,
event-sourced, lock-free — that are **native to hledger** and that we were *bolting onto*
GnuCash.

## 2. What hledger makes native

| Design goal (Phase 10) | On GnuCash | On hledger |
|---|---|---|
| Append-only / immutable / corrections-as-reversal | discipline we impose | native idiom |
| Diffable / git-friendly | the compression + ordering + canonicalization spike | plain text; line-local appends; git is the default PTA workflow |
| Marginal-delta storage | fails — whole gzip file rewritten per save | appending lines *is* the delta |
| Event-sourced (op-log = truth, reports = projection) | a "big commitment" inversion | the journal **is** the op-log; balances always derived |
| No lock / no whole-file rewrite | `.LCK`, session save/end, WAL | append + `hledger check`; no lock |
| Audit + attribution | GnuCash "void" marks | **git history** (who/when/why per commit) |
| Multi-client transport | needs daemon-singleton rework | stateless CLI shell — see §4 |

It also **deletes stack complexity**: no Ubuntu container, no `python3-gnucash`, no
VirtioFS, no sparsebundle-for-bindings, and most of Phase 0 (spikes A–G, KU-1/2/3/12)
becomes moot. hledger is a single static binary.

## 3. What hledger does NOT solve

The **client cached-belief / epoch problem (M10.4) is agent-side and backend-agnostic.**
Two LLM clients with private context can still act on stale beliefs. So the fork still
needs the M10.4 mechanism (epoch CAS + idempotency + server-authoritative tools) — only
its *implementation* simplifies (the epoch becomes a git commit id; see §7).

## 4. Reference implementation — iiAtlas/hledger-mcp

`https://github.com/iiAtlas/hledger-mcp` (TypeScript/Node, stdio, MCP SDK; npm + `.mcpb`;
Jest; ~56★; active). A **generic** hledger CLI bridge.

| | iiAtlas/hledger-mcp | this repo (GnuCash) |
|---|---|---|
| Stack | TS/Node, stdio, subprocess to `hledger` | Swift proxy + Python worker in Apple Container |
| Statefulness | **stateless** CLI shell | stateful (container + sparsebundle + `SingletonLock`) |
| Tools | generic hledger surface (balance/register/print + add/remove/replace/import) | domain-modeled (invoice/budget/ECO/vendor/AP — MC-6) |
| Writes | append + `hledger check`; edits via temp+atomic-replace; `.bak`; `dryRun`; `--read-only` | session save/end, `.LCK`, WAL, pre-session clone |
| Concurrency | **none** — remove/replace by **file+line**; no idempotency; no write serialization | designed: epoch CAS + idempotency + serialized daemon (M10.4) |
| Live GUI | **manages `hledger-web` instances** (start/stop/list) | `gnucash-browse`, non-live, mutually exclusive |

**The headline:** because it is stateless (nothing to attach/lock), Desktop + CoWork +
Claude Code can each spawn it against the same journal and **the CoWork collision simply
does not occur.** This is empirical confirmation that our multi-client problem is
GnuCash-architecture-specific.

**Borrow from it:** CLI binary discovery (`HLEDGER_EXECUTABLE_PATH`), `hledger check`
before commit, atomic temp-replace, `.bak` backups, `dryRun`, `--read-only`, and the
`hledger-web` process management (`src/tools/web.ts`, `web-process-registry.ts`).

**What it lacks (our differentiators):**
- **Domain modeling** — it is a thin CLI surface, not an AP/budget/ECO ledger (MC-6).
- **Concurrency/correctness** — `remove-entry`/`replace-entry` locate by **file + line**
  (`src/tools/remove-entry.ts`): if the journal changed since the LLM read it, the line
  numbers shift and it edits the *wrong* entry — the exact **stale-belief TOCTOU** M10.4
  exists to catch. No idempotency keys (retried add double-posts). No write
  serialization (and `hledger-web`'s add-form is a *second* writer → file races).

For a personal journal that's fine; for an LLM-driven **money** ledger, M10.4 is the gap.

## 5. What you give up (costs)

- **The native macOS GnuCash GUI** as the human inspection surface — the project's
  explicit premise. hledger offers `hledger-web` (localhost web) and `hledger-ui` (TUI),
  not a peer to the GnuCash desktop app. **This is the one hard anchor.** Mitigation: a
  one-directional **hledger→GnuCash projection** keeps the GnuCash GUI as a non-live,
  reopen-to-refresh read model (see §8).
- **Re-modeling MC-6** in hledger idioms — but note MC-6 uses GnuCash as *plain
  double-entry + budgets + GUI*, not its invoice/business module, so the mapping is
  direct (§9). Budgets map to hledger periodic transactions + `balance --budget`.
- **Rewriting the worker layer** — but this *deletes* the container/bindings/sparsebundle
  complexity rather than porting it (§10).

## 6. Target architecture for the fork

```
MCP client(s) ──stdio/HTTP──> daemon ──subprocess──> hledger CLI ──> journal (git)
                                  │                                      │
                                  └── manages hledger-web --watch (read-only GUI)
```

- **Storage:** a plain-text hledger journal, **git-backed**. Consider `include`-per-period
  files (e.g. `2026.journal`) to keep appends local and reduce any merge surface.
- **Reads:** `hledger <cmd> -O json` (machine-readable) parsed by the daemon.
- **Writes (the whole lifecycle):** format the transaction text → **append** → run
  `hledger check` → **`git commit`**. No session, no lock, no WAL — the journal append
  *is* the durable log and the git commit *is* the WAL/recovery point.
- **Corrections:** append a **reversing transaction** (negated postings) tagged
  `; reverses:<txnid>`; never edit/remove a posted line (this is where we diverge from
  iiAtlas's file+line edits).
- **Concurrency:** single serializing daemon (or stateless + a write mutex); the epoch is
  the git HEAD (§7).
- **GUI:** the daemon starts/stops a **read-only `hledger-web --watch`** for the human
  (live; §8).

### Interface to hledger — subprocess + journal text (no API/RPC)

hledger has no daemon, socket, or RPC — it is a **stateless CLI over a plain-text
journal**. The interface is therefore a hybrid split by direction, isolated behind the
adapter seam (§16):

- **Reads → CLI with structured output.** Spawn `hledger <cmd> … -O json` and deserialize
  stdout (most report commands support `-O json`); each read re-parses the journal. No
  long-running process.
- **Writes → direct text append.** We format the transaction text ourselves, **append it
  to the `.journal`**, then `hledger check` (validate), then `git commit`. The **journal
  text is the write API**; hledger only *validates*, it never authors the entry.
- **Deliberately not the data plane:** `hledger add` / `import` (interactive / CSV-rules),
  `hledger-lib` (Haskell-only — FFI from Rust not worth it), and `hledger-web`'s HTTP/JSON
  API (that server is run only for the human GUI §8, not for our I/O).

**Interface contract** — both pinned to 1.52 and both behind the §16 adapter: the
**journal text format** (write) and the **CLI flags + `-O json` schema** (read). Because
it is *subprocess + file*, there is no shared in-process state or lock — the root reason
the stack is stateless (§4). The `git commit` on each validated append supplies M10.4's
epoch (commit id = epoch); reads come from `hledger -O json` against the working tree.

### Write-path failure & validation semantics

`hledger check` in the write path is a **post-condition assertion on our own writer**, not
input validation — the tools are server-authoritative, so a failure on text *we* generated
is (almost always) **our bug**. Therefore:

- **Validate before mutating (no rollback window).** Build a *candidate* (journal copy +
  new txn), run `hledger check --strict` (parse + balanced + `accounts` + `commodities`) on
  it, and only on success **atomically replace** the journal and `git commit`. On failure
  the **live journal is never touched** — the write aborts, nothing commits.
- **Fail closed; internal error.** A failure returns an **internal** error with the `check`
  output attached (for diagnosis), logged loudly — **not** a "rephrase-and-retry" tool
  error. *Input* problems are caught *before* formatting and returned as correctable tool
  errors; a post-format check failure is our bug.
- **Push correctness into tests.** `check --strict` is the belt; the suspenders are
  **property / round-trip tests** — for any valid inputs the formatter output passes
  `check --strict` *and* round-trips (`hledger print -O json` parses back to the same
  semantic txn) — plus golden tests. A production failure is a **test escape** → reproduce,
  add to corpus.
- **Carve-out — balance assertions ≠ formatter bug.** A failing `= $X` assertion can be
  legitimate: a real discrepancy, or the **M10.4 staleness** case (assertion built on a
  stale read while another write landed) → route to **`STALE` → re-read → retry**, not an
  internal bug. So keep balance assertions **out of routine postings**; reserve them for
  explicit **reconciliation** tools, where a failure is meaningful signal.
- **Idempotency / crash:** because nothing commits on failure, the `idem:` tag is never
  written (retry unblocked) and git stays clean; a crash after atomic-replace but before
  commit is reconciled at startup (commit-if-`check`-passes, else restore to HEAD).

**Language:** **Rust** — see §15 for the full analysis and rationale. The Python
worker/container disappears regardless of host language.

## 7. Concurrency on hledger — git commit *is* the epoch

The M10.4 model maps cleanly and gets simpler:

- **Epoch = git commit id** (or a monotonic commit count). One write → one commit → one
  epoch. M10.2's snapshot, the recovery point, and the epoch **collapse into the git
  commit** — no clone-on-commit, no `latest-published` pointer, no compression spike.
- **Idempotency key = a transaction tag** `; idem:<uuid>`. Dedup by
  `hledger print tag:idem=<uuid>` before appending (write-once, in-band — the hledger
  analog of the GnuCash KVP slot).
- **Per-connection last-seen** = the HEAD each connection last read; a consequential call
  is rejected `STALE` if `last-seen != HEAD`, forcing a re-read. No leases ⇒ no deadlock.
- **Soft invariants → flags** (over-budget, overdraft) computed by `hledger balance
  --budget` / queries; recorded, never enforced.

The TLA+ spec in [multi-client.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/multi-client.md)
(§ Formal verification) is **backend-agnostic** — only
the `epoch` interpretation changes (git HEAD instead of snapshot counter).

## 8. Live GUI

- **`hledger-web --watch` / `hledger-ui --watch`** live-reload on file change, and because
  appends are atomic text with no lock, a read-only watching GUI **coexists with the
  writer** — the live, concurrent inspection surface GnuCash could not provide (its native
  GUI loads once and we had to make it mutually exclusive).
- **Run hledger-web read-only** (`--capabilities=view`, no `add`/`manage`). Its write API
  (`PUT /add`) would be a **second writer** appending to the journal *outside* our pipeline
  — it bypasses the single-serializing-writer invariant (§4), the `git commit` epoch/audit
  (M10.4), the `idem:`/discipline/MC-6 conventions, and our atomic-replace recovery, while
  adding an unauthenticated network mutation surface. Worse, its **append** can be silently
  **clobbered** by our **atomic-replace** rename (lost write). Under the hood `/add` just
  appends text — so it offers nothing our disciplined `format → check --strict →
  atomic-replace → git commit` path doesn't, minus the safety. All writes go through the
  daemon; the GUI only reads. (hledger-web's *read* endpoints may optionally serve as a
  structured-read sidecar — still `view`-only.)
- **If the native GnuCash GUI is required**, generate a one-directional GnuCash file from
  the journal as a projection; that GUI stays **non-live** (reopen-to-refresh), same as
  today, just hledger-driven underneath.

## 9. Domain mapping — MC-6 tools → hledger idioms

Accounts are colon-paths (spaces allowed): `liabilities:ap:Acme Architecture`,
`expenses:construction:electrical`, `expenses:change orders:electrical`.

| Tool (MC-6) | hledger realization |
|---|---|
| `receive_invoice` | append txn: `expenses:… $amt` / `liabilities:ap:vendor $-amt`; tag `; invoice:REF` |
| `pay_invoice` | `liabilities:ap:vendor` / `assets:checking` |
| `fund_project` | `assets:checking` / `equity:owner capital` |
| `post_interest` | `assets:checking` / `income:interest` |
| `post_transaction` | arbitrary balanced postings |
| `void_transaction` | append reversing txn, tag `; reverses:<id>` (no in-place edit) |
| `update_transaction` | void + re-post (append-only); never line-edit |
| `get_account_balance` | `hledger balance <acct> -O json` |
| `list_transactions` / `get_transaction` | `hledger register` / `hledger print` (filter by tag/payee/date) |
| `get_ap_aging` | custom: query open `liabilities:ap:*` postings bucketed by date (no native aging) |
| `get_budget_vs_actual` | `hledger balance --budget` against periodic (`~`) budget rules |
| `budget_*` | manage periodic-transaction (`~`) budget definitions |
| `eco_*` (change orders) | `expenses:change orders:*` subtree + `; eco:NNN` tags; approve/void via tag/reversal |
| `vendor_*` | vendors are accounts; `vendor_add` ensures `liabilities:ap:vendor` (+ expense acct); `vendor_list` = `hledger accounts liabilities:ap` |
| `get_project_summary` | composite of `balancesheet` / `incomestatement` / `balance` |

**hledger tags** are the natural home for the metadata that lived in GnuCash KVP slots:
`idem:`, `invoice:`, `eco:`, `vendor:`, `reverses:`. ECO tracking via tags is arguably
cleaner than the GnuCash parallel-account convention.

## 10. What carries over vs. what gets deleted

**Carries over from this repo:**
- The MCP daemon pattern (Swift proxy: stdio/HTTP transport, signal handling, CLI
  subcommands), the **tool catalog + static resources** structure, the **MC-8 tool
  tiering / profiles (MC-10)**, the **MC-6 chart-of-accounts semantics**, the **M10.4
  concurrency model**, and the **test philosophy** (property tests, lifecycle invariants).

**Gets deleted:**
- `worker/` Python + `python3-gnucash`, the Dockerfile/container, Apple Container usage,
  `SparsebundleManager`/`hdiutil`, the `.LCK` handling, the **WAL** (journal append is the
  log), the **pre-session clone backup** and same-second-backup guard, and most Phase 0
  spikes. `ContainerPool`/`ContainerAPIClient` go away; the daemon shells out to `hledger`.

## 11. Migration — GnuCash book → hledger journal

One-time conversion of the existing book to a journal: export via a converter
(`gnucash`/`piecash` → ledger/CSV, or a small script over the existing book), then
`hledger check`. Reconcile balances against the GnuCash book before cutover. Commit the
resulting journal as the git root.

## 12. Fork strategy — two paths

- **A. Fork/depend on iiAtlas/hledger-mcp** as the hledger substrate; layer our **domain
  tools (§9)** and **M10.4 rigor (§7)** on top. Fastest to working; inherits packaging and
  `hledger-web` management. Cost: TS/Node, and re-adding correctness it lacks.
- **B. Greenfield in **Rust** (§15)** — swap GnuCash→hledger, borrow iiAtlas's
  CLI-invocation/validation patterns, keep the daemon shape where the epoch/serialization
  naturally lives. More build, cleaner fit, and the correctness target the money domain
  wants.

## 13. Open decisions for the fork

1. **Is the native GnuCash GUI a hard requirement** (→ projection, non-live) or does
   live `hledger-web` suffice? This gates the whole evaluation.
2. **Language/host:** *decided — **Rust** (§15).* (Go is the documented fallback.)
3. **Fork iiAtlas (A) vs greenfield (B).**
4. **Journal layout:** single file vs `include`-per-period.
5. Do we use any GnuCash feature beyond plain double-entry + budgets + GUI (invoice
   module, scheduled txns, reconciliation screen)? If not, the anchor is GUI-only.

## 14. Tests & formal verification

The M10.4 `T10.4.x` integration tests and the **TLA+/TLC** model
([multi-client.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/multi-client.md)
§ Formal verification) carry over unchanged — the spec's `epoch` is reinterpreted as the
git HEAD. Add hledger-specific tests: append→`hledger check` round-trip, idempotency-tag
dedup, reversing-entry correctness, `--budget` actual-vs-budget, and `hledger-web
--watch` live-reload against a concurrent writer.

---

## 15. Language & deployment targets — decision: **Rust**

**The component.** After the hledger pivot this is a thin MCP server that orchestrates
subprocesses (`hledger`, `git`, `hledger-web`) and files, speaks JSON-RPC over
stdio/HTTP, and does the M10.4 concurrency bookkeeping. It is **I/O- and
orchestration-bound** and **never touches the GPU** — in the Proxmox/vLLM deployment vLLM
owns the GPU and the local agent talks to this server **over MCP HTTP/SSE**. GPU
passthrough therefore imposes *no* language constraint; it only requires that the server
be first-class on Linux, containerize cleanly, and expose HTTP.

**Deployment targets:**
- **macOS** — Claude Desktop / Claude Code, **stdio** transport.
- **Linux (Proxmox container, co-located with vLLM)** — local-agent client, **HTTP/SSE**
  transport, static binary in a minimal image.

**Criteria:** cross-platform (macOS + Linux), clean static-binary containerization, solid
subprocess/file/JSON, both transports (stdio + HTTP/SSE), a usable MCP SDK.

| Language | macOS | Linux container (Proxmox/vLLM) | MCP SDK | Notes |
|---|---|---|---|---|
| **Rust** *(chosen)* | great | **excellent** — `…-linux-musl` static binary, tiny image | official `rmcp` | strong correctness for a money ledger; heavier dev loop; cross-compile via `cross`/CI |
| Go | great | best — trivial `GOOS=linux` cross-compile, ~10 MB static | official `go-sdk` + `mcp-go` | lightest "subprocess-orchestrating service" language; **documented fallback** |
| Swift | best | works, but build-on-Linux + larger image, less idiomatic | official, less mature | strongest only if macOS-primary; weakest container story |
| Python | fine | fine, piggybacks vLLM's runtime | official, most mature | fat image; ironic given we're deleting Python |
| Haskell | ok | ok | community | only if embedding `hledger-lib` instead of shelling out — unnecessary |

(hledger is Haskell, but we invoke it as a CLI subprocess, so the host language is
independent of it.)

**Why Rust:**
- **Containerizes cleanly** — `x86_64-unknown-linux-musl` static binary → a
  `scratch`/distroless image that drops next to vLLM with no runtime deps.
- **First-class on macOS and Linux** — one codebase, both targets.
- **Correctness rigor suits a money ledger** — the type system and exhaustive matching
  align with the M10.4 invariants (epoch CAS, idempotency, append-only); valuable where a
  silent error moves money.
- **Mature building blocks:** `rmcp` (MCP); `tokio` + `axum`/`hyper` (HTTP/SSE);
  `tokio::process` for `hledger`/`git`; `serde_json` for `hledger -O json` and JSON-RPC;
  `git2` (or shelling `git`); `clap` for the CLI.
- **Cost (accepted):** slower iteration than Go and a fiddlier macOS→Linux cross-compile
  (use `cross`, or build in CI) — acceptable for a long-lived, money-adjacent service.
  **Go is the documented fallback** if dev velocity is later prioritized over Rust's
  guarantees.

**Transport:** HTTP/SSE is **first-class** (the vLLM path requires it) alongside stdio
(the macOS path) — see M10.1.

**Reuse:** the existing **Swift** proxy informs the design (transport framing, tool
catalog / static resources, signal & lifecycle handling, the M10.4 logic), but the fork
is a **greenfield Rust rewrite** — the macOS-only Apple-container / Virtualization code is
deleted, not ported.

---

## 16. hledger version — pin **1.52** (not 1.99/2.0)

**Decision:** depend on hledger **1.52** (current stable, 2026-03-20), pinned in the
container/nix and CI. Do **not** base the fork on 1.99/2.0.

**Why not 2.0:**
- **It's a preview, not a release.** 1.99.x are explicitly "2.0 preview" builds (testers
  only, GitHub-only), with **no 2.0 release date**. Upstream keeps **hledger 1.x as the
  stable reference/fallback** and reserves 2.x for aggressive, possibly
  non-backward-compatible cleanups.
- **2.0's headline is irrelevant here.** The whole 2.0 thrust is **lot tracking & capital
  gains** (lot disposals, Gain/UnrealisedGain account types, Beancount-style cost basis,
  `--lots`/`-I` changes). A construction-project ledger has **no securities, cost basis,
  lots, or capital gains** — zero benefit.
- **Its one relevant change would break us.** The `ptype`→`preal` rename **changes
  `-O json` output**, exactly what the read path parses. 2.0 thus offers this domain zero
  benefit and one guaranteed breakage.
- **Packaging.** nix/distros ship **1.52 only**; 2.0 would mean building hledger from
  Haskell source in the container — friction against the minimal Rust+musl image (§15).

**Mitigations (make a future bump cheap):**
- **One adapter seam.** All hledger interaction lives in a single module — the CLI builder
  and the `-O json` parser. Parse **only the fields used; ignore unknowns**, so a
  `preal`/`ptype`-style rename touches exactly one place.
- **Golden-file contract tests.** Record real `hledger 1.52 -O json` output and assert the
  parser against it, so any version bump is caught immediately.
- **Write path is version-robust.** Appended entries are plain double-entry (no
  lots/cost-basis), stable across 1.x *and* 2.x; the version risk is entirely on the
  read/parse side, which the adapter contains.

**Revisit trigger:** move to 2.x only when it is a **formal, nix-packaged release** *and*
a real need for lots/cost-basis appears (e.g. investment tracking) — unlikely for
construction.

Sources: [hledger relnotes](https://hledger.org/relnotes.html) ·
[issue #2547 "Thoughts on hledger 2"](https://github.com/simonmichael/hledger/issues/2547).

---

## 17. Development & deployment dependencies (macOS / Linux)

The fork targets **two environments**: macOS (dev + Claude Desktop, stdio) and Linux
x86_64 (dev + a Proxmox container co-located with vLLM, HTTP/SSE). The MCP server is
**GPU-agnostic** — vLLM owns the GPU; the server only needs to run on the box and expose
HTTP. Typical asymmetry: **dev on Apple-Silicon (arm64) macOS, deploy to x86_64 Linux**,
so cross-compilation is part of the loop.

| Dependency | macOS (dev + Desktop) | Linux (dev + Proxmox/vLLM) | When |
|---|---|---|---|
| Rust toolchain (pinned via `rust-toolchain.toml`) | rustup, stable | rustup, stable | dev/build |
| `clippy` / `rustfmt` / `cargo-nextest` | yes | yes | dev |
| **hledger 1.52** (pinned, §16) | brew or nix | nix, or static release binary; **baked into image** | dev + deploy |
| `git` | system / brew | system / **in image** | dev + deploy |
| `hledger-web` (optional, live GUI §8) | brew / nix | in image | deploy |
| Java (JRE 11+) + `tla2tools.jar` | for TLC | for TLC | dev (M10.4 model-check) |
| Docker/Podman + `cross` | **to cross-build `…-linux-musl`** | native build | dev/CI |
| container runtime | — | Proxmox **LXC or VM**; slim/alpine image base | deploy |

**Build targets:** `aarch64-apple-darwin` (macOS native) and
`x86_64-unknown-linux-musl` (static, for the Proxmox image). Cross-building musl from
macOS needs `cross` (Docker) or a Linux build host — **CI is the reliable cross host**;
don't rely on a clean local macOS→musl path.

**nix as the unifier (recommended).** Since hledger 1.52 must be pinned and reproducible
on *both* OSes, a **nix flake** that provides `hledger 1.52` + the Rust toolchain + `git`
gives identical dev environments on macOS and Linux and pins the hledger contract the
adapter/golden tests (§16) depend on.

**The Linux image is *not* truly `scratch`.** The Rust musl binary has no runtime deps,
but the image must also ship the **hledger 1.52** and **git** binaries (and optionally
`hledger-web`). Use a **slim base (debian-slim / alpine)**, or a **statically-built
hledger** on alpine for a near-minimal image. (This refines the "distroless" note in §15:
distroless/scratch applies to *our* binary, not the whole image.)

**Runtime (deploy) needs:**
- **macOS:** the native binary + `hledger 1.52` + `git` on PATH; registered as a `command`
  in `claude_desktop_config.json` (stdio). No container.
- **Linux/Proxmox:** the image (Rust binary + hledger + git [+ hledger-web]); a
  **persistent volume for the journal git repo**; an exposed **HTTP/SSE port** for the
  vLLM-hosted agent; **no GPU** required by this service.

---

## 18. Dev tooling & handoff (mise · cargo · Buck2)

Preference: **Rust-native tooling (cargo) as the default**, with **mise** retained as the
language-agnostic task orchestrator (as in this repo, where mise wraps `swift build` +
container + install). Buck2 is a **revived option** for scale, not a day-one choice.

### Skills / knowledge that transfer to the fork

- **The architecture record itself** — this appendix + the [Phase 10 planning doc](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/multi-client.md)
  (M10.1–M10.4): the single-writer/epoch concurrency model, snapshot↔git mapping, the
  hledger interface contract (§6), and the write-path failure semantics.
- **MCP protocol experience** — this repo **hand-rolls** the MCP stdio transport
  (`MCPStdioTransport`); the fork can reuse that understanding (or adopt `rmcp`, §15). The
  HTTP/SSE transport (M10.1) becomes mandatory for the vLLM path.
- **Testing philosophy** — property tests + lifecycle invariants (Swift `ContainerPool`
  tests; Python Hypothesis tests) → carry to Rust as property/round-trip + golden tests
  (§6 write-path) and the TLA+/TLC model (M10.4).
- **mise-as-orchestrator pattern** and the **debug-vs-release dev loop**
  (`dev-install-app` builds a fast debug binary for edit→test).
- **Buck2 + local-REAPI caching prior art** — `gen-buck`/`fetch-buck-prelude` here and the
  **buck2-macos-local-reapi** project (local Remote-Execution-API cache on macOS).

### Rust-native tooling (preferred)

| This repo (mise task) | Rust fork |
|---|---|
| `build-proxy` (`swift build`) | `cargo build [--release]` |
| `build-worker` (`container build`, GnuCash) | **gone** — no GnuCash container; a `mise image` task builds the slim deploy image (§17) |
| `test` (pytest / `swift test` in container) | `cargo test` / `cargo nextest` |
| `lint` (ruff+pyright / swiftlint) | `cargo clippy` |
| `fmt` (ruff / swiftformat) | `cargo fmt` |
| `run` (one-shot dispatch) | `cargo run -- …` (stdio) / `--transport http` |
| `install-app` / `dev-install-app` | `mise` task: `cargo build` + register in `claude_desktop_config.json` (Desktop) |
| `gen-buck` / `fetch-buck-prelude` (SPM→BUCK) | **`reindeer`** (Cargo→Buck2) + native `rust_binary` — only if Buck2 path taken |
| `start-container-system`, `prune-*` | **gone** (no Apple Container) |

Pin the toolchain via `rust-toolchain.toml`; cross-build `…-linux-musl` via `cross`/CI
(§17). Keep **nix** for the pinned `hledger 1.52` + git contract the golden tests depend
on (§16/§17).

### mise's role in a Rust project

mise stays the **outer entrypoint** so the dev UX is uniform (`mise run build|test|lint|fmt|run|image|tla`),
delegating to cargo and wrapping the **polyglot/cross-cutting** steps cargo doesn't own:
the hledger pin + version check, `git`, nix env, the **TLA+/TLC** run (Java + `tla2tools.jar`),
the `cross` linux-musl build, the container image build, and golden-fixture regeneration.
This mirrors today's mise-wraps-`swift build`-plus-container pattern.

### Buck2 analysis (revived)

**State here:** the Buck2 work is **git-stashed, not committed** —
`stash@{0}: "buck2 experiment (custom swift/clang rules, spm2buck.py, prelude patcher)"`
(touches `.mise.toml`, `proxy/Package.swift`, `.gitignore`, `.claude/settings.json` plus
untracked `spm2buck.py` / custom rules). It is **Swift/SPM-specific**: `spm2buck.py` (run by
`gen-buck`) generates BUCK from the resolved SPM graph; `fetch-buck-prelude` pins the
prelude. The macOS local remote-cache path is prototyped in **buck2-macos-local-reapi**.
**Because it is stashed — and the fork starts with fresh git history — it will not travel;**
extract it from this repo's stash (`git stash show -p stash@{0}`) if you want the prelude /
local-REAPI / mise wiring as a reference.

**Rust path (if revived):** Buck2 has first-class Rust rules (`rust_binary`/`rust_library`),
and **`reindeer`** (Meta's official tool) **resolves + vendors the third-party crate closure
from `Cargo.toml` and generates the Buck2 `rust_library` targets for it** — the Cargo→Buck2
bridge, since Buck2 doesn't speak crates.io and needs an explicit, vendored, hermetic
dependency graph. It is the first-class analog of the bespoke SPM→BUCK `spm2buck.py`. The
buck2-macos-local-reapi local-REAPI cache applies directly to Rust compile artifacts.
**Only the *approach* transfers** — the stashed Swift/clang rules + `spm2buck.py` do **not**
map to Rust; use native `rust_binary` + `reindeer`, reusing only the prelude-pinning,
local-REAPI, and mise-integration patterns.

**When it pays off vs cargo:**
- **cargo by default** — simplest, idiomatic, sufficient for a single Rust crate; smoothest
  crates.io workflow, and **mature incremental caching** (local `target/`; add **`sccache`**
  for shared/CI compile caching) that covers most of Buck2's build-cache benefit at this
  scale.
- **Buck2 if the fork becomes a polyglot monorepo** — Rust MCP server **+ a Haskell sidecar**
  (the §6 escalation) **+ container images + TLA+ specs + nix-pinned toolchains** — where a
  single hermetic build graph with **remote/shared caching across languages** is worth the
  setup. That is Buck2's sweet spot, and the buck2-macos-local-reapi groundwork makes the
  macOS cache viable.

**Recommendation:** **mise + cargo is the default.** cargo's mature incremental cache
(+ `sccache` for shared/CI) covers the build-speed need at single-crate scale, so Buck2's
main draw — hermetic remote caching across a polyglot graph — doesn't yet earn its setup
cost. **Revive Buck2 + `reindeer` + local-REAPI only when the sidecar/polyglot monorepo
materializes** and cross-language cache-sharing becomes the priority.
