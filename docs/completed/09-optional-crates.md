# 09. Optional crates

- **Status:** Completed (2026-11-07)
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

## Done

- The workspace has `default-members` (the pre-plan-09 crates) plus the
  optional members `llm-proxy-store-redis`, `llm-proxy-store-postgres`,
  `llm-proxy-admin`, `llm-proxy-guardrails`, `llm-proxy-semantic-cache`
  and `llm-proxy-mcp`: a bare `cargo build` compiles none of them
  (verified: `cargo tree` over the default build graph shows zero
  optional crates), each builds alone, and `cargo test --workspace
  --all-features` / `cargo test --workspace` both pass.
- The admission counters moved behind one engine trait
  (`AdmissionBackend`), the cooldown view behind `Cooldowns` (now
  wall-clock), so the replica stores implement engine seams instead of
  their own copy of the rules. Redis: every counter move is one atomic
  `INCR`, refusals roll back, unsettled leases expire on a TTL; tested
  over the real RESP wire against an in-repo minimal server (no Redis
  binary exists here) — two instances over one store enforce one budget
  between them, concurrently, and a refund on one releases budget for
  the other. Postgres: one transaction per admission (windows, caps and
  the lease commit together or not at all), deadpool-managed pool, a
  purge timer; its identical done-when tests are `#[ignore]`d behind
  `LLM_PROXY_TEST_POSTGRES` because this environment has no Postgres
  server. `[replica]` wires either store into the daemon and refuses to
  start when the section names a backend whose feature is off.
- `llm-proxy-admin` (steps 2-3, backlog 35) serves on the dedicated
  admin listener behind `[auth.admin]`: managed client keys (issue, list,
  revoke; argon2id verifiers, the token shown once, merged into
  `ClientAuth` at startup behind the daemon's `admin-ui` feature), the
  sanitized running config, the read-only usage views, and an embedded
  web UI over the same JSON API. `POST /mcp` exposes the admin API as
  MCP tools under protocol revision 2026-07-28 — single-JSON responses,
  202 for notifications, `-32020` header validation, 404/`-32601` for
  unknown methods, `initialize` answered for older-era clients — behind
  the same admin token.
- `llm-proxy-guardrails` (step 4, conflict 9): every guard declares a
  mode (pre-call, buffered, post-hoc) and a fail policy (open or
  closed); the engine's `Guard` trait gained in-place
  `transform_request`/`transform_response` hooks the wrapper masks
  through. A masking guard that cannot keep placeholders consistent
  across stream chunks rejects streaming requests instead of masking
  them unsafely — the plan's rule — and when a guard rewrites a request
  the engine refuses the same-format passthrough shortcut, so the
  redacted core, not the raw client bytes, reaches the upstream.
- `llm-proxy-semantic-cache` (step 5, conflict 7) implements the
  engine's `Cache` trait: caller-scoped buckets, prompts embedded
  through an `Embedder` trait (default: the offline deterministic
  trigram embedder), threshold-gated cosine matching, TTL and per-caller
  bounds, complete non-streaming responses only. The engine's `CacheKey`
  now carries the normalized request so the cache embeds the prompt
  instead of the body hash. Wired from `[server.semantic_cache]` behind
  the `semantic-cache` feature, off by default; the data-class policy
  page covers what it stores.
- `llm-proxy-mcp` (step 6, conflict 17): MCP clients reach configured
  upstream MCP servers through the proxy on its own listener with its
  own token list and a per-token session table aged out by TTL. Every
  `tools/call` reserves and settles through the engine's shared
  `Admission` step and writes the same usage rows (route kind `mcp`),
  refused calls included. Upstream calls carry the revision's required
  headers. The daemon binds it from `[mcp]` behind `mcp-gateway`;
  tests run against in-repo fake upstream MCP servers.
- Non-chat families (step 7, conflict 16) behind the server's
  `endpoint-families` feature: embeddings, rerank, audio
  transcriptions/speech, images, files and batch, each a family-specific
  handler (new `ProviderRouteKind` variants, per-family
  `[server.endpoint_families]` limits) around the generic spine — the
  same registry routing, admission, transport and ledger as chat, never
  forced through the chat core. JSON families get the one surgical edit
  (model aliasing, unknown fields preserved); audio and file bodies pass
  through verbatim with the client's content type.
- Verification gaps recorded in the plan's review, not hidden: no live
  Redis/Postgres/MCP servers exist in this environment, so the Redis and
  MCP paths run against in-repo loopback fakes that speak their real
  wire protocols, and the Postgres integration tests ship `#[ignore]d`
  behind `LLM_PROXY_TEST_POSTGRES`.
