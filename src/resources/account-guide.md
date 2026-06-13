# Account guide

## Naming

Lowercase, colon-separated paths under the five standard roots: `assets`, `liabilities`,
`equity`, `income`, `expenses`. Examples: `assets:checking`,
`liabilities:ap:vendor:Acme`, `expenses:construction:electrical`.

## Declaration (require-pre-declare)

Every account must be declared before a transaction can post to it — `declare_account` for
plain accounts, `vendor_add` for vendors (declares the AP account and the expense account
together). Commodities likewise: `declare_commodity` (e.g. `$`, default 2 decimal places).
A posting to an undeclared account is rejected with a correctable error naming the account.

## Soft delete (tombstoning)

Accounts are **never hard-deleted**. `close_account` tombstones the account: it stays
declared, its history remains valid, and postings to it still resolve. There is no
"reopen" — declare a new account if needed.

## Key fixed accounts

- `assets:checking` — the project checking account (funding, payments, interest).
- `equity:owner capital` — owner funding source (`fund_project` balancer).
- `income:interest` — interest income (`post_interest`).
- `liabilities:ap:vendor:{vendor}` — one AP account per vendor (via `vendor_add`).
- `expenses:construction:{trade}` — shared per-trade expense accounts.
- `expenses:permits and fees` — permits, posted directly with **no AP** (see vendor-guide).
- `expenses:change orders:*` — the ECO parallel hierarchy (see eco-guide).
- `equity:budget` — the budget-rule balancing account (managed by `budget_set`).

See `ledger://expected-chart` for the full tree.
