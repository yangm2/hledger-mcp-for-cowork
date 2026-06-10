# M3 — Concurrency: epoch CAS + record/decide partition

> **Goal.** Add the agent-side correctness layer: per-connection **last-seen HEAD**, the
> **`STALE` epoch CAS** on consequential calls, the **record-vs-decide** partition, and the
> **TLA+** model that turns the C-x tests into checked invariants over all interleavings.

## Why now / depends on

Depends on **M2** (the git-commit epoch and the write pipeline it guards). This is the third
and final **foundational** milestone — it closes the problem hledger does *not* solve: two LLM
clients acting on stale natural-language beliefs (§3). It is small enough to model-check
exhaustively, so it ships with a formal spec, not just tests.

Unlocks: M4 domain tools can be correctly partitioned into *record* (append-only, no check) vs
*decide* (epoch-checked) as they're built.

## Resolved design decisions (pre-M3 plan review fold-in)

Decided before building M3 (assessment against the M1/M2 retrospectives); they tighten the
scope below.

- **Cross-process write serialization via an advisory file lock (`flock`).** The day-one
  deployment is stdio: Claude Desktop, Cowork, and Claude Code each spawn **their own server
  process** on the same journal — so the realistic multi-client form is multi-*process*, and
  the M2 in-process `tokio::sync::Mutex` serializes none of it. Fix: an advisory lock on a
  lockfile beside the journal, held for the same dedup → validate → format → check → swap →
  commit sequence the in-process mutex guards (the mutex stays — it serializes tasks within a
  process; the flock serializes processes). HEAD lives in git (shared state), so once
  check-and-commit is atomic under the lock the epoch CAS is cross-process-correct for free.
  What remains for **M6** is multi-*connection*-in-one-process (HTTP) — not write safety.
- **The CAS gate is a pure, unit-testable state machine — no fake MCP tool.** After M2 the
  whole dispatch surface is record-shaped (post/void/update/declare; per
  [concurrency-model.md](../concurrency-model.md), corrections are *record*), so M3 ships the
  **mechanism**, not a decide tool: a `ToolClass` (`Record`/`Decide`) declared per tool and a
  single `guarded_write(class, …)` entry point that samples HEAD and applies the gate iff
  `Decide`, inside the locks. **C-1 is an in-process test** driving `guarded_write` directly
  with `ToolClass::Decide` — the production code path, no MCP surface needed. The first
  decide-classified domain tool (see *Out of scope*) adds the end-to-end C-1.
- **Two ordering disciplines (the M2 dedup-inside-mutex lesson, applied):**
  1. The `STALE` check runs **inside** the write locks — check-then-commit with a gap is a
     TOCTOU that breaks `NoLostDecision` even though the (atomic-`Decide`) spec passes.
  2. Reads sample HEAD **before** invoking hledger, not after. Bumping last-seen *after* the
     read can record an epoch newer than the data the client actually saw (the unsafe
     direction); sample-before is conservative — worst case a spurious `STALE`, which the
     model deems acceptable. The spec's `Read(c)` is atomic, so this is an
     implementation-only hazard: documented at the call site and pinned by a test.
- **Soft invariants: mechanism + overdraft only.** Over-budget needs budget data (M5 periodic
  `~` rules) and AP-aging needs the M4 chart of accounts — neither data source exists in M3.
  M3 builds the **flag mechanism** and the one flag computable today (**overdraft**), which is
  enough to prove C-6. The aging flag lands with M4 (`get_ap_aging`), the over-budget flag
  with M5 (`get_budget_vs_actual`).
