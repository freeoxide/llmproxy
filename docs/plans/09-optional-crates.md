# 09. Optional crates

- **Status:** Not started
- **Depends on:** 05; the semantic cache also needs 06
- **Audit rows:** None
- **Code-audit bugs:** None
- **Backlog:** 35
- **Conflicts:** 7, 9, 13, 16, 17

Build the large features as opt-in crates behind cargo features. Each uses the engine's
traits for admission and accounting instead of its own copy. Take them in this order
unless real use says otherwise.

## To do

1. **Replica stores (conflict 13).** `llm-proxy-store-postgres` and
   `llm-proxy-store-redis` hold limits, cooldowns, budgets and usage across instances.
   Multi-tenant hosting is assessed only after this works.
2. **Admin API and web UI.** `llm-proxy-admin` serves key management, usage and config
   views on the dedicated admin listener.
3. **Admin MCP tools (backlog 35).** Expose the admin API as MCP tools, reusing its auth.
4. **Guardrails (conflict 9).** Each guard declares a mode (pre-call, buffered or
   post-hoc) and whether it fails open or closed. Masking on streams needs placeholder
   handling across chunks, so a guard that can't do this rejects instead.
5. **Semantic cache (conflict 7).** Off by default, with caller-scoped keys and the data
   policy from plan 06.
6. **MCP gateway (conflict 17).** `llm-proxy-mcp` has its own auth and session model, and
   shares admission and accounting with the proxy.
7. **Non-chat endpoints (conflict 16).** Embeddings, rerank, audio, images, files and
   batch, each as a family-specific handler using the plan 03 engine. See
   [`endpoints.md`](../research/endpoints.md) for per-family notes.

## Done when

- The default build compiles none of these crates.
- Each crate can be enabled alone, and the workspace tests pass with all of them on and
  with all of them off.
- Two instances sharing the replica stores enforce one budget between them.
