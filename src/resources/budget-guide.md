# Budget guide

Budgets are hledger **periodic-transaction rules** (`~ monthly …`) living in a dedicated
`budget.journal` beside the main journal (included automatically the first time a budget is
set). You never edit that file directly — the tools manage it.

## Why `budget_set` *replaces* (not appends)

Periodic rules are directives: two rules for the same account/period **add up** rather than
replace. So `budget_set` upserts the rule for `(account, period)` and rewrites the budget
file wholesale, in one git commit — revision history lives in git, the main journal stays
append-only.

## Workflow

1. Declare the goal account first (`declare_account` / `vendor_add`) — `budget_set` enforces
   require-pre-declare like any write.
2. `budget_set` per account: `{ "account": "expenses:construction:plumbing",
   "period": "monthly", "amount": "500.00", "commodity": "$" }`.
   Periods: `daily` | `weekly` | `monthly` | `quarterly` | `yearly`.
   Calling it again for the same account+period **replaces** the goal.
3. `budget_list` shows the current rules.
4. `get_budget_vs_actual` reports actual vs goal per account (optionally scoped to one
   account query). Goals are summed across the months in the journal's range.

## Over-budget is a flag, never a rejection

An account whose actual exceeds its goal gets a `flag over-budget:` footer line in
`get_budget_vs_actual` output. Writes are **never** rejected for budget reasons — the flag
is information for the human to act on.

Budget targets conventionally mirror the GC's line-item pricing, one per
`expenses:construction:{trade}` (and `expenses:change orders:{trade}` for approved ECO
scope) — see `ledger://expected-chart`.
