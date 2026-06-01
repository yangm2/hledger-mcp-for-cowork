#!/usr/bin/env bash
# Stop hook — Rust quality gate (strict bar from CLAUDE.md).
# Runs fmt + clippy + tests when Rust sources changed in the working tree.
# Exit 2 + stderr  => blocks the stop and feeds the output back to Claude.
# Exit 0           => allows the stop.
set -uo pipefail

# Read the hook payload; bail out of any retry loop if we already blocked once.
input="$(cat 2>/dev/null || true)"
if printf '%s' "$input" | grep -q '"stop_hook_active": *true'; then
  exit 0
fi

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$PROJECT_DIR" 2>/dev/null || exit 0

# Pick up the per-clone hledger path (written by `mise run init-env`) so the e2e
# smoke test runs for real here instead of skipping.
if [ -f "$PROJECT_DIR/.env.local" ]; then
  set -a; . "$PROJECT_DIR/.env.local"; set +a
fi

# Only gate when Rust sources actually changed (staged, unstaged, or untracked).
if ! git status --porcelain 2>/dev/null | grep -q '\.rs$'; then
  exit 0
fi

fail=0
report=""

run() {
  local label="$1"; shift
  local out
  if ! out="$("$@" 2>&1)"; then
    fail=1
    report+=$'\n'"### ${label} — FAILED (\`$*\`)"$'\n'"${out}"$'\n'
  fi
}

run "rustfmt" cargo fmt --check
run "clippy"  cargo clippy --all-targets --all-features -- -D warnings
if cargo nextest --version >/dev/null 2>&1; then
  run "tests" cargo nextest run
else
  run "tests" cargo test
fi

if [ "$fail" -ne 0 ]; then
  {
    echo "Rust quality gate FAILED — fix before finishing (strict bar, see CLAUDE.md)."
    echo "If you genuinely cannot resolve a failure, stop and tell the user what is failing."
    echo "$report"
  } >&2
  exit 2
fi
exit 0
