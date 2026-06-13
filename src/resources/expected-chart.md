# Expected chart of accounts (construction project)

Fixed accounts are known up front; `expenses:construction:*` children come from the GC's
line-item budget; `expenses:change orders:*` mirrors construction for ECO tracking.
Vendor names below are placeholders — the live set is `ledger://vendors`.

```
assets
  checking                          ← the project checking account
liabilities
  ap:vendor:{architect}             ← one AP account per vendor (vendor_add)
  ap:vendor:{structural engineer}
  ap:vendor:{mep consultant}
  ap:vendor:{GC}
equity
  owner capital                     ← funding source (fund_project)
  budget                            ← budget-rule balancer (managed by budget_set)
income
  interest                          ← project-account interest (post_interest)
expenses
  professional - {architect}        ← professional: dedicated per-vendor account
  professional - {structural engineer}
  professional - {mep consultant}
  permits and fees                  ← direct payment; no AP vendor (vendor-guide)
  construction                      ← trade parent; children from the GC budget
    demo
    framing
    electrical                      ← shared; any electrical vendor bills here
    plumbing
    hvac
    tile
    finish carpentry
    painting
    contractor fee
    allowances                      ← if the GC uses allowance line items
  change orders                     ← ECO parallel hierarchy (eco-guide)
    pending:{trade}                 ← created-not-yet-approved COs (reserved subtree)
    {trade}                         ← approved CO cost, mirrors construction:*
    new scope                       ← COs adding scope not in the original contract
```

Budget targets are set per `construction:*` (and `change orders:*`) account to match the
GC's line-item pricing — see `ledger://budget-guide`.
