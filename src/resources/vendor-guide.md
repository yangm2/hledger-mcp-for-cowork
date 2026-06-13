# Vendor guide

Every vendor gets its **own AP account**, `liabilities:ap:vendor:{vendor}`. The expense
account depends on the vendor type — `vendor_add` enforces this:

| Type | Expense account | Notes |
|---|---|---|
| **trade** (electrical, framing, plumbing, …) | shared `expenses:construction:{trade}` | multiple vendors over the project accumulate into the same trade total; pass `trade` to `vendor_add` |
| **professional** (architect, engineer, …) | dedicated `expenses:professional - {vendor}` | individually named, budgeted, and AP-aged |

Vendor names must match **exactly** across `vendor_add`, `receive_invoice`, and
`pay_invoice`.

## Permits & government fees

Permits are **prepaid** — there is no AP relationship with the jurisdiction, and **the
jurisdiction itself is never a vendor**. Post a permit directly with `post_transaction`:

```
postings:
  expenses:permits and fees   120.00 $
  assets:checking                       (balancer)
```

A permit **expediter** (a hired consultant) *is* a vendor — a professional one, with their
own AP account via `vendor_add`.

## GC pass-through invoices

When the general contractor subs out a trade and passes the invoice through, **the GC stays
the vendor** (`liabilities:ap:vendor:{GC}`); the expense goes to the trade account(s):

- **Single-line** pass-through: use `receive_invoice` with the GC as `vendor` and the trade
  account as `expense_account`.
- **Multi-line** GC invoices spanning several trades: use one `post_transaction` with one
  posting per trade account and a single balancing AP posting to the GC, tagged with the
  invoice reference:

```
description: "GC invoice"
tags: { invoice: "INV-042", vendor: "{GC}" }
postings:
  expenses:construction:plumbing    3000.00 $
  expenses:construction:electrical  2000.00 $
  liabilities:ap:vendor:{GC}                  (balancer)
```

Paying either form is the normal `pay_invoice` against the GC.

The live vendor list (with AP balances) is the dynamic resource `ledger://vendors`.
