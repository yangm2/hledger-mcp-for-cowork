# Session context — hledger MCP server

This server manages a **construction-project ledger** in an hledger plain-text journal,
backed by git (every validated write is one commit). Read this once at session start; fetch
the detailed guides below only when you need them.

## Conventions (always apply)

- **Corrections are reversing entries, never edits or deletes.** Use `void_transaction` /
  `update_transaction`; the original entry stays in the journal.
- **Amounts are exact decimal strings** (`"8000.00"`), never JSON numbers.
- **Account paths are lowercase, colon-separated** (`expenses:construction:electrical`).
- **Declare before use**: accounts (`declare_account` / `vendor_add`) and commodities
  (`declare_commodity`) must be declared before a transaction can post to them.
- **Vendor names match exactly** across `vendor_add` / `receive_invoice` / `pay_invoice`.
- **Idempotent retries**: pass the same `idem` key when retrying a write to avoid duplicates.
- Soft invariants (overdraft, AP aging, over-budget) are **flags in report output, never
  write rejections**.
- If a write returns a STALE error, your view of the ledger is outdated: run a read tool
  (e.g. `get_account_balance` or `status`), re-evaluate, then retry.

## Tool groups

- **Reads / reports:** `get_account_balance`, `list_transactions`, `get_ap_aging`,
  `get_project_summary`, `get_budget_vs_actual`, `budget_list`, `vendor_list`, `status`.
- **Money writes:** `fund_project`, `receive_invoice`, `pay_invoice`, `post_interest`,
  `post_transaction` (the general form — multi-posting splits, permits).
- **Corrections:** `void_transaction`, `update_transaction` (append-only reversals).
- **Setup / admin:** `declare_account`, `declare_commodity`, `close_account`, `vendor_add`,
  `budget_set`.
- **Change orders:** `eco_create` → `eco_approve` → `eco_void` (see the guide before use).

## Core flows

- **AP flow:** `receive_invoice` (expense / `liabilities:ap:vendor:{vendor}`) →
  `pay_invoice` (AP / `assets:checking`). Check `get_ap_aging` for outstanding balances.
- **ECO flow:** `eco_create` (pending) → `eco_approve` (posts to the budget-tracked
  change-order account; epoch-checked) → `eco_void` (reversing entries).
- **Budget flow:** `budget_set` per account/period → post actuals → `get_budget_vs_actual`
  (over-budget surfaces as a flag).

## Resource index (fetch on demand)

- `ledger://session-context` — this document.
- `ledger://account-guide` — account types, naming, declaration, soft-delete.
- `ledger://vendor-guide` — trade vs professional vendors, permits, GC pass-through.
- `ledger://expected-chart` — the full expected account tree.
- `ledger://budget-guide` — budget rules, `budget_set` semantics, budget vs actual.
- `ledger://eco-guide` — the change-order lifecycle.
- `ledger://vendors` — **live** vendor list with AP balances (dynamic — reads the ledger).

Read the relevant guide before first use of a setup/admin or change-order tool.
