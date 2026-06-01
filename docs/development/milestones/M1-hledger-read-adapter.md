# M1 — hledger read adapter (the seam)

> **Goal.** Introduce the **single hledger adapter module** (one CLI-command builder + one
> `-O json` parser), verify the **1.52 version pin at startup**, and ship the first *real*
> read tools backed by `hledger`. This is the §16 seam every later milestone reads through.

## Why now / depends on

Depends on **M0** (a live server + logging to hang real tools on). It is the first
**foundational** milestone: every read path in the system funnels through this one module, so
getting its shape — *parse only the fields used, ignore unknowns* — right now is what makes a
future hledger version bump a one-file change.

Coverage becomes a **hard gate (≥ 85% lines)** starting here: the parser and CLI builder are
pure and must be near-fully covered.

Unlocks: M2 (the write path reuses the same adapter for read-back/validation) and M4 (domain
read tools).

## In scope

- **One adapter module** (`src/hledger/` or similar) containing exactly two responsibilities,
  isolated from the rest of the crate:
  - **CLI-command builder** — constructs `hledger <cmd> … -O json -f <journal>` argument
    vectors; no shelling logic leaks outside.
  - **`-O json` parser** — `serde` structs that **deserialize only the fields we use and
    ignore unknowns** (`#[serde(...)]`; no `deny_unknown_fields`), so a `ptype`→`preal`-style
    rename touches one place.
- **Subprocess execution** via `tokio::process`, resolving the binary from
  `HLEDGER_EXECUTABLE_PATH` (falling back to `hledger` on PATH) — the convention the smoke test
  and `.env.local` already use.
- **Startup version check:** run `hledger --version`, assert it is **1.52.x**; on mismatch log
  loudly (warn) and surface in `status`. Decide and document the policy (refuse to start vs.
  warn-and-continue) — recommend **warn-and-continue for reads, hard-gate before M2 writes**.
- **First real read tools** (Tier-1 operational, names from §9):
  - `get_account_balance` → `hledger balance <acct> -O json`.
  - `list_transactions` → `hledger register …` / `get_transaction` → `hledger print …`
    (filter by date/payee/tag). Start with one or two; the full domain set is M4.
- **Golden-file contract tests:** recorded real `hledger 1.52 -O json` output checked into
  `tests/fixtures/`, asserted against the parser. A `mise run`-able **golden regen** task so a
  version bump regenerates fixtures deliberately.
- **Property / round-trip tests** on the parser (the read half of the §6 round-trip contract).
- `status` extended to report the detected hledger version + pin-match.
- **`install` / `uninstall` mise tasks** — register/unregister this server in **Claude
  Desktop's** `claude_desktop_config.json` as a stdio `command` entry pointing at the built
  binary (with `HLEDGER_EXECUTABLE_PATH` from `.env.local` in its `env`). This is also what
  exposes the server to **Claude Cowork**, which bridges Desktop's stdio servers via the SDK
  layer (see [mcp-protocol-versions.md](../mcp-protocol-versions.md) "Cowork"). Idempotent
  (merge, don't clobber other servers); macOS path primary, Linux path handled. Replaces the
  manual config-editing M0 relied on for its one-time Cowork-invoke proof.

## Out of scope (and where it lands)

- **Any write / mutation / git** → **M2**.
- Epoch / per-connection state → **M3**.
- The full §9 domain tool catalog (invoice/pay/fund/vendor/AP/budget/ECO) → **M4/M5**.
- `hledger-web`, HTTP transport, container → **M6**.

## Design references

- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §16 (the adapter seam, parse-only-
  used-fields, golden tests, version pin rationale), §6 ("Reads → CLI with structured output"),
  §9 (tool → hledger mapping).
- CLAUDE.md — *The hledger interface (design contract)*, *Quality bar* (property tests on the
  parser).

## Work items

1. Create the adapter module with a narrow public surface (e.g. `Hledger::balance(acct)`,
   `Hledger::register(query)`); the CLI builder + parser are private behind it.
2. Define the `serde` structs for the `balance` and `print`/`register` JSON shapes — **only**
   the fields consumed. Document each with a comment naming the `-O json` field it maps to.
3. Subprocess runner: spawn, capture stdout/stderr, map non-zero exit + stderr to a typed
   `thiserror` error; log the invocation (command + args, **never** journal contents) at debug.
4. Startup version check + `status` surfacing; record the start/warn policy in a doc comment.
5. Wire `get_account_balance` + at least one of `list_transactions`/`get_transaction` as MCP
   tools, reusing M0's `isError` tool-error convention.
6. Record golden fixtures from the pinned binary; add `mise run golden` (regen) +
   assert-in-tests. Fixtures use **synthetic** accounts/amounts only (no PII).
7. Promote the existing [`tests/smoke.rs`](../../../tests/smoke.rs) read step to go through the
   adapter where it makes sense (keep its graceful skip-when-absent behavior).
8. Add `mise run install` / `mise run uninstall`: build the binary, then merge/remove an
   `mcpServers.hledger-mcp` entry in `claude_desktop_config.json` (macOS:
   `~/Library/Application Support/Claude/…`; Linux: `~/.config/Claude/…`). Idempotent via a
   `python3` JSON merge (mirroring `init-settings-local`); print the config path and the
   "restart Claude to load it" reminder. The config lives outside the repo under `$HOME`, so
   no repo-PII concern.

## Testing & coverage

- **Golden tests:** parser vs recorded `1.52 -O json` for `balance` and `print`/`register`.
- **Property tests (`proptest`):** generate synthetic balanced ledgers / amounts, render via a
  test helper, parse, and assert structural round-trip on the read side (the formatter's write-
  side round-trip is M2). Cover decimals, multiple commodities, account paths with spaces.
- **Unit:** CLI builder produces the exact expected argv for representative queries; version-
  check logic over sample `--version` strings (1.52.x match, 1.51/2.0 mismatch).
- **e2e (real hledger, skips when absent):** `get_account_balance` against a temp journal
  returns the expected number end-to-end.
- **Coverage: ≥ 85% lines enforced** (`mise run cov`). Parser + builder should approach full
  coverage; the subprocess error mapping is covered by the e2e + a forced-failure unit test.

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] All hledger interaction goes through the single adapter module (no `hledger`/`Command`
      calls elsewhere — grep-verifiable).
