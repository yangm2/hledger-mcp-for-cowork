# M4 — Domain tools + chart of accounts (MC-6)

> **Goal.** Turn the generic read/write core into the **construction-project domain ledger**:
> the MC-6 chart-of-accounts model and the §9 domain tools (invoice / pay / fund / interest /
> vendor / AP-aging / project summary), each correctly partitioned record-vs-decide.

## Why now / depends on

Depends on **M1** (reads), **M2** (writes), **M3** (record/decide partition + soft-delete). This
is the first **feature** milestone — it's what makes this server *this* product rather than a
generic hledger bridge (the differentiator called out in
[hledger-rearchitecture.md](../hledger-rearchitecture.md) §4). Every tool here composes the
foundational pieces; little new infrastructure, much domain modeling.

Unlocks: M5 (tiering/profiles organize *this* catalog; budget + ECO extend it).

## In scope

- **The chart-of-accounts model** from [chart-of-accounts.md](../chart-of-accounts.md):
  - **Vendor model** — each vendor its own `liabilities:ap:{vendor}`; **trade** vendors bill to
    a **shared** `expenses:construction:{trade}`, **professional** vendors get a **dedicated**
    `expenses:{category} — {vendor}`.
  - **Permits** are prepaid (`expenses:permits and fees`, no AP); the jurisdiction is never a
    vendor; an expediter is a professional vendor.
  - **GC pass-through** — the GC stays the vendor; expense splits to the trade account(s).
  - Fixed accounts known up front; `expenses:construction:*` children created from the GC
    line-item budget; **change orders** as a parallel hierarchy.
- **Domain tools (§9 mapping)** — each built on the M2 pipeline / M1 adapter:
  - `receive_invoice` — `expenses:… / liabilities:ap:vendor`, tag `; invoice:REF`. *(record)*
  - `pay_invoice` — `liabilities:ap:vendor / assets:checking`. *(record)*
  - `fund_project` — `assets:checking / equity:owner capital`. *(record)*
  - `post_interest` — `assets:checking / income:interest`. *(record)*
  - `vendor_add` / `vendor_list` — ensure `liabilities:ap:vendor` (+ expense acct per
    trade/professional rule); `vendor_list` = `hledger accounts liabilities:ap`.
  - `get_ap_aging` — query open `liabilities:ap:*` postings bucketed by date (custom; no native
    aging). *(read; surfaces a soft-invariant flag)*
  - `get_project_summary` — composite of `balancesheet` / `incomestatement` / `balance`.
- **Tags as metadata** (`invoice:`, `vendor:`, plus the `idem:`/`reverses:` from M2).
- **`update_transaction` / `void_transaction`** specialized for domain entries (still append-
  only reversal from M2).

## Out of scope (and where it lands)

- **Tool tiering / lazy resources / `--profile`** → **M5** (this milestone produces the full
  catalog that M5 then organizes).
- **Budget** (`budget_*`, periodic `~` txns, `balance --budget`) → **M5**.
- **ECO / change-order tools** (`eco_*`) → **M5** (the *account hierarchy* for change orders is
  modeled here; the *tools* are M5).
- **Reconciliation tools** (with balance assertions, the `STALE`-meaningful path) → later
  milestone (note the M2 carve-out reserving balance assertions for these).
- GnuCash projection / migration → out of the MVP-through-features arc (see §8/§11).

## Design references

- [chart-of-accounts.md](../chart-of-accounts.md) — the full domain account model.
- [hledger-rearchitecture.md](../hledger-rearchitecture.md) §9 (MC-6 tool → hledger mapping),
  §11 (migration, informational here).
- [concurrency-model.md](../concurrency-model.md) — record vs decide (classify each tool).
- CLAUDE.md — *The hledger interface* (corrections, soft-delete).

## Work items

1. Encode the account model (path conventions, fixed accounts, trade-vs-professional vendor
   rule, change-order parallel hierarchy) as typed helpers + an account-resolution layer.
2. Implement each §9 tool atop the M2 write pipeline (writers) / M1 adapter (readers); classify
   each as **record** or **decide** (M3) and document the choice in the tool's doc comment.
3. `vendor_add` enforces the trade vs professional account-creation rule (shared vs dedicated
   expense account).
4. `get_ap_aging` bucketing logic (pure, property-testable) + soft-invariant aging flag.
5. `get_project_summary` composite.
6. Golden fixtures for any new `-O json` shapes consumed (balancesheet/incomestatement) —
   updated in the same change (the adapter-seam rule).

## Testing & coverage

- **Property tests:** AP-aging bucketing over synthetic posting sets; vendor account
  resolution (trade → shared, professional → dedicated) over generated vendor inputs.
- **e2e (real hledger):** the canonical lifecycle — `fund_project` → `receive_invoice` →
  `pay_invoice` → `get_ap_aging`/`get_project_summary` — produces correct balances and commits.
- **Correction path:** `void_transaction` on a domain entry posts a correct reversing txn.
- **Partition tests:** record tools never `STALE`; any decide-classified tool is epoch-checked
  (ties to M3 C-1).
- **Golden tests:** new report JSON shapes.
- **Coverage: ≥ 85% lines.**

## Exit criteria

- [ ] `mise run check` green; **`mise run cov` ≥ 85% lines**.
- [ ] All §9 domain tools implemented and callable from a Cowork session.
- [ ] Vendor model honors trade (shared expense acct) vs professional (dedicated) — tested.
- [ ] Permits post prepaid with no AP; GC pass-through keeps GC as vendor (tested).
- [ ] Full lifecycle (fund → invoice → pay → summary/aging) works e2e with correct balances.
- [ ] Each tool is classified record/decide and behaves accordingly.
- [ ] New report JSON shapes have golden fixtures, updated in the same change.
- [ ] **Mutation testing: zero surviving mutants** in the AP-aging bucketing + vendor-resolution
      logic (`mise run mutants`).
- [ ] No PII — all vendors/accounts/amounts in tests are synthetic placeholders.

## Exit-criteria review

> Fill in when closing M4. Demonstrate the end-to-end domain lifecycle e2e (the fund→invoice→
> pay→summary chain) as the headline proof. Verify the trade-vs-professional vendor rule and
> the permit/pass-through special cases against [chart-of-accounts.md](../chart-of-accounts.md).
> Confirm every new tool declares its record/decide partition. Record the verdict.
