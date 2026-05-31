# Fork extract — backend-agnostic pieces for the hledger MCP repo

Staging folder. These files are extracted/adapted from `gnucash-bindings-mcp` and stripped of
GnuCash-specific framing so they drop cleanly into the fresh **hledger MCP** repo. Copy these
out; this folder is not part of this repo's doc set.

## In this folder

- **concurrency-model.md** — the *minimal + epoch* concurrency model **+ the TLA+/TLC spec**;
  epoch = git commit. (From `multi-client.md` M10.4 + Formal verification.)
- **chart-of-accounts.md** — the construction-project domain account model (MC-6), as
  double-entry structure; hledger realization is in the rearchitecture doc's §9.
- **tool-design.md** — MCP tool **tiering + lazy resources** (MC-8) and **profiles** (MC-10).

## Also copy from the source repo (by GH URL)

- The primary plan: **hledger-rearchitecture.md** (the doc this kit supports) —
  <https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/hledger-rearchitecture.md>
- **Appendix F — MCP protocol versions** (backend-agnostic; needed for version negotiation,
  esp. HTTP/SSE + vLLM) —
  <https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/16-appendix-f-mcp-protocol-versions.md>
- **Appendix E — Model Options & Client Architectures** (the local-model / hybrid-coordinator
  story; adapt to **vLLM**) —
  <https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/14-appendix-e-model-options.md>

## Copy-and-adapt (concepts, re-implement in Rust)

- **Appendix A — Testing** (unit/integration + smoke-test conventions) →
  <https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/10-appendix-a-testing.md>
- **Phase 9 — Instrumentation / hybrid-readiness** (metrics + compound-tool analysis; ties to
  Appendix E) →
  <https://github.com/yangm2/gnucash-bindings-mcp/blob/main/docs/development/15-phase-09-instrumentation.md>

## Reference-flip reminder

`hledger-rearchitecture.md` currently links the concurrency model via the **multi-client.md
GH URL**. Once **concurrency-model.md** lives in the fork next to it, repoint those references
to the local file.
