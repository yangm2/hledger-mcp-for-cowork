# Concurrency Model — minimal + epoch (with TLA+ spec)

> **Extracted for the hledger fork.** Adapted from `gnucash-bindings-mcp` →
> [multi-client.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/multi-client.md)
> (§ M10.4 + § Formal verification). Backend-agnostic; the **epoch is the git commit** of
> the journal (one commit per validated write), not a GnuCash snapshot id. The GnuCash-only
> framing (single-client *sites*, sparsebundle snapshots) is intentionally left behind.

## The problem

A linearizable store does **not** linearize the *agents*. Each LLM client holds a
natural-language snapshot of ledger state in its context — an un-invalidated cache with no
TTL — and it *acts* on that belief. Every individual tool call can be perfectly
linearizable and the *decision* still wrong (approve an overrun, report a superseded
balance).

## The framing that picks the design

Multi-client here is *mostly an artifact of how Claude Desktop / CoWork / Claude Code each
spawn the MCP server* — not a realistic concurrent-mutation workload. So we prioritize
**correctness and simplicity over performance/convenience**: hard serialization is
essentially free when real contention is rare, and any mechanism whose only job is to
reduce contention cost is omitted.

## Design

- **Single serializing writer** — all writes on a journal are serialized; there is no
  container pool (it shells out to `hledger` and appends to the journal). "Multi-client"
  reduces to *multiple connections, serialized execution, where a client may hold a stale
  read.* **Over stdio, "multiple connections" means multiple server processes** — Desktop,
  Cowork, and Claude Code each spawn their own server on the same journal — so serialization
  is two layers: an in-process async mutex (tasks within a server) **plus an advisory file
  lock (`flock`) on a lockfile beside the journal** (processes), both held across the full
  dedup → validate → format → check → swap → commit sequence. HEAD lives in git (shared
  state), so with check-and-commit atomic under the lock the epoch CAS is cross-process-
  correct with no further machinery.
- **Idempotency keys** — a write-once journal tag `; idem:<uuid>`; dedup via
  `hledger print tag:idem=<uuid>` before append. A correctness need even at zero concurrency
  (retries/double-posts).
- **Discipline, not machinery:** immutable transactions; corrections as **reversing
  entries**, never in-place edits; accounts **soft-deleted** (tombstone/closed), never
  hard-deleted. Makes "no lost update / no dangling reference" true by construction, with no
  merge engine.
- **Soft invariants → flags:** budget overrun, overdraft, AP-aging surface in reporting;
  never enforced.
- **Global-epoch CAS (the one optional add, recommended):** reject a *consequential* call
  built on a stale read, forcing a re-ground.

## Epoch = git commit

The epoch is logically the **commit-sequence number**, and the write path already produces
exactly that: every mutation is `format → hledger check --strict → git commit`, so **one
commit per write = a monotonic epoch** (use git `HEAD`, or a commit count). Reads come from
`hledger -O json` against the working tree, which equals `HEAD` because writes commit
atomically — so the epoch a client holds is *by construction* the commit it read (no
data/version skew). Granularity is **whole-journal**: coarse, but the rare false-positive
re-read is acceptable precisely because contention is rare.

