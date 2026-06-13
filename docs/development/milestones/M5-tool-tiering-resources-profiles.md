# M5 — Tool tiering, lazy resources, profiles + budget & ECO

> **Goal.** Make the now-large tool catalog **context-efficient**: three-tier advertising,
> `ledger://` lazy resources (with `server_instructions` → `session-context`), the `--profile`
> CLI filter — and round out the domain with **budget** (periodic txns + `balance --budget`)
> and **ECO / change-order** tools.

## Why now / depends on

Depends on **M4** (a full domain catalog to organize) and **M0** (the `server_instructions`
hook + capability declaration to extend). This is a **feature/ergonomics** milestone: with
~30+ tools, MCP loading every schema at session start costs several thousand tokens (see
[model-options.md](../model-options.md) "Token cost baseline") and invites wrong-tool
selection. Tiering + profiles + resources fix that.

It also declares the **`resources` capability** for the first time (M0 declared `tools` only).

## In scope

- **Three-tier advertising** ([tool-design.md](../tool-design.md) MC-8), mapped to the
  **actual M0–M4 catalog** (tool-design.md's GnuCash-era `book_*` names are the declare/close
  tools here):
  - **Tier 1 operational** — daily read/write/correction + diagnostics, full descriptions,
    always loaded: `get_account_balance`, `list_transactions`, `get_ap_aging`,
    `get_project_summary`, `post_transaction`, `receive_invoice`, `pay_invoice`,
    `fund_project`, `post_interest`, `update_transaction`, `void_transaction`, `status`.
  - **Tier 2 administrative** — **one-line** descriptions (detail in a resource), always
    loaded: `declare_account`, `declare_commodity`, `close_account`, `vendor_add`,
    `vendor_list`, `echo`, plus the new `budget_*` / `eco_*` tools.
  - **Resources** — verbose guides/indices, **zero startup cost, fetched on demand**.
- **`ledger://` resources** (scheme illustrative): `session-context`, `account-guide`,
  `vendor-guide`, `expected-chart`, `budget-guide`, `eco-guide`, `vendors` (dynamic, hits
  hledger). Static resources + `tools/list` answer **without touching hledger** (keep discovery
  off the cold-start path).
- **`server_instructions` → `ledger://session-context`:** the `initialize` result directs the
  client to read `session-context` (tool groups, conventions, resource index) before any tool
  call. (M0 shipped a placeholder string; this wires it to the real resource.)
