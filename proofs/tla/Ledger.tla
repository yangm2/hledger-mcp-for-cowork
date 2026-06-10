---------------------------- MODULE Ledger ----------------------------
(***************************************************************************)
(* The M3 concurrency core of hledger-mcp-for-cowork, model-checked over   *)
(* all interleavings within small bounds (see Ledger.cfg).                 *)
(*                                                                         *)
(* The epoch IS the git commit: one validated write = one commit. `epoch`  *)
(* here is a commit *counter*; the implementation uses the HEAD oid (an    *)
(* equality check, exactly as modeled). Connections map to server          *)
(* processes/instances (stdio multi-client = multi-process); their writes  *)
(* are serialized by the in-process mutex + cross-process flock, which is  *)
(* why actions here are atomic.                                            *)
(*                                                                         *)
(* History variables (epochHigh, posted, decided, behind, headOk) exist    *)
(* only to witness the invariants; they add no behavior.                   *)
(*                                                                         *)
(* `\* MUT:` markers are anchors for the spec-mutation sanity checks       *)
(* (proofs/tla/mutate.py): each named mutation must make its invariant     *)
(* fail, proving both that the spec constrains anything at all and that    *)
(* the checker can see a violation. Kept TLC-compatible: `tla2tools.jar`   *)
(* is a drop-in fallback for the `tla` binary.                             *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Connections,   \* client connections (= server processes over stdio)
  Accounts,      \* declared account names
  Keys,          \* idempotency keys
  MaxEpoch       \* state-space bound on the commit counter

VARIABLES
  epoch,       \* the commit counter: git HEAD
  epochHigh,   \* history: highest epoch ever reached (EpochMonotonic witness)
  txns,        \* grow-only set of [key, acct] records (the journal)
  posted,      \* history: number of successful posts (AppendOnly witness)
  lastSeen,    \* per-connection: the epoch its view is current to; -1 = never read
  behind,      \* history, per-connection: TRUE iff some commit it has not observed
               \* exists (ground truth for FullyInformedDecisions)
  tombstoned,  \* soft-deleted accounts (a flag — never removed from Accounts)
  decided,     \* history: [seen, at, informed] per successful decide
  dirty,       \* crash state of the working tree: "none" | "valid" | "invalid"
  headOk       \* whether the journal at HEAD is check-valid (HeadAlwaysValid witness)

vars == <<epoch, epochHigh, txns, posted, lastSeen, behind, tombstoned, decided,
          dirty, headOk>>

UsedKeys == { t.key : t \in txns }

\* The epoch CAS guard on decide calls.  \* MUT:guard
DecideGuard(c) == lastSeen[c] = epoch

\* The idempotency-key dedup on record posts.  \* MUT:fresh
FreshKey(k) == k \notin UsedKeys

\* Startup reconciliation commits the dirty tree only when it is check-valid.  \* MUT:reconcile
CommitDirty(d) == d = "valid"

\* The post-write last-seen bump: a writer's view stays current only when its write
\* landed on top of a view that was already current — its own append teaches it
\* nothing about commits it never read. The unconditional variant (the `bump`
\* mutation) lets a record write mask an interleaved commit, and a later decide
\* then passes the CAS uninformed → FullyInformedDecisions fails.  \* MUT:bump
SeenAfterWrite(c) == IF lastSeen[c] = epoch THEN epoch + 1 ELSE lastSeen[c]

\* Every connection except the author falls behind on a new commit; the author's
\* behind-ness is unchanged (it may already have been behind).
OthersFallBehind(author) == [c \in Connections |-> IF c = author THEN behind[c] ELSE TRUE]

Bump(e) == IF e > epochHigh THEN e ELSE epochHigh

\* A crashed server reconciles (under the flock) before serving anything.
Serving == dirty = "none"

Init ==
  /\ epoch = 0 /\ epochHigh = 0
  /\ txns = {} /\ posted = 0
  /\ lastSeen = [c \in Connections |-> -1]
  /\ behind = [c \in Connections |-> FALSE]
  /\ tombstoned = {}
  /\ decided = {}
  /\ dirty = "none"
  /\ headOk = TRUE

\* A read: hledger output + the pre-read HEAD sample → lastSeen; the connection has
\* now observed everything up to the current epoch. (The implementation samples HEAD
\* *before* the hledger read; the non-atomic window is covered by the conservative-
\* direction test in tests/concurrency.rs.)
Read(c) ==
  /\ Serving
  /\ lastSeen' = [lastSeen EXCEPT ![c] = epoch]
  /\ behind' = [behind EXCEPT ![c] = FALSE]
  /\ UNCHANGED <<epoch, epochHigh, txns, posted, tombstoned, decided, dirty, headOk>>

\* A RECORD write (post / void-as-reversal / declare): append-only, idempotency-
\* keyed, NO epoch check. Tombstoned accounts still resolve (C-4): `a` ranges
\* over ALL of Accounts, tombstoned or not.
Post(c, k, a) ==
  /\ Serving
  /\ epoch < MaxEpoch
  /\ FreshKey(k)
  /\ txns' = txns \cup { [key |-> k, acct |-> a] }
  /\ posted' = posted + 1
  /\ epoch' = epoch + 1
  /\ epochHigh' = Bump(epoch + 1)
  /\ headOk' = TRUE  \* only a check-valid candidate is ever swapped in + committed
  /\ lastSeen' = [lastSeen EXCEPT ![c] = SeenAfterWrite(c)]
  /\ behind' = OthersFallBehind(c)
  /\ UNCHANGED <<tombstoned, decided, dirty>>

\* A DECIDE write: epoch-checked (the CAS). A stale connection is rejected —
\* modeled as the action not being enabled — and must Read first (C-1); since
\* nothing is held, the re-read → retry path is always available (C-5).
\* `informed` records the ground truth at fire time: had this connection actually
\* observed every commit? With the real guard + conditional bump it always has.
Decide(c) ==
  /\ Serving
  /\ epoch < MaxEpoch
  /\ DecideGuard(c)
  /\ decided' = decided \cup
       { [seen |-> lastSeen[c], at |-> epoch, informed |-> ~behind[c]] }
  /\ epoch' = epoch + 1
  /\ epochHigh' = Bump(epoch + 1)
  /\ headOk' = TRUE
  /\ lastSeen' = [lastSeen EXCEPT ![c] = SeenAfterWrite(c)]
  /\ behind' = OthersFallBehind(c)
  /\ UNCHANGED <<txns, posted, tombstoned, dirty>>

\* Soft-delete: tombstoning is itself a record write (one commit). The account is
\* flagged, never removed — references stay resolvable forever.
SoftDelete(c, a) ==
  /\ Serving
  /\ epoch < MaxEpoch
  /\ a \notin tombstoned
  /\ tombstoned' = tombstoned \cup {a}
  /\ epoch' = epoch + 1
  /\ epochHigh' = Bump(epoch + 1)
  /\ headOk' = TRUE
  /\ lastSeen' = [lastSeen EXCEPT ![c] = SeenAfterWrite(c)]
  /\ behind' = OthersFallBehind(c)
  /\ UNCHANGED <<txns, posted, decided, dirty>>

\* A crash between the atomic journal swap and the git commit leaves the
\* working tree dirty: "valid" is our own swapped-in candidate (always
\* check-valid by construction); "invalid" models adversarial/hand-edited
\* content found at startup. HEAD itself is untouched by the crash.
Crash ==
  /\ Serving
  /\ epoch < MaxEpoch
  /\ dirty' \in {"valid", "invalid"}
  /\ UNCHANGED <<epoch, epochHigh, txns, posted, lastSeen, behind, tombstoned,
                 decided, headOk>>

\* Startup reconciliation (runs under the flock, before serving): commit the
\* dirty tree iff it is check-valid (one epoch), else restore to HEAD (epoch
\* unchanged). Either way HEAD remains a check-valid journal. A reconcile commit
\* has no authoring connection — every connection falls behind it.
Reconcile ==
  /\ dirty /= "none"
  /\ dirty' = "none"
  /\ IF CommitDirty(dirty)
       THEN /\ epoch' = epoch + 1
            /\ epochHigh' = Bump(epoch + 1)
            /\ headOk' = (dirty = "valid")
            /\ behind' = [c \in Connections |-> TRUE]
       ELSE /\ epoch' = epoch
            /\ epochHigh' = epochHigh
            /\ headOk' = headOk
            /\ behind' = behind
  /\ UNCHANGED <<txns, posted, lastSeen, tombstoned, decided>>

\* Anchor for the AppendOnly mutation: never enabled in the real spec.  \* MUT:vanish
Vanish == FALSE

Next ==
  \/ \E c \in Connections : Read(c)
  \/ \E c \in Connections, k \in Keys, a \in Accounts : Post(c, k, a)
  \/ \E c \in Connections : Decide(c)
  \/ \E c \in Connections, a \in Accounts : SoftDelete(c, a)
  \/ Crash
  \/ Reconcile
  \/ Vanish

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants — the C-x tests as checked properties over ALL interleavings *)

