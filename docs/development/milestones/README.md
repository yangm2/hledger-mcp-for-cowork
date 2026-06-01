# Milestones — the executable roadmap

This folder turns the design corpus in [`../`](../) into an ordered, shippable sequence. Each
file is **one milestone** with a single goal, explicit scope, the tests that prove it, and a
**checklist of exit criteria** that must be reviewed (and ticked) before the next milestone
starts.

The design docs say *what* the system is; these say *what to build next and when you're done*.

## Sequencing

```
M0  Walking-skeleton MCP + observability   ← the MVP (Cowork can call our tools)
        │
M1  hledger read adapter (the seam)         ← first real backend; the §16 adapter
        │
M2  Write path + git-commit epoch           ← format → check → atomic-replace → commit
        │
M3  Concurrency: epoch CAS + record/decide  ← per-conn last-seen HEAD; TLA+/TLC
        │
M4  Domain tools + chart of accounts (MC-6) ← invoice / pay / fund / vendor / AP
        │
M5  Tool tiering, resources, profiles        ← MC-8 / MC-10 + budget + ECO
        │
M6  Live GUI + HTTP/Linux + packaging        ← hledger-web, HTTP/SSE seam, container, CI
```

**MVP boundary:** M0 alone satisfies the headline goal — *demonstrate that Claude Cowork can
discover and invoke tools this MCP registers* — and ships the **logging + debug** plumbing so
every later milestone is diagnosable from the first commit (per the explicit ask, and per the
protocol-versions doc's "Cowork registered the connector but never invoked its tools" failure
mode).

**Foundational features:** M1–M3 are the load-bearing core (the adapter seam, the write
lifecycle, the concurrency/correctness model). **Other features:** M4–M6 layer the domain
surface, context-budget ergonomics, and the deferred deployment targets on top.

Milestones are strictly ordered by dependency; don't start one until the prior one's exit
criteria are reviewed. Within a milestone, work items can interleave.

## Definition of done (applies to every milestone)

Inherited from [`../../../CLAUDE.md`](../../../CLAUDE.md) *Quality bar*; restated so each
milestone's exit review is self-contained:

- `mise run check` is green — `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings` (**zero** warnings), and `cargo nextest run` / `cargo test` all pass.
- `#![forbid(unsafe_code)]` holds at the crate root (documented exception only).
- New public items carry doc comments; `cargo doc` builds clean.
- Errors via `thiserror` (lib) / `anyhow` (binary edges); no `unwrap`/`expect` on fallible
  paths outside tests.
- **Coverage ≥ 85% lines** (`mise run cov`) — see the ramp below.
- Anything that parses/formats has **property tests** (`proptest`); the hledger adapter has
  **golden-file** tests updated in the same change that touches it.
- **Pure correctness-critical modules pass mutation testing** (`cargo-mutants`) — zero
  surviving mutants in scope. Periodic / on-demand, *not* a Stop-hook gate (see below).
- **No PII** in code, tests, fixtures, comments, or commits (public repo) — synthetic
  placeholders only.

### Coverage ramp

Coverage is **informational until real code exists** and becomes a **hard gate at M1**:

| Milestone | Coverage expectation |
|---|---|
| M0 | Informational. Logging/transport glue is awkward to unit-test; cover what's pure (negotiation, config), assert the rest via the smoke/e2e path. |
| M1 → | **≥ 85% lines enforced** (`mise run cov --fail-under-lines 85`). The adapter parser and formatters are pure and must be near-fully covered. |

### Mutation testing (assertion strength)

Coverage proves a line *ran*; it cannot prove a test would *fail* if that line were wrong. For
a money ledger that gap matters, so the pure correctness-critical modules are also checked with
**`cargo-mutants`** (`mise run mutants`): it injects small faults (`>`→`>=`, body→`Ok(())`,
`+`→`-`) and reruns the suite — a **surviving mutant is a test gap**. It pairs with the
property tests: `proptest` generates the inputs, `cargo-mutants` proves the assertions on them
are tight.

- **Scope:** the pure, high-stakes modules only — the M1 `-O json` parser, the M2 text
  formatter, the M3 epoch-CAS state machine, the M4 AP-aging / vendor-resolution logic.
  **Excluded:** subprocess / transport / logging glue (mutants there mostly just time out).
- **Cadence:** **not** in the Stop-hook or the `mise run check` gate (it reruns the suite once
  per mutant — far too slow). Run it **periodically / on-demand** (`mise run mutants`), and in
  **PR CI** use `mise run mutants-diff` (`--in-diff`) to mutate only changed lines — fast
  enough to block on.
- **Bar:** introduced at **M1** (first pure parser exists); a **real exit check from M2
  onward** — *zero surviving mutants in the in-scope modules*. A surviving mutant is closed by
  strengthening a test (often the relevant property), not by deleting the mutant.

## The exit-criteria review ritual

The ask is explicit: **review exit criteria at the end of each implementation stage.** Every
milestone ends with an *Exit-criteria review* section — a checklist plus a short written
verdict. The ritual:

1. Run `mise run check` and `mise run cov`; paste/representative-quote the result.
2. Walk the milestone's exit checklist item by item; tick only what's demonstrated (a test,
   a log capture, a command output), not what's "probably fine."
3. For any unticked item: either finish it, or record an explicit, dated **deferral** with a
   reason and the milestone that will close it. No silent gaps.
4. Write a one-paragraph verdict: *done / done-with-deferrals / not-done*. Only `done` or a
   reviewed `done-with-deferrals` unlocks the next milestone.

## Milestone file template

Each milestone file follows this shape:

- **Goal** — one sentence.
- **Why now / depends on** — what it unlocks; what must precede it.
- **In scope / out of scope** — explicit boundaries (out-of-scope items name their milestone).
- **Design references** — links into `../`.
- **Work items** — the concrete build steps.
- **Testing & coverage** — the specific tests that prove it, and the coverage target.
- **Exit criteria** — the checklist gating the next milestone.
- **Exit-criteria review** — the ritual above, filled in when the milestone closes.

## Index

- [M0 — Walking-skeleton MCP + observability](M0-mvp-walking-skeleton.md)
- [M1 — hledger read adapter (the seam)](M1-hledger-read-adapter.md)
- [M2 — Write path + git-commit epoch](M2-write-path-git-epoch.md)
- [M3 — Concurrency: epoch CAS + record/decide](M3-concurrency-epoch-cas.md)
- [M4 — Domain tools + chart of accounts](M4-domain-tools-chart-of-accounts.md)
- [M5 — Tool tiering, resources, profiles](M5-tool-tiering-resources-profiles.md)
- [M6 — Live GUI, HTTP/Linux, packaging](M6-live-gui-http-linux-packaging.md)