- **Model-check with `tla-checker` (Rust, via cargo/mise) instead of Java TLC.** The
  [`tla-checker`](https://crates.io/crates/tla-checker) crate
  ([tla-rs](https://github.com/fabracht/tla-rs)) loads TLC-style `.cfg` files and supports
  everything this spec needs (functions, records, quantifiers, small bounded constants,
  `--check-liveness` for `Progress`); pinning it in mise `[tools]`
  (`"cargo:tla-checker"`, binary `tla`) keeps the toolchain single-language — no Java /
  `tla2tools.jar` dep (supersedes [hledger-rearchitecture.md](../hledger-rearchitecture.md)
  §17's Java line). It is young (2026), so two guards: (a) **the spec stays TLC-compatible**
  (standard syntax + `.cfg`) so falling back to `tla2tools.jar` is a task-file edit, and
  (b) the **spec-mutation sanity checks** below double as checker validation — a checker that
  misses a deliberately broken invariant is caught before it gates anything.
- **Spec-mutation sanity checks (mutation testing for the model).** A spec that passes
  because it's under-constrained is worse than none — so prove the gate is load-bearing:
  deliberately break the spec (drop the `Decide` guard → `NoLostDecision` must fail; allow
  txn removal → `AppendOnly` must fail; reuse an idem key → `IdempotentPosts` must fail) and
  assert the checker reports each violation. Automated as part of `mise run tla`.
- **Test-first (TDD).** The M1 "record the contract first" lesson, applied to a state
  machine: write the C-1…C-6 tests and the TLA+ spec (with its mutation checks) **before**
  the implementation. The pure CAS gate is an ideal TDD target; the spec and the tests are
  two renderings of the same contract and should disagree with the implementation, not with
  each other. For tombstones, record the hledger contract first: capture how
  account-directive tags surface in `accounts --declared` / `-O json` as fixtures before
  coding (and test tag queries against tricky values — the M2 unanchored-regex lesson).

## In scope

- **Per-connection last-seen HEAD** — the server tracks the `HEAD` the connection last read
  (**not** a token threaded through the model, which an LLM won't reliably echo). A read bumps
  it, sampling HEAD **before** the hledger read (see decisions). Over stdio there is exactly
  one connection per server instance, so this is a field on the server struct — per-connection
  by construction; the multi-connection *directory* materializes only with HTTP (M6). Keep the
  seam abstract.
- **Cross-process write lock:** advisory `flock` on a lockfile beside the journal, wrapped
  around the same sequence the in-process write mutex guards (see decisions) — multiple stdio
  server processes on one journal are the day-one multi-client form.
- **Epoch CAS / `STALE`:** a *consequential* ("decide") call is rejected `STALE` when
  `last-seen != HEAD`, forcing the client to re-read and retry. The check runs **inside** the
  write locks (no check-to-commit gap). No leases ⇒ nothing held ⇒ no deadlock; progress is
  always available by re-reading.
- **Record vs decide partition** — shipped as mechanism (`ToolClass` + `guarded_write`, see
  decisions):
  - **Record** (post / void-as-reversal): append-only, **no epoch check** — transaction-local
    balance invariant + idempotency key make it safe at any epoch.
  - **Decide** (approve-because-budget, release-because-cash-positive): **epoch-checked** —
    this is where stale belief bites. M4 tools declare which partition they're in; M3 itself
    classifies the existing surface (all record) and tests the decide path in-process.
- **Soft invariants → flags:** the flag **mechanism**, plus the **overdraft** flag — computed
  and surfaced (reporting), **never enforced** (a record call is not rejected for overdrawing).
  Over-budget → M5, AP-aging → M4 (their data sources; see decisions).
- **Soft-delete (tombstone):** accounts are closed/tombstoned, never hard-deleted; postings to
  tombstoned accounts still resolve (completes the **C-4** behavior stubbed in M2). Contract
  fixtures for account-directive tags recorded first (see decisions).
- **TLA+ spec:** `proofs/tla/Ledger.tla` (+ `Ledger.cfg`) modeling `epoch`, `txns` (grow-
  only, with idem key + referenced accounts), `lastSeen[c]`, `accts` w/ `tombstoned`, and the
  `Crash` action. Model-checked headless via a **`mise tla`** task running **`tla-checker`**
  (Rust; spec kept TLC-compatible — see decisions), including the spec-mutation sanity
  checks, gated in CI alongside the C-x integration tests.
- **Carried M2 deferrals:** an e2e for the crash-reconcile **invalid→restore** branch (the
  `Crash` action's implementation side; M2 only unit-tested the restore mechanism), and a
  dedicated **C-3** epoch-monotonicity test (M2 only demonstrated it incidentally).
- **Operator sweep (the M1 lesson):** `status` surfaces the epoch story (HEAD oid is already
  there; add the connection's last-seen / staleness), and CLAUDE.md's *Concurrency model
  (planned)* section is updated in the same change.

## Out of scope (and where it lands)

- Multi-*connection*-in-one-process / multi-transport concurrency (HTTP serving several
  clients, the per-connection directory) → **M6**. (Multi-*process* write safety is **in**
  scope here via the flock — see decisions — because stdio makes it the day-one reality.)
- TLAPS machine-checked proof of `NoLostDecision` → **stretch goal**, not a gate (the model
  check is the gate).
- The domain tools that *use* the partition → **M4/M5**. The first decide-classified tool
  (M5's `eco_approve`; possibly earlier if an M4 tool such as `pay_invoice` —
  "release-because-cash-positive" — is classified decide) carries the **end-to-end** C-1;
  M3's C-1 is in-process.
- The **AP-aging** flag → **M4** (`get_ap_aging`); the **over-budget** flag → **M5**
  (`get_budget_vs_actual`) — their data sources land there.

## Design references

- [concurrency-model.md](../concurrency-model.md) — the whole model: design, epoch=commit,
  record/decide, considered-and-rejected alternatives, **tests C-1…C-6**, and the **full TLA+
  spec** (state, actions, invariants `EpochMonotonic`/`NoLostDecision`/`IdempotentPosts`/
  `AppendOnly`/`RefIntegrity`, `Progress`, and the `Crash` safety note).
- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §7 (epoch on hledger), §14 (tests +
  formal verification carry over).
- CLAUDE.md — *Concurrency model (planned)*.

## Work items

Ordered test-first (see decisions): items 1–3 produce the contracts (fixtures, spec, failing
tests) before items 4–9 implement against them.

1. **Contract fixtures first:** capture how account-directive tags (the tombstone
   representation) surface in `hledger accounts --declared` / `-O json` output; verify tag
   queries against tricky values (substring prefixes, dots, parens — the M2 regex lesson).
2. **Spec first:** write `proofs/tla/Ledger.tla` + `Ledger.cfg` (small bounds: 2–3
   connections, 2–3 accounts, `epoch`/`txns` ≤ 4), incl. the `Crash` action with the "HEAD
   always `check`-valid" invariant. Keep the syntax TLC-compatible. Add the **spec-mutation
   sanity checks** (broken-spec variants that must fail) and the **`mise run tla`** task
   running `tla-checker` (pinned in mise `[tools]` as `"cargo:tla-checker"`, binary `tla`),
   gated in CI alongside the C-x tests.
3. **Tests first:** write C-1…C-6 as failing tests against the production entry points
   (C-1 in-process via `guarded_write` + `ToolClass::Decide`).
4. **Cross-process flock** around the write sequence (in-process mutex retained); a
   two-process contention test proving writes serialize and the journal/epoch stay coherent.
5. **The pure CAS gate:** `ToolClass` (`Record`/`Decide`), an `Epoch` newtype over the commit
   oid, and the single `guarded_write(class, …)` entry point — gate applied iff `Decide`,
   **inside** the locks; `STALE` rejection carries a re-read hint. (HEAD is sampled fresh per
   call — never cached the way `version()` is; opposite lifecycle.)
6. **Last-seen tracking:** a per-server-instance field (per-connection by construction over
   stdio); read tools bump it, sampling HEAD **before** the hledger read (documented at the
   call site, pinned by a test).
7. Classify the existing dispatch surface (all **record**, incl. corrections); document the
   class in each tool's doc comment.
8. Soft-delete/tombstone for accounts; postings to tombstoned accounts resolve (**C-4**).
9. The soft-invariant **flag mechanism** + the **overdraft** flag (**C-6**).
10. **Carried M2 deferrals:** e2e for crash-reconcile **invalid→restore** (the `Crash`
    action's implementation side); a dedicated **C-3** monotonicity test.
11. **Operator sweep:** `status` reports last-seen/staleness alongside the existing HEAD oid;
    update CLAUDE.md's *Concurrency model (planned)* section in the same change.

## Testing & coverage

Implement the full **C-1…C-6** suite from [concurrency-model.md](../concurrency-model.md),
written **before** the implementation (test-first; see decisions):

- **C-1** STALE: a decide call with `last-seen < HEAD` is rejected; a fresh read then retry
  succeeds. **In-process** through `guarded_write` + `ToolClass::Decide` (the production code
  path; no MCP decide tool exists yet — the e2e variant lands with the first decide-classified
  domain tool, see *Out of scope*).
- **C-2** Idempotency (carried from M2; re-assert under the partition).
- **C-3** Epoch monotonic — now a dedicated test (M2 only demonstrated it incidentally).
- **C-4** Post to a tombstoned account resolves — no dangling reference.
- **C-5** Progress/liveness: a stale client always succeeds after re-reading (nothing held).
- **C-6** A soft-invariant violation (an overdrawing post) **succeeds** and is surfaced as a
  flag, not rejected. (Over-budget/aging variants land in M5/M4 with their data sources.)

Plus:

- **Model-checking** (`tla-checker`) of `EpochMonotonic`, `NoLostDecision`, `IdempotentPosts`,
  `AppendOnly`, `RefIntegrity`, `Progress`, and the `Crash` invariant over all interleavings
  within bounds — **and** the spec-mutation sanity checks (each deliberately broken spec
  variant must be reported as a violation; validates both the spec and the young checker).
- **Two-process contention test** for the flock: concurrent writers from separate processes
  serialize; no interleaved candidate/commit corruption; epochs stay monotonic.
- **Crash-reconcile invalid→restore e2e** (the carried M2 deferral).
- **Read-ordering test:** last-seen is the HEAD sampled *before* the hledger read (a write
  landing mid-read must not mark the connection fresh).
- **Coverage: ≥ 85% lines** on the connection/CAS/partition logic (pure state machine — should
  be high).

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] Per-connection last-seen HEAD tracked; reads bump it, sampling HEAD **before** the
      hledger read (test).
- [ ] Decide calls reject `STALE` when behind; record calls never do (**C-1**, partition
      test, in-process via `guarded_write`); the CAS check runs **inside** the write locks
      (no check-to-commit gap).
- [ ] **Cross-process flock:** two concurrent server processes on one journal serialize their
      writes (contention test); the in-process mutex is retained.
- [ ] **C-1…C-6 all green**, written test-first.
- [ ] Soft-invariant **mechanism** + **overdraft** flag surface as flags, never rejections
      (**C-6**); aging/over-budget flags explicitly deferred to M4/M5.
- [ ] Accounts soft-delete; postings to tombstoned accounts resolve (**C-4**); the
      account-directive-tag contract was fixture-recorded before coding.
- [ ] `mise run tla` model-checks the spec headless via **`tla-checker`** and passes all
      listed invariants + `Progress` + `Crash`; the **spec-mutation sanity checks** each fail
      as expected; gated in CI. The spec stays TLC-compatible (fallback documented).
- [ ] The epoch interpretation in the spec is git-HEAD (matches the M2 implementation).
- [ ] Carried M2 deferrals closed: crash-reconcile **invalid→restore** e2e; dedicated **C-3**
      test.
- [ ] `status` surfaces last-seen/staleness; CLAUDE.md *Concurrency model* section updated.
- [ ] **Mutation testing: zero surviving mutants** in the epoch-CAS / record-vs-decide state
      machine (`mise run mutants`) — the C-1…C-6 tests must be tight enough to kill them all.
- [ ] **Structured code review of the CAS/partition module before sign-off** (the M2 standing
      rule: 5 bugs / ~600 lines found post-ship; this module carries the same stakes).

## Exit-criteria review

> The standing instruction was: confirm `mise run tla` actually exhausts the bounded state
> space and every invariant holds — a spec that passes because it's under-constrained is
> worse than none, which the **spec-mutation checks** exist to rule out; confirm C-1…C-6 map
> 1:1 to passing tests and the contention test ran as *separate processes*.

**Reviewed 2026-06-10 — verdict: done (one documented liveness-tooling deferral).**

Gate (via `mise`, hledger 1.52 from `.env.local`):

- `mise run check` — **green**: fmt, clippy `-D warnings`, **129 tests** (was 107 entering M3).
- `mise run cov` — **90.3% lines ≥ 85%**; `epoch.rs` and `flags.rs` both **100%**.
- `mise run tla` — **green**: tla-checker exhausts **28,352 reachable states / 94,749
  transitions** (bounds: 2 connections × 2 accounts × 3 keys, epoch ≤ 4); all 7 invariants
  hold (`EpochMonotonic`, `NoLostDecision`, `FullyInformedDecisions`, `IdempotentPosts`,
  `AppendOnly`, `RefIntegrity`, `HeadAlwaysValid`). All **5 spec mutations** are caught
  (drop the decide guard / drop key dedup / allow txn removal / commit an invalid reconcile /
  unconditional last-seen bump) — the gate is load-bearing and the young checker demonstrably
  sees violations. Gated in CI (`tla` job).

Checklist notes:

- [x] C-1…C-6 map 1:1 to named tests in `tests/concurrency.rs` (plus the partition half of
      C-1, the read-ordering test, and the masking regression below). All run against real
      hledger through the production `guarded_write` path.
- [x] The contention test (`tests/mcp_stdio.rs::concurrent_writers_from_two_processes_serialize`)
      spawns **two real server processes** on one journal and pipelines writes into both
      before reading responses: 8/8 writes land, 8 distinct epochs, final journal
      `check --strict`-valid.
- [x] **The pre-sign-off code review caught a real design bug** (the M2 standing rule paying
      off): the first implementation bumped `last_seen` to the new HEAD after *every*
      successful write — but a record write teaches the writer nothing about commits that
      interleaved since its last read, so the bump let a later decide pass the CAS
      uninformed. Fixed to a **conditional bump** (only when `last_seen == HEAD-before-op`);
      the spec gained the `behind`/`informed` ground truth and the **`FullyInformedDecisions`**
      invariant, whose `bump` mutation reproduces exactly this bug and fails — the model now
      *proves* the fix. Pinned by
      `tests/concurrency.rs::record_write_does_not_mask_interleaved_commits`.
- [x] Tombstone contract recorded empirically before coding (1.52: a later duplicate
      `account` directive's tag registers; `tag:` queries are unanchored → the adapter query
      is `tag:^tombstoned$`; `accounts` has no `-O json` — line output).
- [x] Lockfile lesson re-applied: the write lockfile made repo-wide `is_dirty()` report the
      ledger dirty forever — `git_status_line` switched to the journal-scoped
      `is_path_dirty` (the M2 granularity lesson, second occurrence).

**Structured review round (2026-06-10, post-sign-off `/code-review`) — 7 findings, all fixed:**

1. **The write gate is now structural, not conventional:** every write op takes a
   `&WriteGuard` proof token whose only mints are `guarded_write` / `guarded_once` (private
   unit field) — an M4 tool bypassing the flock+CAS is a compile error, not a review catch.
2. **The read discipline is now structural:** `grounded_read` on the server owns the
   sample-HEAD-before-hledger ordering and the success-only bump; read tools (and M4's
   additions) cannot get the ordering wrong.
3. **A failed startup reconcile now blocks writes** (`write_block`, surfaced in `status`),
   self-healing: each write attempt retries the reconcile and unblocks on success — a write
   can no longer silently absorb unreconciled foreign content into its commit.
4. `tombstone_account`'s idempotent path renders `(unborn)` instead of an empty oid.
5. A post-op epoch-sample failure no longer fails an already-committed write — it clears
   last-seen (conservative) and warns.
6. The six near-identical write-tool bodies collapsed into one `guarded_tool` helper
   (class + op + render closure).
7. One oid-shortening implementation (`epoch::short_oid`); the server duplicate is gone.

**Deferrals (accepted, none block M4):**

- **`Progress` liveness is not machine-checked by the gate.** tla-checker 0.6.3 cannot verify
  quantified weak fairness (it reports a fairness-violating trace as a property violation).
  The property + `LiveSpec` + `Ledger_live.cfg` are in the spec, TLC-compatible — checkable
  with `tla2tools.jar` on demand; operationally C-5 is pinned by
  `c5_stale_connection_always_progresses_after_reread`. Revisit on a tla-checker upgrade.
- **TLAPS** not attempted (accepted stretch deferral, per plan).
- The flock has **no acquisition timeout** — a hung writer process blocks others until it
  dies (the lock dies with the fd). Contention is rare by design; revisit if M6's daemon
  deployment changes the exposure.
