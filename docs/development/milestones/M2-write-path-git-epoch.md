# M2 — Write path + git-commit epoch

> **Goal.** Implement the full write lifecycle — **format txn text → build candidate journal →
> `hledger check --strict` → atomically replace the live journal → `git commit`** — with
> append-only corrections (reversing entries) and idempotency tags. One validated write = one
> commit = one epoch.

## Why now / depends on

Depends on **M1** (the adapter, for read-back and for sharing the binary/version plumbing). It
is the second **foundational** milestone: it establishes the *only* way data ever enters the
ledger, and the safety semantics (fail-closed, internal-error-on-our-bug) that the money domain
requires. The **version pin becomes a hard gate here** — never write against a non-1.52 binary.

The **epoch** produced here (the git commit) is the substrate M3's CAS checks against.

Unlocks: M3 (epoch CAS), M4 (domain write tools all funnel through this path).

## Resolved design decisions (M1-review fold-in)

Decided before building M2; they tighten the scope below.

- **git via the `git2` crate, not a `git` subprocess.** A plain Cargo.toml dependency:
  `git2 = { default-features = false, features = ["vendored-libgit2"] }`. `vendored-libgit2`
  makes `libgit2-sys` compile a **bundled** libgit2 from C via the `cc` crate (no system libgit2,
  **no cmake**), statically linked → the single self-contained binary is preserved.
  `#![forbid(unsafe_code)]` still holds — the FFI lives in `libgit2-sys`. **This supersedes
  CLAUDE.md *Stack* ("subprocesses via `tokio::process` … shells out to `hledger` and `git`")
  for the git side** — that line needs updating (hledger still shells out); the **runtime no
  longer needs a `git` binary on PATH**.
  - **No flake change needed:** the only build prerequisite is a C compiler, which the nix
    devShell (`mkShell` → stdenv) and the per-OS CI runners already provide. (There is no local
    cross-lint to satisfy — the retired `check-cross` would have needed a cross C compiler to
    build libgit2 for a foreign target; Linux portability is covered by the **native CI matrix**.)
