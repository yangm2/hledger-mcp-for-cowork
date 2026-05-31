# Chart of Accounts (construction-project domain model)

> **Extracted for the hledger fork.** The *domain* model from `gnucash-bindings-mcp` →
> [00-overview.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/00-overview.md)
> (MC-6). It is double-entry account structure, so it transfers as-is; the **hledger
> account-path realization and tool mapping** live in the rearchitecture doc's §9. GnuCash's
> native budget feature is replaced by **hledger periodic transactions + `balance --budget`**.

## Vendor model

Each vendor has its **own AP account** under Liabilities. Expense accounts differ by vendor
type:

| Type | AP account | Expense account | Example |
|---|---|---|---|
| **Trade** (electrical, framing, …) | `liabilities:ap:{vendor}` | shared `expenses:construction:{trade}` | Pacific Crest Electrical → `expenses:construction:electrical` |
| **Professional** (architect, engineer) | `liabilities:ap:{vendor}` | dedicated `expenses:{category} — {vendor}` | Acme Architecture → `expenses:architecture — Acme Architecture` |

- **Trade vendors** bill to a **shared trade expense account** — multiple vendors over the
  project (replacement mid-trade, GC sub pass-through) accumulate into the same trade total.
  Adding a trade vendor uses an existing account; it does not create a new expense account.
- **Professional vendors** each get their **own expense account** (contracts are
  individually named, budgeted, and AP-aged).

## Permits & government fees

Permits are **prepaid** — no AP relationship with the jurisdiction. Post directly against
`expenses:permits and fees`. A permit **expediter** (a hired consultant) is a professional
vendor with their own AP account. **The jurisdiction itself is never a vendor.**

## GC pass-through invoices

When the GC subs out a trade and passes the invoice through, **the GC is still the vendor**
(`liabilities:ap:{GC}`) and the expense splits to the relevant trade account(s).
Single-line pass-throughs use the invoice tool; multi-line GC invoices spanning several
trades use an explicit multi-posting transaction.

## Fixed accounts (known up front)

```
assets
  project checking — First Project Bank
liabilities
  ap:Acme Architecture
  ap:Peak Structural
  ap:Meridian MEP
  ap:Summit HVAC
  ap:[GC name TBD]
equity
  owner capital — First Project Bank
income
  interest income — project account
expenses
  architecture — Acme Architecture        ← professional; dedicated per-vendor
  structural engineering — Peak Structural
  mep consulting — Meridian MEP
  hvac engineering — Summit HVAC
  permits and fees                          ← direct payment; no AP vendor
  construction        ← trade parent; children created from the GC budget
  change orders       ← ECO tracking (parallel hierarchy)
```

## `expenses:construction:*` children

Created during pre-construction when the GC delivers their line-item budget — each line item
becomes a sub-account; that structure simultaneously defines the expense hierarchy and the
budget targets:

```
construction:demo
construction:framing
construction:electrical        ← shared; any electrical vendor bills here
construction:plumbing
construction:hvac
construction:tile
construction:finish carpentry
construction:painting
… (GC-defined)
construction:contractor fee
construction:allowances        ← if the GC uses allowance line items
```

## Change Orders (ECO)

A **parallel hierarchy** mirroring construction, so original-contract budget stays clean and
ECO cost is visible independently and in aggregate:

```
change orders:demo
change orders:electrical
… (mirrors construction:*)
change orders:new scope        ← COs adding scope not in the original contract
```

## Budget

Targets are set per `construction:*` (and `change orders:*`) account to match the GC's
line-item pricing. On hledger: model the budget as **periodic transactions** (`~` rules) and
report actual-vs-budget with **`hledger balance --budget`** — the live equivalent of querying
a native budget. (See the rearchitecture doc's §9 mapping and §16 for `-O json` parsing.)
