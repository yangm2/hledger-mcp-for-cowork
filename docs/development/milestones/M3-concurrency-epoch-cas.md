# M3 — Concurrency: epoch CAS + record/decide partition

> **Goal.** Add the agent-side correctness layer: per-connection **last-seen HEAD**, the
> **`STALE` epoch CAS** on consequential calls, the **record-vs-decide** partition, and the
> **TLA+/TLC** model that turns the C-x tests into checked invariants over all interleavings.

## Why now / depends on

Depends on **M2** (the git-commit epoch and the write pipeline it guards). This is the third
and final **foundational** milestone — it closes the problem hledger does *not* solve: two LLM
clients acting on stale natural-language beliefs (§3). It is small enough to model-check
exhaustively, so it ships with a formal spec, not just tests.

Unlocks: M4 domain tools can be correctly partitioned into *record* (append-only, no check) vs
*decide* (epoch-checked) as they're built.

## In scope

- **Per-connection last-seen HEAD** — the daemon tracks, per connection, the `HEAD` that
  connection last read (a minimal directory; **not** a token threaded through the model, which
  an LLM won't reliably echo). A read bumps it.
- **Epoch CAS / `STALE`:** a *consequential* ("decide") call is rejected `STALE` when
  `last-seen != HEAD`, forcing the client to re-read and retry. No leases ⇒ nothing held ⇒ no
  deadlock; progress is always available by re-reading.
- **Record vs decide partition:**
  - **Record** (post / void-as-reversal): append-only, **no epoch check** — transaction-local
    balance invariant + idempotency key make it safe at any epoch.
  - **Decide** (approve-because-budget, release-because-cash-positive): **epoch-checked** —
    this is where stale belief bites. M4 tools declare which partition they're in.
- **Soft invariants → flags:** over-budget / overdraft / AP-aging are **computed and surfaced**
  (reporting), **never enforced** (a record call is not rejected for being over budget).
- **Soft-delete (tombstone):** accounts are closed/tombstoned, never hard-deleted; postings to
  tombstoned accounts still resolve (completes the **C-4** behavior stubbed in M2).
- **TLA+/TLC spec:** `proofs/tla/Ledger.tla` (+ `Ledger.cfg`) modeling `epoch`, `txns` (grow-
  only, with idem key + referenced accounts), `lastSeen[c]`, `accts` w/ `tombstoned`, and the
  `Crash` action. Run headless via a **`mise tla`** task (Java + `tla2tools.jar`), gated in CI
  alongside the C-x integration tests.

## Out of scope (and where it lands)

- Multi-*process* / multi-transport concurrency (HTTP serving several clients) → **M6**; M3's
  model is connection-level and transport-agnostic.
- TLAPS machine-checked proof of `NoLostDecision` → **stretch goal**, not a gate (TLC is the
  gate).
- The domain tools that *use* the partition → **M4**.

## Design references

- [concurrency-model.md](../concurrency-model.md) — the whole model: design, epoch=commit,
  record/decide, considered-and-rejected alternatives, **tests C-1…C-6**, and the **full TLA+
  spec** (state, actions, invariants `EpochMonotonic`/`NoLostDecision`/`IdempotentPosts`/
  `AppendOnly`/`RefIntegrity`, `Progress`, and the `Crash` safety note).
- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §7 (epoch on hledger), §14 (tests +
  formal verification carry over).
- CLAUDE.md — *Concurrency model (planned)*.

## Work items

1. Connection registry holding `last-seen HEAD` per connection; read tools bump it.
2. Classify the dispatch surface into **record** vs **decide**; enforce the CAS on decide calls
   (reject `STALE` with a re-read hint when behind).
3. Soft-delete/tombstone for accounts; ensure postings to tombstoned accounts resolve.
4. Soft-invariant computation (over-budget/overdraft/aging) surfaced as **flags** in
   read/report output — never as rejections.
5. Write `proofs/tla/Ledger.tla` + `Ledger.cfg` (small bounds: 2–3 connections, 2–3 accounts,
   `epoch`/`txns` ≤ 4); include the `Crash` action with the "HEAD always `check`-valid"
   invariant.
6. `mise run tla` task (headless TLC) + CI gating; document the Java/`tla2tools.jar` dep
   (already listed in [hledger-rearchitecture.md](../hledger-rearchitecture.md) §17).

## Testing & coverage

Implement the full **C-1…C-6** suite from [concurrency-model.md](../concurrency-model.md):

- **C-1** STALE: a decide call with `last-seen < HEAD` is rejected; a fresh read then retry
  succeeds.
- **C-2** Idempotency (carried from M2; re-assert under the partition).
- **C-3** Epoch monotonic.
- **C-4** Post to a tombstoned account resolves — no dangling reference.
- **C-5** Progress/liveness: a stale client always succeeds after re-reading (nothing held).
- **C-6** A soft-invariant violation (over-budget post) **succeeds** and is surfaced as a flag,
  not rejected.

Plus **TLC** model-checking of `EpochMonotonic`, `NoLostDecision`, `IdempotentPosts`,
`AppendOnly`, `RefIntegrity`, `Progress`, and the `Crash` invariant over all interleavings
within bounds.

- **Coverage: ≥ 85% lines** on the connection/CAS/partition logic (pure state machine — should
  be high).

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] Per-connection last-seen HEAD tracked; reads bump it (test).
- [ ] Decide calls reject `STALE` when behind; record calls never do (**C-1**, partition test).
- [ ] **C-1…C-6 all green.**
- [ ] Soft invariants surface as flags, never rejections (**C-6**).
- [ ] Accounts soft-delete; postings to tombstoned accounts resolve (**C-4**).
- [ ] `mise run tla` model-checks the spec headless and passes all listed invariants +
      `Progress` + `Crash`, and is gated in CI.
- [ ] The epoch interpretation in the spec is git-HEAD (matches the M2 implementation).
- [ ] **Mutation testing: zero surviving mutants** in the epoch-CAS / record-vs-decide state
      machine (`mise run mutants`) — the C-1…C-6 tests must be tight enough to kill them all.

## Exit-criteria review

> Fill in when closing M3. The distinctive check here is the **TLA+/TLC gate**: confirm `mise
> run tla` actually exhausts the bounded state space and that every invariant (esp.
> `NoLostDecision` and the `Crash` HEAD-validity) holds — a spec that passes because it's
> under-constrained is worse than none. Confirm C-1…C-6 map 1:1 to passing tests. Record the
> verdict; TLAPS, if not attempted, is noted as an accepted stretch deferral (not a gap).
