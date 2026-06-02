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

> Fill in when closing M2. Beyond `mise run check`/`cov`, explicitly demonstrate the
> **fail-closed** property (the journal-untouched-on-failure test) and the **exactly-one-
> commit-per-write** property — these are the load-bearing safety claims. Confirm corrections
> never line-edit. Record the verdict; if the reconciliation `Crash` handling is only partially
> tested, defer the remainder explicitly to M3's TLA+ work.