**Implementation — the server tracks per-connection last-seen `HEAD`** (over stdio that is
one `write::ConnectionView` per server instance; the multi-connection directory — one shared
`WriterLock`, one view per connection — materializes only with HTTP), *not* a token threaded
through the model (an LLM won't reliably echo it). A read bumps that
connection's last-seen; a consequential call checks last-seen vs current `HEAD`; if behind →
`STALE` → client re-reads, retries. No deadlock is possible (nothing is held), so progress
is always available by re-reading.

Two ordering disciplines the atomic-action spec doesn't show but the implementation must
keep (both are check-then-act races of the kind M2's dedup-inside-the-mutex lesson covers).
Both live structurally on `ConnectionView` — `guarded` (writes) and `grounded_read` (reads)
are the only code paths that touch a connection's last-seen, so a new tool cannot re-derive
the ordering wrongly:

1. **The `STALE` check runs inside the write locks.** Check-then-commit with a gap lets
   another writer commit in between — a successful `Decide` that did *not* observe the
   latest epoch, i.e. a `NoLostDecision` violation the spec can't see because its `Decide`
   is atomic.
2. **Reads sample `HEAD` *before* invoking hledger, never after.** Bumping last-seen to a
   post-read `HEAD` can record an epoch newer than the data the client actually saw (unsafe:
   a stale belief passes the CAS). Sampling before is conservative — worst case a spurious
   `STALE` re-read, which this model already accepts as cheap.

## Record vs decide partition

- **Record** (post / void-as-reversal): append-only, *no epoch check* — cannot corrupt
  (transaction-local invariant: postings balance to zero), idempotency-keyed.
- **Decide** (approve a CO *because there's budget*): epoch-checked — this is where stale
  belief actually bites. The canonical example is `eco_approve` (M5): the agent approves a
  change order based on a read of current budget state, and a stale read could approve a CO
  that busts the budget.

  Note: `pay_invoice` is **Record**, not Decide. The ledger does not release funds — only the
  human can do that (via bank billpay). The agent records a payment the human has made or
  authorized; it never makes the payment decision itself. "Release-because-cash-positive" only
  applies to a hypothetical autonomous payment agent, which is not this workflow.

## Considered and rejected for this usage model

- **MESI coherence** — we cannot force-invalidate or evict an LLM context, so true coherence
  is unenforceable; only the directory/CAS *detection* half survives, which is exactly what
  the epoch keeps.
- **Leases / Rust ownership (`&mut`)** — add the wandering-holder failure mode and
  TTL/heartbeat complexity to guard contention that doesn't occur. The
  *acquire-before-mutate* re-grounding benefit is retained more cheaply by the epoch CAS.
- **CRDT merge engine** — unnecessary with a single authority (nothing to merge), and
  convergence ≠ invariant preservation. Only the CRDT-derived *discipline* (immutability,
  append-only corrections, soft-delete) is kept.

## Tests

```
C-1  A consequential call with last-seen < HEAD is rejected STALE; a fresh read then
     retry succeeds.
C-2  A post with a duplicate `idem:` tag yields exactly one transaction.
C-3  Epoch is monotonic: each validated write makes one commit; reads never move it back.
C-4  A post referencing a soft-deleted (tombstoned) account still resolves — no dangling
     reference.
C-5  Progress/liveness: a stale client always succeeds after re-reading (nothing held → no
     deadlock).
C-6  A soft-invariant violation (over-budget post) succeeds and is surfaced as a flag,
     not rejected.
```

---

## Formal verification (TLA+ / TLC)

The concurrency core (epoch CAS + append-only + idempotency) is small enough to model in
**TLA+** and model-check exhaustively with **TLC** over small bounds — turning the tests
above into checked invariants over *all* interleavings.

**Spec:** `proofs/tla/Ledger.tla` (+ `Ledger.cfg`).

**State:** `epoch` (commit counter); `txns` (grow-only set, each with idempotency key +
referenced accounts); `lastSeen[c]` per connection; `accts` with a `tombstoned` flag.

**Actions:**
- `Read(c)` — `lastSeen[c] := epoch`.
- `Post(c, txn)` — append iff its idempotency key is unused; `epoch := epoch + 1`.
- `Decide(c)` — **guarded**: enabled only if `lastSeen[c] = epoch`; else the client must
  `Read` first (models `STALE`); on success `epoch := epoch + 1`.
- `SoftDelete(a)` — set `tombstoned`, never remove.

**Invariants (TLC):**
- `EpochMonotonic` — `epoch` never decreases.
- `NoLostDecision` — every successful `Decide` observed the latest epoch (its guard held at
  commit, no commit interleaved since the read it relied on) → serializability of guarded
  decisions.
- `IdempotentPosts` — no two `txns` share an idempotency key.
- `AppendOnly` — `txns` is grow-only; no element is mutated or removed.
- `RefIntegrity` — every txn references only accounts that exist (live or tombstoned).

**Properties:**
- `Progress` (weak fairness) — a `STALE`-blocked client can always reach a successful retry;
  holds trivially because no resource is held (no leases ⇒ no circular wait).

**TLC config:** small finite bounds — 2–3 connections, 2–3 accounts, `epoch`/`txns` capped
(e.g. ≤ 4) — enough to exercise the interleavings that matter.

**Optional TLAPS:** a machine-checked proof of `NoLostDecision` (the central safety property)
for unbounded `epoch`; TLC covers the rest. Treat TLAPS as a stretch goal — TLC
model-checking is the gate.

**CI:** a `mise tla` task runs the model check headless via the Rust
[`tla-checker`](https://crates.io/crates/tla-checker) (pinned in mise `[tools]`; no Java
dep), gated alongside the C-x integration tests. The spec stays **TLC-compatible**
(standard syntax + `.cfg`) so `tla2tools.jar` remains a drop-in fallback/cross-check. The
task also runs **spec-mutation sanity checks** — deliberately broken spec variants (drop the
`Decide` guard, allow txn removal, reuse an idem key) that must each be reported as
violations; this proves the gate is load-bearing and validates the young checker itself.

> **Crash safety (add a `Crash` action when implementing):** a crash between the atomic
> journal replace and `git commit` must leave `HEAD` a valid `check`-passing journal —
> startup reconciles by committing if `check` passes, else restoring to `HEAD`. Model this
> as an action whose invariant is "HEAD is always a `check`-valid journal."