- **Require pre-declared accounts & commodities (no auto-declare).** `check --strict` rejects an
  undeclared account/commodity (verified against 1.52). So the write path **validates every
  referenced account/commodity against the journal's *declared* set and rejects unknowns as
  correctable _input_ errors, before formatting** — which keeps "a `check` failure can only be
  our bug" true *by construction*. Needs (a) a read of the declared set (`hledger accounts
  --declared`, `hledger commodities` via the M1 adapter) and (b) a **minimal `declare_account` /
  `declare_commodity` write tool** so posting to a new account is usable MCP-only. The full
  chart-of-accounts model (hierarchy, types, tombstones) stays **M4**.
- **Stable transaction identity = an author-stamped `; id:<uuid>` tag.** hledger has no native
  stable id (`tindex` is positional — verified). Every posted transaction is stamped `id:<uuid>`;
  `void_transaction` references it (`; reverses:<id>`). This is **distinct from `idem:<uuid>`**
  (which dedups a *write attempt*). Reference correction targets by this tag — never by file+line
  or `tindex`.
- **Corrections are an append-only audit trail.** `update_transaction` = void (reversing entry)
  **+** re-post = **two** transactions; there is no in-place "amended" view. Intended.
- **Write-path failures log verbatim (no PII scrubbing).** Logs are local and never in the public
  repo, so the loud internal-error log attaches the full `hledger check` output (offending
  postings, candidate path) **unscrubbed** — consistent with CLAUDE.md *write-path discipline*
  ("log loudly with the `check` output attached"). (Still never log the *entire* journal — the
  `check` snippet suffices.)

## In scope

- **The text formatter** — renders a balanced transaction (date, payee, postings, tags) to
  hledger journal text. Pure, total, and the subject of heavy property testing.
- **The write pipeline** (per [hledger-rearchitecture.md](../hledger-rearchitecture.md) §6
  "Write-path failure & validation semantics"):
  0. **Validate input** (balanced; account/commodity **declared** — see decisions; well-formed
     date/amount). Reject failures as correctable tool errors here, *before* formatting.
  1. Format the candidate transaction text (stamping `; id:<uuid>` and the `; idem:<uuid>`).
  2. Build a **candidate journal** (copy of live + appended txn) in a temp file **inside the
     live journal's own directory** (not `$TMPDIR`) so the step-4 rename is same-filesystem
     (a cross-device `rename` fails).
  3. `hledger check --strict` (parse + balanced + `accounts` + `commodities`) on the candidate.
  4. On success: **atomically replace** the live journal (same-dir temp + `rename`) and commit
     via **git2** (`Repository` → stage the journal → `commit`). The new commit's oid **is the
     epoch** (M3 reads it via `repo.head()`).
  5. On failure: **live journal untouched, nothing commits.**
- **Write-path failure semantics:**
  - A `check --strict` failure on text *we* generated is an **internal error** — return it as
    such with the `check` output attached, **logged loudly** — *not* a rephrase-and-retry tool
    error. (*Input* problems are validated and returned as correctable tool errors **before**
    formatting.)
  - **Balance assertions stay out of routine postings** — reserved for the M-future
    reconciliation tools, where a `= $X` failure is meaningful signal (and may route to the M3
    `STALE` path), not a formatter bug.
- **Idempotency:** a write-once `; idem:<uuid>` tag; dedup via `hledger print tag:idem=<uuid>`
  **before** appending. A retried write produces exactly one transaction.
- **Corrections are append-only:** `void_transaction` posts a **reversing** transaction tagged
  `; reverses:<id>` (where `<id>` is the target's author-stamped `id:` tag — never `tindex` or
  file+line); `update_transaction` = void + re-post (**two** transactions, the intended audit
  trail). **Never** edit/remove a posted line (the explicit divergence from iiAtlas's file+line
  edits).
- **Input validation / declarations:** before formatting, read the journal's **declared**
  accounts (`hledger accounts --declared`) and commodities (`hledger commodities`) via the M1
  adapter and reject any referenced-but-undeclared name as a correctable input error.
  `declare_account` / `declare_commodity` write tools (themselves going through the pipeline)
  add the directives; the full chart-of-accounts is M4.
- **git integration (git2):** open-or-init the repo (`Repository::open` / `init`) under the
  journal's directory, stage the journal, commit with a structured **synthetic, no-PII** message.
  One commit per validated write; `HEAD`'s oid is the epoch.
- **Bootstrap a fresh ledger:** if the configured journal does not exist, create it (with the
  baseline `commodity`/`account` declarations needed for `--strict`), `init` the repo, and make
  the initial commit — or refuse with a clear error. Default journal location honors
  `LEDGER_FILE` → `~/.hledger.journal` (not macOS `Application Support`; the ledger is
  user-owned, git-backed data).
- **`status` reports write-readiness** (operator surface): repo present, `HEAD` short oid, dirty
  vs clean, and whether the write pin-gate is open (1.52) or blocking writes.
- **Crash reconciliation at startup:** if a crash left the tree replaced-but-uncommitted,
  reconcile — **commit if `check` passes, else restore to `HEAD`** — so `HEAD` is always a
  `check`-valid journal (the invariant M3's TLA+ `Crash` action models).
- **First write tools:** `post_transaction` (arbitrary balanced postings) + `void_transaction`,
  plus the minimal `declare_account` / `declare_commodity` (prerequisites of the require-pre-
  declare policy). Domain-specific writers (`receive_invoice`, etc.) are M4 but reuse this
  pipeline.
- A serializing **write mutex** so concurrent writes don't interleave (the single-serializing-
  writer invariant; the *connection-aware* epoch CAS is M3).

## Out of scope (and where it lands)

- **Per-connection last-seen HEAD / `STALE` / record-vs-decide partition** → **M3**.
- **TLA+/TLC** model of the concurrency core → **M3** (the `Crash` invariant is implemented
  here but formally modeled there).
- Domain write tools (invoice/pay/fund/vendor/ECO) → **M4**.
- Budget periodic transactions → **M5**.
- `hledger-web` second-writer concerns → **M6**.

## Design references

- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §6 (write lifecycle + failure
  semantics + crash reconciliation), §7 (epoch = git commit, idempotency tag), §9 (tool →
  txn mapping).
- [concurrency-model.md](../concurrency-model.md) — idempotency keys, append-only/reversing-
  entry discipline, the `Crash` safety note; tests **C-2, C-3, C-4** land here.
- CLAUDE.md — *The hledger interface* (Writes / Corrections), *write-path discipline*.

## Work items

1. The formatter module (pure); doc-comment the exact journal-text grammar it emits (incl. the
   `id:`/`idem:` tags).
2. **Record the `check --strict` failure contract first** (the M1 "record the contract before
   coding" lesson): capture exit code + stderr for unbalanced / undeclared-account / bad-date /
   bad-amount into fixtures, then build the error classifier against them.
3. Adapter extension: read the **declared** accounts (`accounts --declared`) + commodities; an
   input validator that rejects undeclared/unbalanced/malformed inputs as correctable tool
   errors *before* the formatter is called.
4. The write pipeline: input-validate → format → candidate build (same-dir temp) → `check
   --strict` (M1 adapter) → atomic `rename` → **git2** commit. Typed errors distinguishing
   **input error** (correctable) from **internal/post-format failure** (loud internal error +
   attached `check` output, logged verbatim).
5. Idempotency: generate/accept `idem:` UUID; pre-append dedup query **inside the write mutex**
   (avoid the retry TOCTOU); write-once guarantee. Stamp a separate `id:<uuid>` per transaction.
6. `void_transaction` (reversing entry referencing the target's `id:` tag) + `update_transaction`
   (void + re-post). **Never** file+line or `tindex`.
7. git plumbing via **`git2`** (Cargo.toml dep, `vendored-libgit2` — no flake/cmake change):
   open-or-init, stage, commit; HEAD oid = epoch; crash-reconciliation at startup.
   `declare_account` / `declare_commodity` + journal bootstrap.
8. Write mutex (single serializing writer).
9. Wire `post_transaction` + `void_transaction` + `declare_account` / `declare_commodity` as MCP
   tools; extend `status` with git/write-readiness.
10. Extend the [`smoke.rs`](../../../tests/smoke.rs) e2e to exercise the real
    declare→format→check→replace→commit→read-back loop through the production code path.

## Testing & coverage

- **Property / round-trip (the core safety net, per §6):** for arbitrary valid synthetic
  inputs, the formatter output (a) passes `hledger check --strict` **and** (b) round-trips —
  `hledger print -O json` parses back to the same semantic transaction. This is the suspenders
  to `check`'s belt.
- **Adversarial / negative property (the input/internal boundary):** for arbitrary *invalid*
  synthetic inputs (unbalanced postings, missing/empty account or commodity, **a referenced
  account/commodity not in the declared set**, malformed date/amount), the write path **rejects
  them as correctable tool errors *before* formatting**
  — the formatter is never invoked and `hledger check` is never reached. This makes "a `check`
  failure can only be our bug" a *checked* property, not an assumption: every `check`-path
  entry is, by construction, post-validation. Pair with a generator for the valid set
  (round-trip above) so the two properties partition the input space — valid inputs always
  format-and-check-clean, invalid inputs never format.
- **C-2 (idempotency):** a post retried with a duplicate `idem:` tag yields exactly one txn.
- **C-3 (epoch monotonic):** each validated write makes exactly one commit; reads never move
  HEAD back.
- **C-4 (ref integrity):** a post referencing a soft-deleted/tombstoned account still resolves
  (no dangling reference) — soft-delete itself may stub here, fully in M4.
- **Failure-path unit/integration:** a deliberately malformed formatter output triggers the
  **internal-error** path (check output attached, live journal untouched, no commit, `idem:`
  not written → retry unblocked).
- **Crash reconciliation:** simulate replaced-but-uncommitted state; assert startup commits a
  valid tree / restores an invalid one to HEAD.
- **Coverage: ≥ 85% lines.** The formatter and pipeline are pure/near-pure and should be high;
  git/crash paths covered by the integration tests above.

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] A write goes format → `check --strict` → atomic-replace → `git commit`, producing exactly
      one commit (e2e against real hledger + git).
- [ ] On `check` failure the **live journal is byte-identical to before** and **no commit**
      occurs (test asserts both).
- [ ] Internal (our-bug) failures return a loud internal error with `check` output attached;
      input errors return correctable tool errors — distinct paths, both tested.
- [ ] **Adversarial property:** invalid inputs — including a **referenced account/commodity not
      in the declared set** — are rejected as tool errors *before* formatting (formatter never
      called, `check` never reached) — generated, not just hand-picked.
- [ ] **Require-pre-declare** works end-to-end: `declare_account` / `declare_commodity` add
      directives through the pipeline; `post_transaction` to a now-declared account succeeds, to
      an undeclared one returns a correctable input error.
- [ ] Idempotency: duplicate `idem:` → exactly one transaction (**C-2**); dedup runs inside the
      write mutex (no retry TOCTOU).
- [ ] Every posted transaction carries a stable `id:<uuid>`; `void_transaction` references it
      (not `tindex`/file+line); corrections are reversing entries; no in-place line edits
      anywhere (grep + test).
- [ ] git is via **`git2`** (no `git` subprocess in the write path; `#![forbid(unsafe_code)]`
      holds); a fresh-ledger **bootstrap** creates journal + declarations + initial commit.
