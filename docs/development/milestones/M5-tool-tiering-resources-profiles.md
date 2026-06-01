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

- **Three-tier advertising** ([tool-design.md](../tool-design.md) MC-8):
  - **Tier 1 operational** — daily read/write/correction tools, full descriptions, always
    loaded.
  - **Tier 2 administrative** — `book_*` / `vendor_*` / `budget_*` / `eco_*`, **one-line**
    descriptions (detail in a resource), always loaded.
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
- **ECO / change orders:** `eco_*` over the `expenses:change orders:*` parallel hierarchy +
  `; eco:NNN` tags; approve/void via tag/reversal. **ECO approval is a `decide` call** (it acts
  on a budget belief → epoch-checked, M3).

## Out of scope (and where it lands)

- `tools/listChanged` builder-pattern profile promotion → deferred (note in
  [tool-design.md](../tool-design.md) "Forward note"): only when a client honors
  `listChanged`.
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
5. Budget tools: manage `~` periodic rules in the journal (via the M2 write pipeline) +
   `get_budget_vs_actual` (via the M1 adapter, `balance --budget`); over-budget flag.
6. ECO tools over `change orders:*` + `eco:` tags; `eco_approve` as a **decide** call.
7. Golden fixtures for `balance --budget` JSON.

## Testing & coverage

- **Unit:** `tools/list` advertises the correct set per profile; dispatch still works for a
  non-advertised tool (the MC-10 invariant).
- **Unit/integration:** `resources/list`/`read` return the static guides; `session-context` is
  served without touching hledger (assert no subprocess spawn on the discovery path).
- **e2e:** budget round-trip — define `~` rules, post actuals, `get_budget_vs_actual` reports
  correct variance; over-budget surfaces as a flag (**C-6** family).
- **e2e:** ECO lifecycle — create/approve (decide → epoch-checked, ties to **C-1**)/void over
  the change-orders hierarchy.
- **Golden:** `balance --budget` JSON shape.
- **Coverage: ≥ 85% lines.**

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] `tools/list` reflects tiers; Tier-2 are one-line, detail in resources.
- [ ] `resources` capability declared; `ledger://` guides + dynamic `vendors` served;
      discovery path (static resources + `tools/list`) makes **no** hledger call (asserted).
- [ ] `server_instructions` points clients at `ledger://session-context`.
- [ ] `--profile` filters advertising only; any tool stays callable; `status` reports profile.
- [ ] Budget tools + `get_budget_vs_actual` work e2e; over-budget is a flag, not a rejection.
- [ ] ECO tools work e2e; `eco_approve` is epoch-checked (decide).
- [ ] New report JSON shapes have golden fixtures.
- [ ] No PII in resources, guides, or fixtures.

## Exit-criteria review

> Fill in when closing M5. The distinctive checks: (1) a tool **not** advertised under the
> active profile still **dispatches** when named (MC-10 invariant); (2) the discovery path
> spawns **no** hledger subprocess (cold-start cost claim); (3) `eco_approve` correctly
> rejects `STALE` on a stale budget belief. Record the verdict.