\* C-3: the epoch never decreases (epochHigh trails the max ever reached).
EpochMonotonic == epoch = epochHigh

\* C-1: every successful decide held the guard at commit time (no commit
\* interleaved between the epoch it relied on and the commit).
NoLostDecision == \A d \in decided : d.seen = d.at

\* C-1, strengthened: every successful decide was *fully informed* — the
\* connection had actually observed every commit (not merely holding a
\* lastSeen that claims so). This is what catches an unsound lastSeen bump:
\* a record write masking an interleaved commit makes a later decide pass the
\* CAS while behind — see the `bump` mutation.
FullyInformedDecisions == \A d \in decided : d.informed

\* C-2: no two recorded transactions share an idempotency key.
IdempotentPosts == \A t1 \in txns, t2 \in txns : (t1.key = t2.key) => (t1 = t2)

\* The journal is grow-only: nothing recorded is ever mutated or removed.
AppendOnly == Cardinality(txns) = posted

\* C-4: every transaction references an account that (still) exists — live or
\* tombstoned. (Accounts are never hard-deleted, so this can only fail if a
\* mutation lets references outlive accounts.)
RefIntegrity == \A t \in txns : t.acct \in Accounts

\* Crash safety: HEAD always points at a check-valid journal.
HeadAlwaysValid == headOk

----------------------------------------------------------------------------
(* Liveness (C-5): nothing is held, so a stale connection can always reach   *)
(* freshness by re-reading — under weak fairness of every connection's own   *)
(* Read, staleness always resolves.                                          *)
(*                                                                           *)
(* NOT part of the `mise run tla` gate: tla-checker (0.6.3) cannot yet       *)
(* verify quantified/parameterized weak fairness (it mis-handles the         *)
(* fairness side-conditions, reporting a fairness-violating trace as a       *)
(* property violation). Check with real TLC (`tla2tools.jar`, Ledger_live    *)
(* .cfg) when wanted; operationally C-5 is pinned by                         *)
(* tests/concurrency.rs::c5_stale_connection_always_progresses_after_reread. *)

Fairness == \A c \in Connections : WF_vars(Read(c))

LiveSpec == Init /\ [][Next]_vars /\ Fairness

Progress == \A c \in Connections : (lastSeen[c] /= epoch) ~> (lastSeen[c] = epoch)

============================================================================