- **Workflow "flows" — adapt the GnuCash-MCP ones to hledger.** The session-context + guide
  resources are where this server expresses its **skill-like procedural guidance** (skills
  proper are a *client-side* Claude feature, not an MCP primitive — this is the MCP-native
  equivalent: instructions + resources; MCP **prompts** are the possible future home for
  *user-invokable* versions). Port the flows the predecessor encoded in
  `gnucash-bindings-mcp` → `proxy/Sources/gnucash-mcp/Resources/session-context.md` (and the
  `*-guide` resources), translating each to hledger idioms (per [chart-of-accounts.md](../chart-of-accounts.md)
  and the rearchitecture §9 mapping):
  - **AP flow:** `receive_invoice` → `pay_invoice`. GnuCash "DR expense / CR AP-vendor" becomes
    balanced **postings** (`expenses:… $amt` / `liabilities:ap:vendor $-amt`); paying is
    `liabilities:ap:vendor` / `assets:checking`.
  - **ECO flow:** `eco_create (pending)` → `eco_approve` → `eco_void`. "Approve posts a txn and
    adjusts budget" maps to a posting under `expenses:change orders:*` + `; eco:NNN` tag against
    `~` periodic budget rules; **void is a reversing entry** (tag `reverses:`), not a delete.
  - **Reconciliation flow:** `reconcile_account` → `mark_cleared` per matched txn — maps to
    hledger **balance assertions** (the M2 carve-out reserved them for reconciliation) +
    cleared status (`*`); lands with the reconciliation tools (see *Out of scope*).
  - **Conventions to carry over:** read the relevant guide before first Tier-2 use;
    **correction = reversing entry, never delete** (M2/M3 discipline, replacing GnuCash "void");
    amounts as decimal strings; **lowercase colon account paths** (`expenses:construction:electrical`,
    not GnuCash's Title-case `Expenses:Construction:Electrical`); vendor names matched exactly
    across invoice/pay.
- **`--profile` CLI flag** ([tool-design.md](../tool-design.md) MC-10): restricts which tools
  are *advertised*; the full catalog stays compiled-in and **callable** (a tool named from a
  prior session still dispatches). Profiles: `full` (default), `operational`, `readonly`,
  `setup`, `construction`, `reconcile`. `status` reports the active profile.
- **Budget** ([chart-of-accounts.md](../chart-of-accounts.md) Budget, §9): `budget_*` tools
  manage periodic-transaction (`~`) rules; `get_budget_vs_actual` → `hledger balance --budget`.
  Over-budget is a **soft-invariant flag** (M3), never enforced.
  **Open design decision (resolve early in M5):** `~` rules are *directives*, not
  transactions — revising a target by appending a second rule for the same account/period
  **accumulates** rather than replaces, and the reversing-transaction correction idiom doesn't
  apply to directives. Budget revision must reconcile with the append-only discipline some
  other way (candidates: a dedicated `!include`d budget file that is replaced wholesale within
  the epoch-commit pipeline, à la the M3 tombstone precedent for account directives; or
  explicit delta rules). Document the chosen mechanism in `budget-guide`.
- **ECO / change orders:** `eco_*` over the `expenses:change orders:*` parallel hierarchy +
  `; eco:NNN` tags; approve/void via tag/reversal. **ECO approval is a `decide` call** (it acts
  on a budget belief → epoch-checked, M3).
- **M4 deferrals (dated 2026-06-11)** — the two chart-of-accounts special cases
  ([chart-of-accounts.md](../chart-of-accounts.md)) that M4 left untested and undocumented.
  Per the design doc, **no new tool mechanism is required** — the existing surface already
  carries both (`invoice:` is not a reserved tag); the deferred work is *tests proving the
  paths + guide prose encoding the rules*:
  - **Permits** post prepaid via `post_transaction` against `expenses:permits and fees`
    (no AP posting); the jurisdiction is never a vendor; an expediter is a professional
    vendor (`vendor_add`).
  - **GC pass-through** — the GC stays the vendor. *Single-line* pass-throughs use
    `receive_invoice` with the trade account as `expense_account` (works today); *multi-line*
    GC invoices spanning several trades use an explicit multi-posting `post_transaction`
    (one AP posting to the GC, one per trade account, tagged `invoice:REF`).
  Both rules land in the `vendor-guide`/`account-guide` resources authored here.

## Out of scope (and where it lands)

- `tools/listChanged` builder-pattern profile promotion → deferred (note in
  [tool-design.md](../tool-design.md) "Forward note"): only when a client honors
  `listChanged`.
- **`verify_structure` tool** (would consume `ledger://expected-chart` and gate the
  setup→full profile promotion) → deferred with the `listChanged` promotion above; it
  appears in no milestone yet. `expected-chart` still ships in M5, as a guide the model
  reads — not as machine-checked input.
- HTTP/SSE transport (the *networked* per-process profile story) → **M6**.
- `resources/subscribe` / live resource invalidation → out of scope (not needed).
- Reconciliation tools (balance-assertion / `STALE`-meaningful) → later milestone.

## Design references

- [tool-design.md](../tool-design.md) — MC-8 tiering + lazy resources, MC-10 profiles, the
  resource list, `server_instructions` behavior.
- [model-options.md](../model-options.md) — token-cost baseline that motivates tiering/profiles.
- [chart-of-accounts.md](../chart-of-accounts.md) — Budget (periodic txns + `--budget`), change
  orders hierarchy.
- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §9 (`budget_*`, `eco_*`).
- [mcp-protocol-versions.md](../mcp-protocol-versions.md) — declaring the `resources` capability.

## Work items

1. Tag each tool with a tier; `tools/list` renders Tier-1 full + Tier-2 one-line descriptions.
2. Implement `resources/list` + `resources/read` (declare the `resources` capability); ship the
   static `ledger://` guides + the dynamic `vendors` resource (hits hledger). Ensure static
   resources + `tools/list` never touch hledger. **Author each guide as a real `.md` file under
   a `resources/` dir, compiled in via `include_str!`** (CLAUDE.md *Conventions*) — not inline
   strings: keeps the prose diffable/reviewable while staying a single self-contained binary
   with no runtime files. (M0's placeholder `INSTRUCTIONS` literal moves to `include_str!` here
   as it grows into session-context.)
3. Wire `server_instructions` to point at `ledger://session-context`; author that resource by
   **porting the GnuCash-MCP flows** (`gnucash-bindings-mcp` →
   `proxy/Sources/gnucash-mcp/Resources/session-context.md`: AP / ECO / reconciliation flows +
   workflow conventions) into hledger idioms — DR/CR → balanced postings, void → reversing
   entry, Title-case → lowercase colon paths, budget via `~` rules (see the *Workflow "flows"*
   scope item).
4. `--profile` flag → a per-profile advertised-name set filtering `tools/list`; dispatch
   unaffected; `status` reports it. (Optional `--tools a,b,c` ad-hoc filter noted as a stretch.)
5. Budget tools: **first resolve the `~`-rules-vs-append-only design decision** (see the
   *Budget* scope bullet), then manage the rules through the M2 epoch-commit pipeline +
   `get_budget_vs_actual` (via the M1 adapter, `balance --budget`); over-budget flag.
6. ECO tools over `change orders:*` + `eco:` tags; `eco_approve` as a **decide** call.
7. Golden fixtures for `balance --budget` JSON.
8. **M4 deferrals:** e2e tests proving the permit path (`post_transaction`, no AP) and both
   GC pass-through shapes (single-line via `receive_invoice`, multi-line via a multi-posting
   `post_transaction` with the GC as the sole AP vendor); encode both rules in the
   vendor/account guides. No new tool mechanism (see the *M4 deferrals* scope bullet).

## Testing & coverage

- **Unit:** `tools/list` advertises the correct set per profile; dispatch still works for a
  non-advertised tool (the MC-10 invariant).
- **Unit/integration:** `resources/list`/`read` return the static guides; `session-context` is
  served without touching hledger (assert no subprocess spawn on the discovery path).
- **e2e:** budget round-trip — define `~` rules, post actuals, `get_budget_vs_actual` reports
  correct variance; over-budget surfaces as a flag (**C-6** family).
- **e2e:** ECO lifecycle — create/approve (decide → epoch-checked, ties to **C-1**)/void over
  the change-orders hierarchy.
- **e2e:** a multi-line GC pass-through invoice (one `post_transaction`) splits across two
  trade accounts with the GC as the sole AP vendor; a permit posts with no AP account touched.
- **Mutation testing** on the new pure logic: tier/profile filtering sets, budget-variance
  computation, ECO state transitions (the README's M2+ rule).
- **Golden:** `balance --budget` JSON shape.
- **Coverage: ≥ 85% lines.**

## Exit criteria

- [x] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [x] `tools/list` reflects tiers; Tier-2 are one-line, detail in resources.
- [x] `resources` capability declared; `ledger://` guides + dynamic `vendors` served;
      discovery path (static resources + `tools/list`) makes **no** hledger call (asserted).
- [x] `server_instructions` points clients at `ledger://session-context`.
- [x] `--profile` filters advertising only; any tool stays callable; `status` reports profile.
- [x] Budget tools + `get_budget_vs_actual` work e2e; over-budget is a flag, not a rejection.
- [x] ECO tools work e2e; `eco_approve` is epoch-checked (decide).
- [x] New report JSON shapes have golden fixtures.
- [x] **Mutation testing: zero surviving mutants** in the profile-filtering, budget-variance,
      and ECO-transition logic (`mise run mutants`).
- [x] **M4 deferrals closed:** permits post prepaid with no AP; GC pass-through keeps the GC
      as vendor (single-line via `receive_invoice`, multi-line via multi-posting
      `post_transaction`) — both tested and documented in the guides.
- [x] No PII in resources, guides, or fixtures.

## Exit-criteria review

**Reviewed 2026-06-12 — verdict: done** (one carried-forward note under *Deferral* below;
nothing newly deferred).

- **Gate:** `mise run check` green — 218 tests (fmt clean, clippy zero warnings, real-hledger
  e2e incl. two-process contention); `mise run cov` = **94.63% lines** (main.rs 0% as
  documented; total clears 85% with room).
- **Tiering (MC-8):** `catalog::TOOLS` classifies all 24 tools; an exhaustiveness test pins
  the catalog against the router, so an unclassified new tool fails the suite. The
  bogus-binary e2e asserts Tier-2 descriptions are one-liners pointing at their guide while
  Tier-1 keeps full descriptions.
- **Resources:** capability declared; 6 static guides (authored as `.md`, `include_str!`) +
  the dynamic `ledger://vendors`. **Distinctive check (2), cold-start claim:** the discovery
  e2e runs the entire path — initialize, `tools/list`, `resources/list`, every static read —
  against `HLEDGER_EXECUTABLE_PATH=/nonexistent/hledger` and succeeds; only the documented
  dynamic exception (`vendors`) errors, proving nothing else touches the backend. Unknown
  URIs return `-32002`. `server_instructions` (and a unit test) point at
  `ledger://session-context`, which indexes every resource (pinned by test).
- **Profiles (MC-10):** `--profile` filters `tools/list` only; **distinctive check (1)**: the
  e2e under `--profile operational` advertises exactly Tier 1, `status` reports
  `profile: operational`, and the non-advertised `declare_commodity` still dispatches
  (dispatch/`get_tool` come from the full router by construction).
- **Budget:** the "`~`-rules vs append-only" decision is resolved as designed — rules live in
  a wholesale-replaced, journal-`include`d `budget.journal` validated in a scratch dir before
  an atomic swap + **one** commit (the `include` line lands in the same commit on first use);
  startup `reconcile` now covers the budget file too (dirty-granularity lesson, 3rd
  occurrence). The e2e proves: set → list → overspend → `flag over-budget:` (a flag, not a
  rejection) → **re-set replaces rather than accumulates** (the design point) and clears the
  flag. Golden fixture `budget_basic.json` pins the `balance --budget -O json` pair-cell
  shape.
- **ECO + distinctive check (3):** lifecycle e2e — `eco_create` (pending subtree, vendor AP)
  → `eco_approve` (transfer into the budget-tracked account, response carries the
  budget-vs-actual standing) → double-approve rejected → `eco_void` (2 reversing entries,
  subtree zeroed). The cross-process e2e proves `eco_approve` is a genuine **decide**: after
  another process commits, the approve fails **STALE** over the wire; a re-read unblocks it —
  the C-1 epoch-CAS contract rolled forward from M4, now demonstrated end-to-end.
- **Mutants:** the diff run's misses were closed (the `vendors_text` row-match, the
  `budget_pair` null-actual guard — both by the extract-pure-function + match-and-miss-test
  pattern); scoped re-runs over the criterion's logic (catalog `advertised`, budget
  parse/render/upsert/variance, ECO helpers, over-budget flags, `budget_pair`) report zero
  survivors (50 + 7 + 7 caught, 15 unviable, 0 missed).
- **M4 deferrals closed:** the permits/GC-pass-through e2e posts a permit with no AP account
  touched (aging stays empty) and a multi-line GC invoice splitting across two trade accounts
  with the GC as sole AP vendor, then pays it off; both rules are documented in
  `ledger://vendor-guide` (plus account-guide/expected-chart).
- **No PII:** synthetic placeholders throughout (Acme, GC, PaidUp, plumbing, fake amounts).

**Deferral (carried, dated 2026-06-12):** the `reconcile` profile currently advertises the
read-only set — its distinct reconciliation tools (balance assertions, the
`STALE`-meaningful read path) remain with the later reconciliation milestone, as this
document already scoped. `tools/listChanged` promotion and `verify_structure` stay deferred
as scoped above.