- [ ] Epoch monotonicity holds (**C-3**); crash reconciliation keeps `HEAD` `check`-valid.
- [ ] Version pin is a **hard gate** on the write path (refuses to write against non-1.52);
      `status` reports git/write-readiness.
- [ ] **Mutation testing: zero surviving mutants** in the text-formatter (and the M1 parser)
      modules — the bar goes hard here; a survivor is closed by tightening a test/property, not
      by excluding the mutant. Add the formatter to the `mise run mutants` default file set (the
      task takes file args).
- [ ] No PII in journals, commit messages, or fixtures.

## Exit-criteria review

**Reviewed 2026-06-02 — verdict: done-with-deferrals.**

Gate (via `mise`, hledger 1.52 from `.env.local`):

- `mise run check` — **green**: `cargo fmt --check`, clippy `-D warnings` (host), **102 tests**
  (unit + golden + proptest + stdio integration + write e2e).
- `mise run cov` — **88.04% lines ≥ 85%**. Pure write modules are high: `write/format.rs` 100%,
  `write/validate.rs` 91%, `hledger/amount.rs` 99.6%, `git.rs` 93%. (`main.rs` 0% — spawned-
  subprocess only; `logging.rs` 73% — os_log layer; both accepted, total clears the bar.)
- `mise run mutants` on the pure core (`amount.rs` + `format.rs` + `validate.rs`; `json.rs`
  unchanged from M1's 0) — **0 surviving mutants** (69 tested: 64 caught, 5 unviable). A first
  pass found 9 survivors; closed by removing a redundant digit-check in `Quantity::parse` and
  adding `Add`-scaling + isolated `validate_date`/`validate_tag_key` boundary tests.

Checklist:

- [x] `mise run check` green; `mise run cov` ≥ 85% (88.04%).
- [x] format → `check --strict` → atomic-replace → `git2` commit, exactly one commit per write
      (`smoke::write_path_declare_post_void_round_trip`: declare/post/void each a distinct oid).
- [x] **Fail-closed:** on `check` failure the live journal is **byte-identical** and **no commit**
      (`write::tests::append_and_commit_fails_closed_on_invalid_text` asserts both + temp cleanup;
      `smoke` also asserts byte-identity on the undeclared-account input error).
- [x] Internal vs input errors are distinct, both tested (validate → `Input`; post-format `check`
      rejection → loud `Internal` with attached output, logged verbatim).
- [x] **Require-pre-declare** end-to-end: `declare_*` add directives through the pipeline; posting
      to a declared account succeeds, to an undeclared one returns a correctable `Input` error.
- [x] Idempotency (**C-2**): duplicate `idem:` → exactly one transaction; dedup runs inside the
      write mutex (no TOCTOU — mutex held across dedup→commit).
- [x] Every post carries a stable `id:<uuid>`; `void` references it (not `tindex`/file+line); the
      round-trip e2e confirms the `id` tag survives. **No in-place edits** — grep shows the write
      path only ever appends + atomically replaces; corrections are reversing entries.
- [x] git via **`git2`** (`vendored-libgit2`; no `git` subprocess in the write path);
      `#![forbid(unsafe_code)]` holds. Fresh-ledger **bootstrap** + startup **reconcile** of a
      valid uncommitted journal both tested.
- [x] **§6 round-trip safety net:** `smoke::posted_transactions_round_trip_through_hledger` posts
      representative txns (negatives, a second commodity, a user tag); each passes `check --strict`
      (post only commits if it does) and `print -O json` parses back to the same account+amount+id.
- [x] Version pin is a **hard gate** (`gate()` refuses non-1.52); `status` reports git/write-
      readiness (repo, HEAD short oid, dirty/clean, writes enabled/blocked).
- [x] **Mutation: zero surviving mutants** in the pure core; formatter added to the `mutants`
      default set.
- [x] No PII: commit messages are synthetic (`post id:<uuid>`, `void reverses:…`, `declare …`);
      fixtures/tests use placeholder accounts/amounts.

**Deferrals (none block M3):**

- **Generative (`proptest`) versions of the round-trip & adversarial properties.** Both are
  covered by enumerated representative cases + the mutation-tight pure formatter/validator, but
  not a generator that shells to hledger per case (one subprocess/case is slow/flaky). Hardening
  candidate for later; the pure formatter already has a structural proptest.
- **C-3 epoch monotonicity** is *demonstrated* (distinct oids per write; reads never commit) but
  not a dedicated long-run monotonicity test — **M3** formalizes epoch behavior under the TLA+
  model and CAS.
- **C-4 ref-integrity / tombstoned accounts** — out of scope here (soft-delete is **M4**), as the
  plan allowed.
- **reconcile "restore an *invalid* uncommitted journal to HEAD"** branch: the restore mechanism
  is unit-tested (`git::tests::restore_to_head_…`) and the valid→commit branch is e2e-tested, but
  the invalid→restore path isn't yet exercised end-to-end. Carry into **M3** (the `Crash` action).
- **Pin-mismatch refusal** is implemented + unit-reachable via the `Refused` path, but not e2e-
  tested against a real non-1.52 binary (we only have 1.52 pinned). Low risk.

> Confirmed the two load-bearing safety claims directly: the journal is byte-identical after a
> forced `check` failure (no commit), and each validated write produces exactly one commit. No
> code path edits a posted line in place.

## Post-code-review retrospective

**Reviewed 2026-06-09.** A structured code review of `16e29ab` (7 finder angles × 6 candidates
→ 1-vote empirical verify) found **5 confirmed correctness bugs** and 1 cleanup finding in the
freshly-shipped write path. All were fixed in `60badce`. Final state: 107 tests, 90.57% coverage,
0 surviving mutants on `regex_escape`.

### What the review found

1. **Unanchored-regex dedup (critical).** `tag:idem=<value>` is an unanchored POSIX regex in
   hledger 1.52 — `txn-1` matches `txn-10` (substring), `.` matches any character, metacharacters
   like `(` cause a non-zero exit. Empirically verified against the real binary. All tag-based
   lookups now use `regex_escape` + `^…$` anchoring, plus a Rust-level post-filter as a
   belt-and-suspenders exact-match check. **This was the most dangerous bug:** idempotency silently
   deduplicating the wrong transactions.

2. **`update_transaction` validated after the void (critical).** The original called
   `void_transaction` before validating the replacement. A bad replacement left the original voided
   with nothing posted in its place. Fixed: validate the full replacement first; if it fails, return
   an `Input` error before touching the journal.

3. **`reconcile` used repo-wide dirty check (medium).** `is_dirty()` returns true for any untracked
   file in the repo — including the candidate temp files the write path itself produces. Fixed:
   `is_path_dirty(relpath)` (via `git2::Repository::status_file`) checks only the journal path;
   `sweep_candidate_temps` removes stray temps at startup. A spurious reconcile commit is a
   correctness violation: it advances HEAD without a corresponding validated write.

4. **`void_transaction` used unanchored tag lookup (same root cause as #1).** The `id:` lookup
   for the target transaction had the same regex bug — an id like `abc-1` would match `abc-10`.
   Fixed alongside #1 with `find_by_exact_tag`.

5. **`version()` spawned a subprocess per write op (performance/reliability).** The version pin
   gate is called in every `post_transaction`, `void_transaction`, and `declare_*` call. Fixed:
   `Arc<OnceCell<Version>>` caches the result for the process lifetime; failures are not cached
   (retries). The binary doesn't change under a running process.

6. **Duplicated `declared_sets` query (cleanup).** Both `post_transaction` and `void_transaction`
   called `declared_accounts` + `declared_commodities` inline. Extracted to `declared_sets()` helper.

### Lessons

**hledger query semantics require empirical verification.** The documentation describes
`tag:NAME=VALUE` as a regex match but doesn't flag its POSIX-unanchored-substring behavior, the
wildcard meaning of `.`, or metacharacter error cases. For any query feature that will be used
defensively (dedup, id lookup), **test with real hledger against tricky values** (substring
prefixes, dots, parens) before shipping. The same applies to any hledger query predicate — assume
unanchored until proven otherwise.

**Multi-step operations: validate all prerequisites before executing any side effect.** The
`update_transaction` bug is an instance of a general principle: if an operation has two or more
irreversible side effects (void + post), validate that all later side effects are feasible before
executing the first. The fix (validate replacement before void) follows from this directly.

**Dirty-check granularity must match the invariant.** Using a repo-wide `is_dirty()` for a
single-path invariant (the live journal has uncommitted changes) is wrong by construction — the
repo directory contains other files. Use the narrowest check available (`status_file(relpath)`)
and reason about exactly which state it measures.

**The review-round yield on fresh write-path code was high (5 bugs / ~600 lines).** M1's lesson
("fix latent bugs before wiring into CI") was only partially absorbed. The write path carries the
highest correctness stakes in the system — money-adjacent, append-only, no undo — and benefited
from a dedicated review pass before proceeding to M3. **Standing rule: run a code review on each
milestone's primary module before the exit-criteria sign-off.**

**Mutation testing validates the _verifier_, not just coverage.** The original `find_by_exact_tag`
had no Rust-level post-filter; a mutation that broke the regex would have still passed if hledger
returned a superset. The Rust-level filter closes this gap and is itself mutation-tested. A 0-
survivor result on pure logic (like `regex_escape`) means the test suite is sensitive to every
behavioral nuance of the function — that's the signal mutation testing is trying to give.
