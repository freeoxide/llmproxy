# 03. Engine and crate split

- **Status:** Not started
- **Depends on:** 02
- **Audit rows:** None
- **Code-audit bugs:** BUG-23
- **Backlog:** None
- **Conflicts:** 13, 16

Make one request engine that the server, the SDK and the bindings all share. This
comes before features so they don't get built into the axum layer, where SDK users
can't reach them.

## Target crates

```text
llm-proxy-protocol     wire types and client codecs; no I/O
llm-proxy-adapters     provider codecs; no I/O (split from llm-proxy-provider)
llm-proxy-transport    HTTP, credentials and egress
llm-proxy-engine       request lifecycle without HTTP, plus the traits
llm-proxy-store-*      optional: sqlite, postgres, redis
llm-proxy-server       axum binding: routes and error mapping only
llm-proxy-admin        optional: admin API and web UI
llm-proxy-mcp          optional
llm-proxy              facade crate with cargo features
apps/llm-proxy         daemon and CLI
```

The facade name is taken by the binary's package today. Rename that package and
keep the binary named `llm-proxy`. Confirm each new name is free on crates.io.

## Pipeline slots

Every feature takes one of these places, in this order:

1. Authenticate the caller.
2. Apply caller limits and key allowlists.
3. Resolve the route to a provider or pool.
4. Claim the idempotency key.
5. Look up the cache and join any single-flight request.
6. Run preflight and reserve budget.
7. Run pre-call guards.
8. Apply request mutations in their fixed phases.
9. Run the attempt loop: pick a member and credential, encode, send, retry or
   fail over.
10. Hold the stream until it can be committed to the client.
11. Run streaming and post-call guards.
12. Encode for the client.
13. Settle budget, write usage rows and store cache and idempotency results.

## To do

1. **Split adapters from transport** so pure encoding doesn't need `reqwest`.
2. **Extract the engine.** Move the lifecycle out of
   `crates/llm-proxy-server/src/routes/core_pipeline.rs`. It takes core types and
   returns a `CoreResponse` or a stream of `CoreEvent`, with no axum types.
3. **Add traits with local defaults (conflict 13):** `Router`, `Admission`, `Cache`, `Guard`,
   `CredentialSource`, `UsageSink` and `ConfigSource`. `EventBus` and `Tokenizer`
   already exist.
4. **Open the provider set.** Replace the closed `ProviderProtocol` and
   `ProviderAdapter` enums with registration, so an SDK user can add a provider.
5. **Support config from code.** Add a builder, and make TOML `AppConfig` one
   `ConfigSource`.
6. **Shrink the server.** It keeps routes and error mapping, and `build_router`
   stays public behind a small, stable API.
7. **Clean up boundaries.** Move admin reads off direct SQLite access in
   `crates/llm-proxy-api/src/reader.rs`, and move `crates/llm-proxy-core/src/pid.rs`
   to the app.
8. **Support endpoint families (conflict 16).** Keep admission, routing and
   accounting generic around family-specific handlers. `MAX_BODY_BYTES` and the
   `TimeoutLayer` become per-family settings.
9. **Add cargo features.** The server builds without `rusqlite`, the core without
   `tiktoken-rs`, and `reqwest` uses one TLS stack (BUG-23).
10. **Make it publishable.** Remove the exact `time` pin from `Cargo.toml` and add
    versions to the 14 path dependencies.

## Done when

- A Rust program runs the full pipeline against a fake upstream without binding a
  port.
- An axum app mounts the proxy router in a test.
- `llm-proxy-protocol` and `llm-proxy-adapters` build with no network dependencies.
- The contract tests from plan 02 pass unchanged through the binary.
- `cargo publish --dry-run` passes for every published crate.
