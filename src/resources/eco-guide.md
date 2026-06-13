# Change-order (ECO) guide

Change orders live in a **parallel expense hierarchy**, `expenses:change orders:{trade}`,
mirroring `expenses:construction:*` — original-contract budget stays clean and ECO cost is
visible per trade and in aggregate. ECO state is a pure account-and-tag machine; every ECO
transaction carries `eco:{id}` and an `eco_event:` marker.

## Lifecycle

1. **`eco_create`** — records the CO as **pending**: posts the amount to
   `expenses:change orders:pending:{trade}` against the vendor's AP account. Committed
   exposure is visible immediately, but *outside* the budget-tracked per-trade account.
   (`pending` is therefore a reserved trade name under `change orders`.)
   Requires `eco` (your CO number, e.g. `"7"` or `"ECO-7"`), `trade`, `vendor`,
   `description`, `date`, `amount`, `commodity`.

2. **`eco_approve`** — the **decide** call: transfers the pending amount into the
   budget-tracked `expenses:change orders:{trade}`. It is **epoch-checked**: if the ledger
   changed since your last read, it fails with STALE — run a read tool, re-evaluate the
   budget (`get_budget_vs_actual` on the change-orders account), then retry. The response
   includes the account's budget-vs-actual line so the approval decision is grounded.

3. **`eco_void`** — reverses the CO's unreversed transactions (the create, and the approval
   if it happened). Append-only, like every correction: reversing entries, never deletes.

## Querying ECO state

- Pending exposure: `get_account_balance` on `expenses:change orders:pending`.
- Approved CO cost: `get_account_balance` on `expenses:change orders` (or per trade).
- One CO's history: `list_transactions` with query `["tag:eco=^{id}$"]`.
- Budget impact: set goals on `expenses:change orders:{trade}` via `budget_set`; approved
  CO cost then shows up in `get_budget_vs_actual`.
