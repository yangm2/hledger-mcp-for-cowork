hledger MCP server: a construction-project ledger over an hledger journal (git-backed,
append-only). Before the first tool call, read the resource `ledger://session-context` —
it carries the tool groups, the ledger conventions (corrections are reversing entries;
amounts are decimal strings; accounts/commodities must be declared before use), and the
index of on-demand guides. Tool descriptions here are intentionally brief; the detail
lives in the `ledger://` resources.
