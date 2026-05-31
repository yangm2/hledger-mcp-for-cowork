# Tool Design — tiering, lazy resources, and profiles

> **Extracted for the hledger fork.** The backend-agnostic MCP tool-design patterns from
> `gnucash-bindings-mcp` →
> [00-overview.md](https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/00-overview.md)
> (MC-8 tiering + lazy resources, MC-10 profiles). Tool *names* are the domain catalog
> (mapped to hledger in the rearchitecture doc's §9); the `ledger://` resource scheme below
> is illustrative (any URI scheme works).

---

## Tiering + resource-based lazy context (MC-8)

**Problem:** MCP loads *all* tool schemas into the context window at session start. With
~30+ tools that is several thousand tokens, and rarely-used setup tools shouldn't occupy
context during everyday operation.

**Three tiers:**

- **Tier 1 — operational** (loaded always, full descriptions): the daily read + write +
  correction tools (balances, lists, post/pay/fund, update/void).
- **Tier 2 — administrative** (loaded always, **one-line** descriptions; detail in a
  resource): chart-of-accounts (`book_*`), vendors (`vendor_*`), budget (`budget_*`), ECO
  (`eco_*`).
- **MCP Resources** (zero startup cost, fetched on demand): the verbose guides and indices.

**Resources (fetched on demand):**

```
ledger://session-context      — tool groups, conventions, resource index (read at start)
ledger://account-guide        — account types / naming conventions
ledger://vendor-guide         — vendor categories
ledger://expected-chart       — full expected account tree (used by verify-structure)
ledger://budget-guide         — budget workflow (periodic txns + balance --budget)
ledger://eco-guide            — ECO numbering / approval workflow
ledger://vendors              — live vendor list with AP balances (dynamic; hits hledger)
```

**`session-context` + `server_instructions`:** the server's `initialize` response carries
`server_instructions` directing the client to read `ledger://session-context` before any
tool call — a small static resource (no backend) giving tool groups, naming conventions, and
the resource index. Static resources and `tools/list` are answered **without** touching
hledger, keeping discovery off the cold-start path.

**Why it matters:** the verbose per-tool docs live in resources, fetched only when relevant,
so steady-state startup stays small and the model picks tools from a leaner list.

---

## Tool profiles via CLI flag (MC-10)

**Decision:** `--profile` on the start command restricts which tools are *advertised* in
`tools/list`. The full catalog is always compiled in and always *callable*; the profile only
filters advertising — so a tool named from a prior session still dispatches normally.

**Rationale:** match the advertised set to the task — advertising all tools during a quick
balance check wastes context and invites wrong-tool selection.

| Profile | Advertises | When |
|---|---|---|
| `full` (default) | all tools | general use, agentic tasks |
| `operational` | Tier 1 only | daily ledger work |
| `readonly` | read-only tools | balances & reports |
| `setup` | `book_*` + `vendor_*` + `budget_*` + `eco_*` | pre-construction setup |
| `construction` | Tier 1 + `eco_*` | active construction; track COs |
| `reconcile` | reconciliation + reporting | month-end |

**Implementation:** profile is runtime state set at start; `tools/list` filters by a
per-profile name set; dispatch is unaffected. `status` reports the active profile. (Optional
later: a `--tools a,b,c` flag for ad-hoc filtering.)

**Forward note for the fork:** with the HTTP/SSE transport, the profile is per-server-process
as today; if a future build supports `tools/listChanged`, a builder-pattern progression
(advertise `setup` first, promote to `full` after `verify-structure` passes) becomes possible
— deferred until the client honors `tools/listChanged`.
