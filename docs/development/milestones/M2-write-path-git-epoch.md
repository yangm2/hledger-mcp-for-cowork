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

## In scope

- **The text formatter** — renders a balanced transaction (date, payee, postings, tags) to
  hledger journal text. Pure, total, and the subject of heavy property testing.
- **The write pipeline** (per [hledger-rearchitecture.md](../hledger-rearchitecture.md) §6
  "Write-path failure & validation semantics"):
  1. Format the candidate transaction text.
  2. Build a **candidate journal** (copy of live + appended txn) in a temp location.
  3. `hledger check --strict` (parse + balanced + `accounts` + `commodities`) on the candidate.
  4. On success: **atomically replace** the live journal (temp + rename) and `git commit`.
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
  `; reverses:<id>`; `update_transaction` = void + re-post. **Never** edit/remove a posted line
  (the explicit divergence from iiAtlas's file+line edits).
- **git integration:** init-if-needed, stage the journal, commit with a structured message
  (synthetic, **no PII**). One commit per validated write.
- **Crash reconciliation at startup:** if a crash left the tree replaced-but-uncommitted,
  reconcile — **commit if `check` passes, else restore to `HEAD`** — so `HEAD` is always a
  `check`-valid journal (the invariant M3's TLA+ `Crash` action models).
- **First write tools:** `post_transaction` (arbitrary balanced postings) + `void_transaction`.
  Domain-specific writers (`receive_invoice`, etc.) are M4 but reuse this pipeline.
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

1. The formatter module (pure); doc-comment the exact journal-text grammar it emits.
2. The write pipeline: candidate build → `check --strict` (via the M1 adapter) → atomic rename
   → `git commit`. Typed errors distinguishing **input error** (correctable tool error) from
   **internal/post-format failure** (loud internal error + attached `check` output).
3. Idempotency: generate/accept `idem:` UUID, pre-append dedup query, write-once guarantee.
4. `void_transaction` (reversing entry) + `update_transaction` (void + re-post); reference the
   target by transaction id/tag, **never by file+line**.
5. git plumbing (shell `git` or `git2`); init-if-needed; crash-reconciliation at startup.
6. Write mutex (single serializing writer).
7. Wire `post_transaction` + `void_transaction` as MCP tools.
8. Extend the [`smoke.rs`](../../../tests/smoke.rs) e2e to exercise the real
   format→check→replace→commit→read-back loop through the production code path.

## Testing & coverage

- **Property / round-trip (the core safety net, per §6):** for arbitrary valid synthetic
  inputs, the formatter output (a) passes `hledger check --strict` **and** (b) round-trips —
  `hledger print -O json` parses back to the same semantic transaction. This is the suspenders
  to `check`'s belt.
- **Adversarial / negative property (the input/internal boundary):** for arbitrary *invalid*
  synthetic inputs (unbalanced postings, missing/empty account or commodity, malformed
  date/amount), the write path **rejects them as correctable tool errors *before* formatting**
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
- [ ] **Adversarial property:** invalid inputs are rejected as tool errors *before* formatting
      (formatter never called, `check` never reached) — generated, not just hand-picked.
- [ ] Idempotency: duplicate `idem:` → exactly one transaction (**C-2**).
- [ ] Corrections are reversing entries; no in-place line edits anywhere (grep + test).
- [ ] Epoch monotonicity holds (**C-3**); crash reconciliation keeps `HEAD` `check`-valid.
- [ ] Version pin is a **hard gate** on the write path (refuses to write against non-1.52).
- [ ] **Mutation testing: zero surviving mutants** in the text-formatter (and the M1 parser)
      modules (`mise run mutants`) — the bar goes hard here; a survivor is closed by tightening
      a test/property, not by excluding the mutant.
- [ ] No PII in journals, commit messages, or fixtures.

## Exit-criteria review

> Fill in when closing M2. Beyond `mise run check`/`cov`, explicitly demonstrate the
> **fail-closed** property (the journal-untouched-on-failure test) and the **exactly-one-
> commit-per-write** property — these are the load-bearing safety claims. Confirm corrections
> never line-edit. Record the verdict; if the reconciliation `Crash` handling is only partially
> tested, defer the remainder explicitly to M3's TLA+ work.