- [ ] Parser ignores unknown JSON fields (a test adds a bogus field and still parses).
- [ ] Golden fixtures recorded from pinned 1.52 and asserted; `mise run golden` regenerates.
- [ ] Startup version check detects a non-1.52 binary and surfaces it (unit + `status`).
- [ ] `get_account_balance` (+ one list/get tool) work e2e against real hledger and are
      callable from a Cowork session.
- [ ] `mise run install` registers the server in Claude Desktop config (idempotently, without
      clobbering other servers) and it appears in Cowork; `mise run uninstall` cleanly removes
      just this entry. Round-trip verified (install → entry present → uninstall → entry gone).
- [ ] Property/round-trip tests on the parser pass.
- [ ] **Mutation testing introduced** (`mise run mutants`): baseline run on the `-O json` parser
      module; surviving mutants reviewed (this milestone establishes the tool; *zero surviving*
      becomes a hard bar from M2). Wire `mise run mutants-diff` into PR CI.
- [ ] No PII in fixtures or tests.

## Exit-criteria review

**Reviewed 2026-06-01 — verdict: done-with-deferrals.**

Gate (run via `mise`, hledger 1.52 from `.env.local`):

- `mise run check` — **green**: `cargo fmt --check`, clippy `-D warnings` (host **and** the
  `aarch64-unknown-linux-gnu` cross target; `x86_64` std not installed locally → skipped, still
  covered by CI), and **62 tests** pass (unit + golden + proptest + stdio integration + e2e).
- `mise run cov` — **89.08% lines ≥ 85%** (hard gate now active). The pure modules are at/near
  100%: `amount.rs` 100%, `cli.rs` 100%, `json.rs` 100%, `mod.rs` 95%, `runner.rs` 92%,
  `server.rs` 89%. (`main.rs` reads 0% — it is exercised only by the *spawned-subprocess* stdio
  e2e, which llvm-cov cannot instrument; `logging.rs` 75% — the macOS os_log layer can't be
  readback-tested, per the apple-log gotcha. Both are accepted; the total clears the bar.)
- `mise run mutants` (scoped to `json.rs` + `amount.rs`) — **24 mutants: 18 caught, 6 unviable,
  0 survived.** Zero survivors already meets the stricter M2 bar.

Checklist:

- [x] `mise run check` green; `mise run cov` ≥ 85% (89.08%).
- [x] All hledger interaction goes through the single adapter module — grep confirms no
      `process::Command` / `Command::new` outside `src/hledger/`.
- [x] Parser ignores unknown JSON fields (`json::tests::ignores_unknown_fields` injects bogus
      fields and still parses).
- [x] Golden fixtures recorded from pinned 1.52 and asserted; `mise run golden` regenerates
      (and scrubs the absolute `-f` path so fixtures stay machine-independent / PII-free).
- [x] Startup version check detects a non-1.52 binary and surfaces it (`mod::tests`
      pin-mismatch unit tests + `status` reports `hledger X.Y (pinned|MISMATCH)`; `main` logs
      a warn on mismatch — warn-and-continue for reads, documented; M2 hard-gates writes).
- [x] `get_account_balance` + `list_transactions` work e2e against real hledger
      (`mod::tests::balance_reads_real_account`, `list_transactions_filters_by_query`) and over
      the wire (`mcp_stdio::read_tools_work_end_to_end_against_fixture_journal`).
- [x] Property / round-trip tests on the parser pass (`json` proptests: exact-decimal
      losslessness + amount-JSON round-trip).
- [x] Mutation testing introduced and wired into PR CI (`mutants-diff` job).
- [x] No PII in fixtures or tests (synthetic accounts/vendors/amounts; absolute paths scrubbed).

**Deferrals (none block M2):**

- **`install` round-trip into a live Cowork — CLOSED 2026-06-01.** Verified end-to-end: `mise
  run install --journal …/tests/fixtures/sample.journal` registered the server; a real Cowork
  session (`client.name = local-agent-mode-hledger-mcp`) called `status`, `get_account_balance`
  (incl. a parent-prefix `assets` → `$93.66` sum), and `list_transactions` (filtered + unfiltered)
  — all `is_error: false` with correct values, no warn/error log lines. Captured in a
  `mise run debug-log` os_log ndjson trace. (`install` also now bakes `LEDGER_FILE` so the
  registered server has a journal; `--journal` flag added.)
- **`register` parser** — only `balance` (→ `get_account_balance`) and `print` (→
  `list_transactions`) are wired; the `register_basic.json` fixture is recorded but its parser
  lands with the domain register/aging tools in **M4**. M1 scope said "start with one or two."

> Verified the golden contract is real: deliberately editing a fixture amount makes the
> corresponding `json::tests` assertion fail, so a future hledger output drift is caught.
