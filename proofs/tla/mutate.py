#!/usr/bin/env python3
"""Spec-mutation sanity checks for proofs/tla/Ledger.tla (M3).

A spec that passes because it is under-constrained is worse than none. Each mutation
below deliberately breaks one guard/discipline in the spec; the model checker MUST
report a violation of the expected invariant(s). This proves (a) the invariants are
load-bearing and (b) the checker itself can see a violation — it doubles as a
validation of the (young) `tla-checker` binary.

Run via `mise run tla` (after the real spec passes). The checker binary is resolved
from $TLA_BIN, defaulting to `tla` on PATH.

Anchors: each mutation replaces one exact line of the spec (marked `\\* MUT:<name>`
in Ledger.tla). The script fails loudly if an anchor line is missing — the spec and
this script must move together.
"""

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
SPEC = HERE / "Ledger.tla"
CFG = HERE / "Ledger.cfg"
TLA_BIN = os.environ.get("TLA_BIN", "tla")

# name -> (exact line to replace, replacement line, invariants that may be reported —
# at least one must be). Multiple acceptable invariants happen when one broken guard
# can be caught first by either of two witnesses (e.g. FreshKey=TRUE lets a key be
# reused with the same account — caught by AppendOnly via the posted counter — or a
# different account — caught by IdempotentPosts; the checker reports whichever state
# it reaches first).
MUTATIONS = {
    "guard": (
        "DecideGuard(c) == lastSeen[c] = epoch",
        "DecideGuard(c) == TRUE",
        {"NoLostDecision"},
    ),
    "fresh": (
        "FreshKey(k) == k \\notin UsedKeys",
        "FreshKey(k) == TRUE",
        {"IdempotentPosts", "AppendOnly"},
    ),
    "vanish": (
        "Vanish == FALSE",
        "Vanish == \\E t \\in txns : txns' = txns \\ {t} /\\ UNCHANGED "
        "<<epoch, epochHigh, posted, lastSeen, behind, tombstoned, decided, dirty, headOk>>",
        {"AppendOnly"},
    ),
    # The bug the M3 code review actually caught: an unconditional post-write
    # last-seen bump lets a record write mask an interleaved commit, so a later
    # decide passes the CAS while the connection never observed that commit.
    "bump": (
        "SeenAfterWrite(c) == IF lastSeen[c] = epoch THEN epoch + 1 ELSE lastSeen[c]",
        "SeenAfterWrite(c) == epoch + 1",
        {"FullyInformedDecisions"},
    ),
    "reconcile": (
        'CommitDirty(d) == d = "valid"',
        'CommitDirty(d) == d /= "none"',
        {"HeadAlwaysValid"},
    ),
}


def run_checker(spec_dir: pathlib.Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [TLA_BIN, str(spec_dir / "Ledger.tla"), "--config", str(spec_dir / "Ledger.cfg")],
        capture_output=True,
        text=True,
        timeout=600,
    )


def main() -> int:
    spec_text = SPEC.read_text()
    failures = []
    for name, (old, new, expected) in MUTATIONS.items():
        if spec_text.count(old) != 1:
            print(f"FAIL [{name}]: anchor line not found exactly once in Ledger.tla:\n  {old}")
            failures.append(name)
            continue
        mutated = spec_text.replace(old, new)
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            (tmp_path / "Ledger.tla").write_text(mutated)
            shutil.copy(CFG, tmp_path / "Ledger.cfg")
            result = run_checker(tmp_path)
        output = result.stdout + result.stderr
        violated = result.returncode != 0 and any(inv in output for inv in sorted(expected))
        if violated:
            hit = [inv for inv in sorted(expected) if inv in output]
            print(f"ok   [{name}]: checker caught the broken spec (violated: {', '.join(hit)})")
        else:
            print(
                f"FAIL [{name}]: mutation was NOT caught "
                f"(exit {result.returncode}, expected one of {sorted(expected)}).\n"
                f"--- checker output ---\n{output}"
            )
            failures.append(name)
    if failures:
        print(f"\nspec-mutation check FAILED: {failures} — the spec (or checker) is not load-bearing")
        return 1
    print(f"\nspec-mutation check passed: all {len(MUTATIONS)} broken-spec variants were caught")
    return 0


if __name__ == "__main__":
    sys.exit(main())
