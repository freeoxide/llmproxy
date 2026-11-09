# Plan 03 — Engine and crate split: fix spec

Paths are relative to the repository root. Line numbers were re-checked against the tree on
2026-11-09 and may drift by a few lines; symbol names are authoritative.
Cross-plan: items tagged `(also: plan 04)` touch code plan 04's spec also lists. The 03 spec owns
the `expect()`/slice clean-up in `engine/src/lib.rs`, `transport/src/client.rs` and
`core/src/provider_config.rs`; the 04 spec owns `classification.rs`, `error.rs`, `provider_registry.rs`.

## Done — leave alone
- Crate split: `llm-proxy-protocol`, `-adapters`, `-transport`, `-engine`, `-server`, `-sdk`, app package `llm-proxy-app` with `[[bin]] llm-proxy`; `llm-proxy-provider` is gone (A3, B4).
- Engine surface: `Engine::execute` / `execute_stream` (`engine/src/lib.rs`), `EngineResponse { core, raw_body }`, `EngineStream { head, tail }` (`engine/src/types.rs`); engine has no axum in `[dependencies]` (axum only in dev-deps and `catalog.rs` tests).
- Traits with local defaults: `Router`/`RegistryRouter`, `Admission`/`AllowAllAdmission`/`PolicyAdmission`, `Cache`/`NoCache`/`MemoryCache`, `Guard` (no-op defaults), `CredentialSource`/`ConfigCredentialSource`, `ConfigSource`/`StaticConfigSource`/`SwappingConfigSource` (`engine/src/traits.rs`, `admission.rs`, `cache.rs`). `EventBus` and `Tokenizer` pre-exist.
- Open provider set: `ProtocolName`, `ProviderAdapterRegistry::{builtin,register,get,protocol_names}` and `RegistrationError::Duplicate`; tests `a_registered_custom_protocol_serves_through_the_pipeline`, `duplicate_protocol_registration_is_rejected` (`engine/tests/open_provider_set.rs`).
- Builders: `EngineBuilder` (`engine/src/lib.rs`), `ProxyBuilder` (`sdk/src/lib.rs`); tests in `sdk/tests/mount_and_build.rs`.
- Server: `build_router(AppState)` public (`server/src/lib.rs:35`), `AppState::engine()` builds once via `EngineSlot`; per-family `DefaultBodyLimit` (`server/src/routes/mod.rs:154`, test `body_limit_is_per_family_not_global`).
- `pid` moved to `apps/llm-proxy/src/pid/`; `crates/llm-proxy-core/src/pid.rs` is gone.
- Fake-upstream pipeline tests (`engine/tests/fake_upstream.rs`), axum mount test (`sdk/tests/mount_and_build.rs:72`).
- Pipeline slots 3 (resolve_plan), 4 (idempotency_step), 5 (cache lookup + single-flight), 6 (admit), 9 (ladder), 10 (hold), 11 (inspect_response/transform_response/inspect_event), 12 (client encode in server), 13 (settle/store) are in the plan's order in `engine/src/lifecycle.rs`; only slots 2, 7 (stream) and 8 deviate (items below).
- Publish metadata: `time` pin gone, versions on root path deps, description/license/repository on every crate (K1–K3).

## Work items

### 03-1 [FIX] [P0] Enforce the per-key allowlist at slot 2, before idempotency replay, cache and single-flight
- **Where:** `crates/llm-proxy-engine/src/lifecycle.rs` — `run_once_attempt` (resolve_plan, then `idempotency_step` ~L981, cache `lookup` ~L1015, single-flight ~L1049, `admit` ~L1094) and `execute_stream_attempt` (`idempotency_step`, replay, `stream_shares.acquire`, then `admit`); `crates/llm-proxy-engine/src/admission.rs` `PolicyAdmission::reserve` (allowlists at ~L371 and ~L388); trait `Admission` in `engine/src/traits.rs` (~L230); `crates/llm-proxy-server/src/auth_pair.rs` `impl Admission for SwappingAuthPair` (~L155); `engine/src/lib.rs` module doc (slot 2 text "server middleware today").
- **Now:** allowlists (`allow_models`, `allow_providers`) are checked only inside `PolicyAdmission::reserve`, which runs at slot 6 after the idempotency claim/replay, the cache hit and the single-flight join. A replay, a cache hit or a joined flight returns before `reserve`.
  Real exposure (verified, not cross-tenant): `CacheKey`, `IdempotencyStore::claim` and `SingleFlight` are all scoped by `caller` (the key id), so a hit can only be served to the key that already got that answer. The gap is temporal, and it is real in the managed-key daemon: `AppState::with_auth_pair` installs `SwappingAuthPair` as the engine's `Admission` (`server/src/state.rs` ~L246), so `SwappingAuthPair::replace` tightens the live key's allowlist WITHOUT rebuilding the engine (a rebuilt engine would get fresh `MemoryCache`/`IdempotencyStore`). The old `MemoryCache` entries (`server.cache.ttl`) and idempotency outcomes (`server.idempotency.ttl`) survive the swap, so a key that was just restricted away from model M is still served M's cached or replayed answer until the TTL ends. The same applies to any user-supplied long-lived `Cache`/`Admission` pair (the `Cache` trait is public, and a shared cache outlives any one engine). A request that was never allowed cannot be served from cache, because nothing was stored for it.
- **Wanted:** plan slot order: "2. Apply caller limits and key allowlists. 3. Resolve the route ... 4. Claim the idempotency key. 5. Look up the cache". The allowlist verdict must precede slots 4 and 5 on both execution paths. Rate/budget windows stay at slot 6 (cache hits are deliberately not charged, plan 05 step 6).
- **How:**
  1. Add to trait `Admission` a defaulted method `async fn authorize(&self, claim: &AdmissionClaim<'_>) -> Result<(), EngineError> { let _ = claim; Ok(()) }` (doc: slot 2, no accounting, must be cheap and side-effect free).
  2. In `admission.rs` factor the two allowlist blocks of `reserve` into `fn check_allowlists(&self, claim: &AdmissionClaim<'_>) -> Result<(), EngineError>`; call it from the new `PolicyAdmission::authorize` and keep calling it at the top of `reserve` (defence in depth; the Redis/Postgres crates and `live_server.rs` tests call `reserve` directly and may assert the 403).
  3. In `lifecycle.rs` extract the "first member + model" selection from `admit` (the `ordered_candidates(...).first()` block) into a helper returning `(member, model)`, and build the `AdmissionClaim` in a helper used by both `admit` and a new `authorize_step(inner, req, plan) -> Result<(), EngineError>` (estimated tokens may be 0/None in the authorize claim; allowlists ignore them).
  4. Call `authorize_step` immediately after `resolve_plan` in `run_once_attempt` and `execute_stream_attempt`. Streaming: on error call `emit_response_failed` and return (no lead exists yet; mirror the idempotency error arm). Non-streaming: plain `?` (the `execute` wrapper emits the failed row).
  5. `SwappingAuthPair` MUST forward `authorize` (`Arc::clone(self.current().admission()).authorize(claim).await`), otherwise the default `Ok(())` silently disables slot 2 in the daemon. Check any other `impl Admission` that wraps another (grep `impl Admission for`: the test impls and `PolicyAdmission` need no change).
  6. Update the `engine/src/lib.rs` slot-2 doc line and the `admission.rs` module doc ("Allowlists (step 3)") to say allowlists run at slot 2 via `Admission::authorize`.
- **Test:** `crates/llm-proxy-engine/tests/cache_and_dedup.rs` (has `AlwaysHitCache`-style helpers; reuse the fake upstream pattern there), new tests:
  - `a_tightened_allowlist_beats_a_cached_answer`: a test `Admission` (`ToggleAdmission { forbid: Arc<AtomicBool> }`) whose `authorize` returns `Err(EngineError::Forbidden(..))` when the flag is set and whose `reserve` always succeeds; builder with `MemoryCache` and a counting fake upstream. Request 1 (flag off) reaches upstream (count 1). Set the flag. Identical request 2 must return `Err(EngineError::Forbidden(_))`, upstream count stays 1, and a `RecordingBus` shows a `ResponseFailed` row for request 2 and no `ResponseCompleted{cache_hit:true}`.
  - `a_tightened_allowlist_beats_an_idempotent_replay`: same with `server.idempotency.enabled` and an `Idempotency-Key` header (see `idempotency_key_replays_after_completion_and_409s_while_in_flight`, `cache_and_dedup.rs:623`).
  - `a_tightened_allowlist_beats_a_cached_answer_on_the_streaming_path`: streaming request with the same idempotency key replays events today; after the flip it must be `Forbidden`.
  - `admission.rs` unit test: `authorize` returns 403 for a disallowed model / provider and `Ok` for an unrestricted key, with no counter moved (a following `reserve` of an allowed claim still fits the rpm window).
  - `crates/llm-proxy-server/tests/auth_pair.rs` or `core_pipeline.rs` near L3827 (existing `SwappingAuthPair` tests): after `pair.replace_admission(<policy forbidding model>)`, a previously cached request returns 403 over HTTP.
  All must fail before the change (cache/replay answered 200) and pass after.
- **Done when:** `grep -n "authorize" crates/llm-proxy-engine/src/lifecycle.rs` shows the call before `idempotency_step(` in both `run_once_attempt` and `execute_stream_attempt`; the new tests pass; `impl Admission for SwappingAuthPair` defines `authorize`.
- **Risk / notes:** pools: allowlists match `requested_model` OR the first ordered member's upstream model; keep using the same helper as `admit` so slot 2 and slot 6 agree. A failover to a later member with a different upstream model is not re-checked (pre-existing; mention in the `authorize` doc rather than expanding scope). Do not move rpm/tpm/budget checks. `emit_response_failed` for non-stream comes from `execute`.

### 03-2 [FIX] [P1] Streaming path: run pre-call guards after the budget reservation (slot 7 after slot 6)
- **Where:** `engine/src/lifecycle.rs` `execute_stream_attempt` — `for guard in &inner.guards { guard.inspect_request ... }` block (~L1607) precedes `admit(...)` (~L1630). Non-stream `run_ladder_once` (~L1165) already runs guards after `admit`.
- **Now:** a guard-rejected streaming request never reserves; non-streaming reserves first, then is refused and refunded. Order differs per mode.
- **Wanted:** "6. Run preflight and reserve budget. 7. Run pre-call guards."
- **How:** move the guard loop below the `admit` match. On a refusal after `admit`: call `settle(&inner, &ticket, false, None, None)` (refund; no usage), `emit_response_failed`, publish `Err(refusal.clone())` to `stream_lead.take()`, return `Err(refusal)`. Do not construct `build_core_event_stream` before the guards pass (it takes ownership of `ticket`).
- **Test:** `engine/tests/admission_budgets.rs` (has `CountingAdmission`, `SettlementLog`): `a_guard_refused_stream_reserves_then_refunds` using a rejecting `Guard` (see `BlockSecretText` in `open_provider_set.rs:213`) with a `SettlementLog` admission: after `execute_stream` returns `Err(EngineError::InvalidRequest(_))`, the log shows exactly one `reserve` and one `settle(succeeded=false, usage=None)`; today it shows zero reserves. Also assert a non-streaming refusal produces the identical log (parity).
- **Done when:** `grep -n "inspect_request\|admit(" engine/src/lifecycle.rs` shows `admit(` before `inspect_request` in `execute_stream_attempt`; test passes.
- **Risk / notes:** behaviour change: a guard-refused stream now counts toward rpm/rpd (same as non-stream today); document in the test comment. The refusal must still reach single-flight joiners as itself (keep `lead.publish`).

### 03-3 [FIX] [P1] Apply guard request rewrites at slot 8 (after inspect and reserve), not before routing
- **Where:** `engine/src/lifecycle.rs` `apply_request_transforms` (~L815) called from `execute` (~L792) and `execute_stream` (~L1460) before `run_once` / `execute_stream_attempt`; doc comment calls it "Slot 7.5".
- **Now:** `Guard::transform_request` (masking) runs before routing, idempotency, cache and admission on both paths, and BEFORE `inspect_request`. Consequence beyond ordering: with a masking guard and a rejecting guard both installed, the rejecting guard only ever sees the masked text, so a secret it would block is already redacted.
- **Wanted:** "7. Run pre-call guards. 8. Apply request mutations in their fixed phases." (after 6, and after 7).
- **How:** delete the two early calls. Non-stream: in `run_ladder_once` after the `inspect_request` loop, `let rewritten = apply_request_transforms(inner, req).await?;` where the function now takes `&EngineRequest` and returns `Option<EngineRequest>` (`None` when no guard changed the text; set `inbound.wire.force_core_encode = true` on the clone as today), then `let req = rewritten.as_ref().unwrap_or(req);` for the rest of the ladder. The `cache_key`/`CacheKey` keeps borrowing the ORIGINAL request (cache identity = what the client sent). Stream: after the reordered guard check and `admit` (03-2), `let req = match apply_request_transforms(&inner, &req).await { Ok(..) , Err(e) => settle + emit_response_failed + lead.publish + return }` before `build_core_event_stream`. Keep the `before != after` text comparison.
- **Test:** `engine/tests/response_masking_passthrough.rs` (already has a masking guard): add `a_rejecting_guard_sees_the_unmasked_request`: install a masking guard and a rejecting guard (rejects text containing `sk-secret`) with the masker listed first; send `sk-secret`; assert `Err(InvalidRequest)` and zero upstream calls (today the masker runs first and the request goes through). Add `a_cache_hit_is_served_without_running_request_transforms` with a counting `transform_request` guard: second identical request is a cache hit and the counter stays at 1 (documents the new order). Keep the existing masking tests green (upstream still receives the redacted body on both paths).
- **Done when:** `grep -n "apply_request_transforms" engine/src/lifecycle.rs` shows calls only inside `run_ladder_once` and `execute_stream_attempt`, after `inspect_request`; new tests pass.
- **Risk / notes:** the cache/idempotency/single-flight identity is now the unredacted request (same as before for `body_hash`, which was always the raw-body digest). Estimated tokens in `admit` use the unmasked text. Update the doc comment ("Slot 7.5") to "Slot 8".

### 03-4 [FIX] [P1] Make `UsageSink` an actual pluggable seam
- **Where:** `engine/src/lib.rs` — `EngineInner.usage: EventBusSink` (~L151), `Engine::usage_sink() -> &EventBusSink` (~L307), `EngineBuilder.usage: Option<EventBusSink>` (~L341), `EngineBuilder::event_bus` (~L473), `build()` (~L524); `engine/src/traits.rs` `UsageSink`/`EventBusSink` (~L663); every `inner.usage.record(...)` in `lifecycle.rs`; re-export in `sdk/src/lib.rs:68`.
- **Now:** no `dyn UsageSink` exists; only the concrete `EventBusSink` can be installed. `Engine::usage_sink()` has no callers in the workspace.
- **Wanted:** "Add traits with local defaults: ... `UsageSink` ..." and Done: "Traits with local defaults: ... `UsageSink`".
- **How:** change the field, builder field and accessor to `Arc<dyn UsageSink>`; add `EngineBuilder::usage_sink(mut self, sink: Arc<dyn UsageSink>) -> Self`; make `event_bus(bus)` set `Arc::new(EventBusSink::new(bus))`; default stays `EventBusSink` over `NoopBus`. `Engine::usage_sink()` returns `&Arc<dyn UsageSink>` (keep the name; no other callers). Update the `Debug` impl. Keep `EventBusSink` exported once (already in `traits` + root `pub use`; do not add another path).
- **Test:** `engine/tests/open_provider_set.rs`: `a_custom_usage_sink_receives_the_ledger_events` — a `CollectingSink` (`Mutex<Vec<ProxyEvent>>`) installed with `.usage_sink(..)`, run one non-streaming request through the existing fake provider; assert it received `RequestReceived` then `AttemptCompleted` then `ResponseCompleted` for the request id, and that a builder with BOTH `.event_bus(recording)` then `.usage_sink(custom)` delivers only to the later setter (document last-wins in the builder doc).
- **Done when:** `grep -rn "EventBusSink" engine/src/lib.rs engine/src/lifecycle.rs` shows it only in the default construction; test passes.
- **Risk / notes:** `usage_sink()` is public API; the changed return type is a breaking change inside 0.1.x, acceptable (zero callers).

### 03-5 [DECIDE] [P2] Admin read path still opens SQLite through `storage::UsageReader`
- **Where:** `crates/llm-proxy-api/src/routes.rs:16,22,44` (`UsageReader::new(db_path)`), `crates/llm-proxy-storage/src/usage_query.rs:285` (`Connection::open_with_flags`), `crates/llm-proxy-api/Cargo.toml` (enables storage `sqlite`), `crates/llm-proxy-admin/src/lib.rs`.
- **Now:** `api/src/reader.rs` is gone and `api` has no rusqlite dependency, but the reader moved into `storage` (gated by `sqlite`) and still reads the file directly. The query DTOs and `UsageQueryError::Sqlite(#[from] rusqlite::Error)` live in the same sqlite-gated module, so a backend-neutral trait would first need those types moved out.
- **Wanted:** plan step 7 "Move admin reads off direct SQLite access in `crates/llm-proxy-api/src/reader.rs`". Done says "the api crate no longer opens SQLite".
- **Recommended default: narrow the claim (DOC), do not build a trait yet.** Reason: there is one backend; a trait with a single implementor plus relocating the DTOs is churn without a consumer, and the dependency-hygiene goal (api/admin crates contain no SQL) is already met. Revisit when a Postgres reader exists.
- **How (default):** edit `docs/completed/03-engine.md` Done "Boundaries (step 7)": "Admin reads moved out of `llm-proxy-api` into `llm-proxy-storage::UsageReader`; the `api` and `admin` crates contain no SQL, but the process still reads the SQLite ledger through storage's `sqlite` feature. A backend-neutral reader trait is not built." If the alternative is chosen instead: move `RequestFilter/RequestRow/...` and a new `UsageQueries` async trait into non-gated storage modules, make `UsageQueryError` backend-neutral (`Backend(String)`), implement it for `UsageReader`, give `UsageApi::with_reader(Arc<dyn UsageQueries>, token)` and gate `UsageApi::new(path, ..)` behind an api feature `sqlite`.
- **Test:** default: none (doc). Alternative: `api/tests/usage_api.rs` gets one test using an in-memory fake `UsageQueries` returning a canned page, asserting the JSON body.
- **Done when:** the Done paragraph contains the sentence above (grep `still reads the SQLite ledger`).
- **Risk / notes:** none for the doc path.

### 03-6 [FIX] [P1] `tiktoken-rs` must not reach consumers that do not count tokens
- **Where:** root `Cargo.toml:51` `llm-proxy-core = { path = "crates/llm-proxy-core", version = "0.1.0" }` (default features kept); `crates/llm-proxy-core/Cargo.toml:14-17` `default = ["tiktoken"]`; consumers listing `llm-proxy-core = { workspace = true }`: adapters, transport, engine, server, api, admin, guardrails, mcp, sdk, semantic-cache, store-redis, store-postgres, app (and `bridge`, which bindings reach by path).
- **Now:** because the workspace entry has default features on, every dependent compiles `tiktoken-rs` (Cargo.lock closure confirms adapters/transport/engine/facade). Member crates cannot turn defaults off once the workspace entry has them on. Done claims "the core without `tiktoken-rs`" but this only holds for `cargo build -p llm-proxy-core --no-default-features`.
- **Wanted:** "The server builds without `rusqlite`, the core without `tiktoken-rs`" as a consumer-visible feature.
- **How:** set `default-features = false` on the root `llm-proxy-core` entry. Then enable BPE only where it is used: grep who calls `Counter`/`TiktokenTokenizer` (`engine/src/preflight.rs`, `engine/src/lib.rs` counter, server state `token_counter`). Add to `llm-proxy-engine/Cargo.toml` a feature `tiktoken = ["llm-proxy-core/tiktoken"]` in `default`, forward it from `llm-proxy-server`, `llm-proxy-sdk` and `apps/llm-proxy` (`default = ["tiktoken"]`, `tiktoken = ["llm-proxy-engine/tiktoken", ...]`). `llm-proxy-adapters`, `-transport`, `-protocol`, `-api`, `-admin`, `-storage` must not enable it. Check `bindings/node` and `bindings/python` lock/build still enable it through bridge -> engine default. Run `cargo shear`.
- **Test:** CI assertion (see 03-7): `! cargo tree -p llm-proxy-adapters -e normal | grep -q tiktoken-rs` and `! cargo tree -p llm-proxy-transport -e normal | grep -q tiktoken-rs`; plus `cargo test -p llm-proxy-engine --no-default-features` compiles (the heuristic path of `Counter`). Behaviour: `engine/src/preflight.rs` tests that assert BPE-specific counts must be `#[cfg(feature = "tiktoken")]`.
- **Done when:** `cargo tree -p llm-proxy-adapters -e normal -i tiktoken-rs` reports no match; default `cargo test --workspace --all-features` still passes (all-features re-enables it).
- **Risk / notes:** feature unification means a plain `cargo build` of the app (default features) still includes tiktoken, as intended. Do not break `cargo test --workspace` (resolver 2 unifies per build, so tests that need BPE see it through the default engine feature).

### 03-7 [FIX] [P1] CI: feature-build and dependency-closure guards
- **Where:** `.github/workflows/ci.yml` (currently only `cargo test -p llm-proxy-sdk --no-default-features --locked`, lines ~33-35).
- **Now:** nothing builds `llm-proxy-server` without rusqlite, core without tiktoken, nor checks that protocol/adapters/engine stay network-free or on one TLS stack. These are Done-when bullets and BUG-23.
- **Wanted:** "`llm-proxy-protocol` and `llm-proxy-adapters` build with no network dependencies"; "The server builds without `rusqlite`, the core without `tiktoken-rs`, and `reqwest` uses one TLS stack".
- **How:** add a CI job `feature-closures` (after the quality job's toolchain steps) with these steps, each failing on violation:
  - `cargo build -p llm-proxy-server --no-default-features --locked` and `! cargo tree -p llm-proxy-server --no-default-features -e normal | grep -q rusqlite`.
  - `cargo build -p llm-proxy-core --no-default-features --locked` and `! cargo tree -p llm-proxy-core --no-default-features -e normal | grep -q tiktoken-rs`; `! cargo tree -p llm-proxy-adapters -e normal | grep -q tiktoken-rs` (depends on 03-6).
  - network-free: for `p in llm-proxy-protocol llm-proxy-adapters`: `! cargo tree -p $p -e normal --prefix none | grep -Eq '^(reqwest|hyper|tokio|mio|socket2|rustls|axum) '`.
  - engine has no axum at runtime: `! cargo tree -p llm-proxy-engine -e normal --prefix none | grep -q '^axum '`.
  - one TLS stack: `! grep -Eq '^name = "(native-tls|openssl|openssl-sys)"' Cargo.lock`.
  Use `--prefix none` output (names first on each line); verify the exact regex locally against `cargo tree` output format before committing.
- **Test:** the CI job itself; locally run each command once and confirm exit codes (include a deliberate-failure sanity check by running the `grep` against a package known to contain the crate, e.g. `llm-proxy-transport` contains `reqwest`).
- **Done when:** `ci.yml` has the job and it is green on the branch.
- **Risk / notes:** `cargo tree` for dev-deps is excluded by `-e normal`; the lockfile TLS grep is a coarse guard (see 03-14 for reqwest 0.13).

### 03-8 [FIX] [P1] CI: `cargo publish --dry-run` check for the published crates
- **Where:** `.github/workflows/ci.yml` (no publish step), `release.yml` is cargo-dist only; crate manifests (no `publish = false` except `bindings/*`).
- **Now:** Done-when "`cargo publish --dry-run` passes for every published crate" is a self-report. K4 (inherited `readme = "README.md"` with no per-crate README) is also unverified; cargo resolves a workspace-inherited readme path against the workspace root, so it should package, but nothing proves it.
- **Wanted:** the Done-when bullet verified by CI.
- **How:** decide the published set: engine chain `protocol, core, adapters, transport, storage, engine, api, server, sdk`, app `llm-proxy-app`. For every other library crate (bridge, guardrails, semantic-cache, mcp, admin, store-redis, store-postgres) either include it or set `publish = false` in its `Cargo.toml` (default: include them if they carry versions on all path deps; otherwise `publish = false`). Add CI job `publish-dry-run`: `cargo publish --workspace --dry-run --locked` (workspace publish with an in-order local overlay is stable on the pinned 1.96.1 toolchain; confirm with `cargo publish --help`, and fall back to `cargo package --workspace --locked` if the flag is absent). Exclude `publish = false` crates via the manifest.
- **Test:** the CI job; locally run it once and record the result in the commit message.
- **Done when:** job present and green; `grep -c "publish = false" crates/*/Cargo.toml` matches the intended unpublished set.
- **Risk / notes:** packaging needs every `path` dep to carry `version`; `llm-proxy-guardrails`/`-bridge` are not in `[workspace.dependencies]` by design (bridge comment). If they are published, add versions where missing.

### 03-9 [DECIDE] [P1] "Contract tests pass unchanged through the binary"
- **Where:** `crates/llm-proxy-server/tests/contract_replay.rs` (in-process `build_router` + `tower::oneshot`, 18 tests), fixtures in `crates/llm-proxy-server/tests/fixtures/contract/` (README there), `apps/llm-proxy/tests/cli_serve.rs` (spawns `CARGO_BIN_EXE_llm-proxy` with a fake upstream: helpers `write_config`, `RunningServer::start`).
- **Now:** no test replays a contract fixture through the spawned binary.
- **Wanted:** Done-when "The contract tests from plan 02 pass unchanged through the binary."
- **Recommended default: do both, cheaply.** Add a binary-level replay for ONE non-streaming and ONE streaming fixture (the scaffolding already exists), and fix the wording to "through the real router in-process; two fixtures additionally through the spawned binary". Reason: replaying all 18 over TCP duplicates coverage; the binary adds only config loading, startup wiring and real listener behaviour, which two fixtures prove.
- **How:** new `apps/llm-proxy/tests/cli_contract.rs`: start a loopback fake upstream (copy the pattern from `cli_serve.rs`) that captures the request body and replies with a canned Anthropic/OpenAI body; `RunningServer::start` against it; POST the fixture's `request.json` (path from `concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/llm-proxy-server/tests/fixtures/contract/<dir>/...")`; read the fixture README for file names) with a blocking HTTP client already used in `cli_serve.rs`.
- **Test:** `contract_fixture_replays_through_the_spawned_binary` (non-stream) and `..._streaming...`: assert status 200, the captured upstream body equals the fixture body apart from the same allowed edits `contract_replay.rs` permits (reuse its comparison rules; copy the small normaliser rather than exporting it), and the client response parses.
- **Done when:** both tests pass; `docs/completed/03-engine.md` Done-when verification sentence names them.
- **Risk / notes:** keep `NO_PROXY=127.0.0.1` as `cli_serve.rs` does; do not touch fixtures (their README says they are frozen).

### 03-10 [DOC] [P2] Correct the crate-table and transport claims in the plan/Done text
- **Where:** `docs/completed/03-engine.md` "Target crates" block and Done "Crate split".
- **Now:** (a) Done says `llm-proxy-transport` is "HTTP egress, credentials, discovery"; transport has `client.rs`, `discovery.rs`, `lib.rs` only. Credential selection is `engine/src/traits.rs:581 ConfigCredentialSource` and `lifecycle.rs resolve_attempt` (~L271, `.pick(` ~L295). (b) The target list shows `llm-proxy-store-*` (sqlite, postgres, redis); sqlite is the `sqlite` feature of `llm-proxy-storage`, not a crate. (c) Extra crates not in the table exist: core, storage, api, bridge, guardrails, semantic-cache.
- **Wanted:** doc matches code (code is the truth).
- **How:** edit the Done paragraph: "`llm-proxy-transport` (HTTP egress and model discovery; credential selection lives in the engine's `CredentialSource`)". Add a line under the target list: "As built: `store-sqlite` is the `sqlite` feature of `llm-proxy-storage`; `llm-proxy-core`, `-storage`, `-api`, `-bridge`, `-guardrails` and `-semantic-cache` exist beyond this table; the facade is `llm-proxy-sdk`." Do not add design essays (AGENTS.md).
- **Test:** none.
- **Done when:** `grep -n "credential selection lives in the engine" docs/completed/03-engine.md` hits.
- **Risk / notes:** also fix the crate-level doc in `crates/llm-proxy-transport/src/lib.rs` if it says "credentials".

### 03-11 [DOC] [P2] Remaining wording corrections in plan 03 Done text
- **Where:** `docs/completed/03-engine.md`.
- **Now / Wanted (one edit each):**
  - Config from code: Done says TOML `AppConfig` is one `ConfigSource` "(`StaticConfigSource`)". The daemon/server uses `SwappingConfigSource` (`server/src/state.rs:192`); `StaticConfigSource` serves `EngineBuilder::new`, SDK and bridge. Say both.
  - "14 path dependencies": root `Cargo.toml` has 13 path entries in `[workspace.dependencies]` (bridge is referenced directly by `bindings/*`, which carry their own version). Reword to "every path dependency carries `version = \"0.1.0\"`".
  - Done-when "without binding a port": the proxy binds no port; the fake upstream is a loopback listener on `127.0.0.1:0` (`engine/tests/fake_upstream.rs:29`). Keep the existing Done phrase "with the proxy binding no port" and drop the plan text's ambiguity by adding "(the fake upstream itself is a loopback listener)".
  - Server step: Done says "keeps routes, middleware and error mapping"; it also keeps client encode/decode and bare-model resolution (`core_pipeline.rs` ~1086 lines, `route_target_exists`, `resolve_bare_target`). Add "client codecs and bare-model resolution".
  - Add after Done-when text the result of 03-9 and 03-8 once decided.
- **Test:** none.
- **Done when:** the four phrases above appear in the file.
- **Risk / notes:** none.

### 03-12 [DECIDE] [P2] Two `emit_response_failed` writers (engine and server)
- **Where:** `engine/src/lifecycle.rs:162 emit_response_failed(&EngineInner, .., &EngineError, ..)`; `server/src/routes/core_pipeline.rs:154 emit_response_failed(&Arc<dyn EventBus>, .., &RouteError, ..)` (15+ call sites in `routes/*.rs`).
- **Now:** both build a `ProxyEvent::ResponseFailed` with the same field set; the server one covers errors raised before the engine runs (auth, host guard, JSON/content-type, unknown route, families that bypass the engine). No double write on the engine path was found: engine failures return `Err(map_engine_error(..))` and the route callers do not call `emit_response_failed` for them (verified in `chat.rs`).
- **Wanted:** plan step 6 "Shrink the server"; one writer for one ledger row.
- **Recommended default: share the row builder, keep both entry points.** Add `ResponseFailed::new(request_id, provider, model, error_kind, message, http_status, start: Instant)` (or a `fn response_failed_event(...)`) in `llm-proxy-storage`, called by both. A full merge is not possible: the error enums and their `error_kind` strings differ (`kind_name()` vs `route_error_variant_name`).
- **How:** implement the constructor in `crates/llm-proxy-storage/src/lib.rs` next to `ResponseFailed`; replace both struct literals; leave the `warn!` upstream-body logging in each (different error types).
- **Test:** a storage unit test `response_failed_new_computes_latency_ms` is low value; instead rely on existing `server/tests/core_pipeline.rs` ledger assertions staying green. State this in the commit.
- **Done when:** `grep -rn "ResponseFailed {" crates/llm-proxy-engine/src crates/llm-proxy-server/src` finds no struct literals.
- **Risk / notes:** pure refactor; skip entirely if it complicates `#[non_exhaustive]` struct construction across crates (then add a doc cross-reference in both functions instead).

### 03-13 [TEST] [P2] Per-family `TimeoutLayer` actually fires
- **Where:** `server/src/routes/mod.rs:156-159` `TimeoutLayer::with_status_code(408, family_request_timeout(config, family))`; existing config-only test `family_settings_parse_from_toml` (`server/tests/endpoint_families.rs:123`); server-wide timeout test `chat_completions.rs:1448 timeout_returns_normalised_json_408` (helpers `spawn_hanging_mock`, `state_with_provider_timeout`).
- **Now:** nothing proves a family's own timeout (rather than the server default) governs.
- **Wanted:** "`MAX_BODY_BYTES` and the `TimeoutLayer` become per-family settings."
- **How:** no production change.
- **Test:** `endpoint_families.rs`, two tests using a loopback mock provider (copy `spawn_hanging_mock` and a delayed-200 mock; build the provider the way `chat_completions.rs::state_with_provider_timeout` does):
  - `a_family_timeout_shorter_than_the_server_default_fires`: server `request_timeout = 60s`, `endpoint_families.chat_completions.request_timeout = 100ms`, upstream hangs; POST `/providers/<p>/v1/chat/completions`; assert 408, JSON body `error.type == "timeout_error"`, and wall time `< 5s`.
  - `a_family_timeout_longer_than_the_server_default_does_not_fire`: server `request_timeout = 100ms`, chat family `= 60s`, upstream answers 200 after 500ms; assert 200 (the family value, not the server value, applies).
  Both fail if the layer used the server-wide value.
- **Done when:** both test names pass.
- **Risk / notes:** use real time (no `start_paused`; real sockets).

### 03-14 [FIX] [P1] reqwest 0.13 (OTLP exporter) has no TLS backend
- **Where:** `apps/llm-proxy/Cargo.toml:51` `opentelemetry-otlp = { version = "0.33", default-features = false, features = ["http-json", "reqwest-blocking-client", "metrics"] }`; `Cargo.lock` `reqwest 0.13.5` (L3568) dependency list has no `rustls`/`hyper-rustls`/`webpki-roots`/`native-tls`; `apps/llm-proxy/src/otel_metrics.rs:25-36` accepts "an http(s) OTLP metrics endpoint".
- **Now:** verified from the lock: the second reqwest major carries no TLS stack, so an `https://` OTLP endpoint cannot connect (reqwest rejects/fails the TLS handshake). This is the exact BUG-23 area (one TLS stack, and it must work).
- **Wanted:** "`reqwest` uses one TLS stack (BUG-23)"; Done: "reqwest uses rustls only".
- **How:** enable a rustls feature on `opentelemetry-otlp` (check feature names in the 0.33 manifest on docs.rs or in `~/.cargo/registry` after fetch; expected `reqwest-rustls` or `reqwest-rustls-webpki-roots`) so reqwest 0.13 uses rustls with bundled roots (match the transport crate's `rustls-tls` choice: webpki roots, no native roots, no OpenSSL). Re-run `cargo shear`; confirm `Cargo.lock` still has no `native-tls`/`openssl`. If enabling rustls would pull a second rustls major or `aws-lc-sys` C build that breaks musl, instead document "OTLP endpoint must be http://" and make `otel_metrics.rs` reject `https://` at config time with a clear message (second-choice).
- **Test:** `apps/llm-proxy/src/otel_metrics.rs` unit test (there is a local-endpoint test ~L194): `an_https_endpoint_builds_an_exporter_without_a_tls_error` — building the exporter for `https://127.0.0.1:1/v1/metrics` returns `Ok` (build only, no network). If the reject-https fallback is taken: the test asserts the error message. Plus the CI lockfile grep from 03-7.
- **Done when:** `awk` over `Cargo.lock` shows the 0.13.x reqwest entry listing a rustls dependency (or the https rejection test passes).
- **Risk / notes:** build-time cost only; verify `cargo build --locked` for the app.

### 03-15 [TEST] [P2] Custom `Router` seam is exercised
- **Where:** `engine/src/traits.rs:42 trait Router`, `RegistryRouter`; wired by `EngineBuilder::router` (`lib.rs:437`); `grep 'impl Router for'` finds only `traits.rs:173`.
- **Now:** no test installs a non-default router.
- **Wanted:** trait with local default (D1) — a seam that is never exercised is untested.
- **How:** none.
- **Test:** `engine/tests/open_provider_set.rs`: `a_custom_router_chooses_the_target`: a `PinnedRouter` implementing `Router` that ignores `RouteQuery.provider` and returns a one-candidate `RoutePlan` for provider `"b"`; registry holds providers `a` and `b` with distinct fake upstreams; request route `"a"`; assert the response text comes from `b`'s upstream, `a`'s upstream saw zero calls, and the attempt row's `provider == "b"`.
- **Done when:** test passes.
- **Risk / notes:** `RoutePlan` fields are public; read `RegistryRouter::resolve` for the minimal valid plan (retry/affinity defaults).

### 03-16 [TEST] [P2] Pin the client re-encode failure delta
- **Where:** `server/src/routes/core_pipeline.rs` ~L336-370 (encode error -> `RouteError::Internal("client encode: ..")` after the engine settled); Done "Known behavior deltas".
- **Now:** behaviour matches the Done text by reading; no test pins it (searched `client encode`, `re-encode`).
- **Wanted:** Done: "returns the same 500 while the ledger keeps the truthful `ResponseCompleted` row and writes no failed row".
- **How:** none.
- **Test:** `server/tests/core_pipeline.rs`: `a_client_encode_failure_returns_500_and_keeps_the_completed_row`. Route an Anthropic-dialect client (`/providers/p/v1/messages`) to a provider of protocol `openai_chat_completions` (cross-format, so no passthrough) whose upstream returns a message with a `refusal` and no content; `protocol::client::anthropic::encode_response` errors on refusal (see test `encode_response_with_refusal_propagates_error`, `protocol/src/client/anthropic.rs` ~L2335, for the exact shape). Use a `RecordingBus`. Assert HTTP 500 body is the internal-error envelope, the bus holds one `ResponseCompleted` and no `ResponseFailed` for the request id.
- **Done when:** test passes.
- **Risk / notes:** if the refusal shape no longer errors, pick another core response that the target encoder rejects.

### 03-17 [DOC] [P2] Stale `llm-proxy-provider` comment
- **Where:** `crates/llm-proxy-protocol/tests/fixture_tests.rs:589` — "(mirror llm-proxy-provider run_stream_decode_fixture)".
- **Now:** names a crate that no longer exists.
- **Wanted:** no references to the removed crate in non-historical text.
- **How:** reword to "(mirror the adapters crate's stream-decode fixture runner)" or point at the real function (`grep -rn "run_stream_decode_fixture" crates`). Leave the `assert absence` hits in `protocol/src/client/*.rs` alone.
- **Test:** none.
- **Done when:** `grep -rn "llm-proxy-provider" crates apps --include=*.rs` returns only the two "absence" assertions.
- **Risk / notes:** test file, comment only.

## Rule violations to clean up
- `crates/llm-proxy-engine/src/lib.rs:84-90` `PROVIDER_NAME_REGEX` — no `expect()` in non-test code (even under `#[expect]`) — `Regex::new(r"^[a-z0-9_-]+$").expect("valid provider name regex")` — replace with a plain function: `!name.is_empty() && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))` inside `validate_provider_name` (L120) and delete the `LazyLock`/regex use (check `cargo shear` for the now-unused `regex` dep in engine; classification/error in plan 04 still use it until they are converted).
- `crates/llm-proxy-engine/src/lib.rs:355-362` `EngineBuilder::new` — `.expect("an empty provider registry is always valid")` — add `ProviderRegistry::empty()` (or `impl Default`) in `crates/llm-proxy-core/src/provider_registry.rs` (two empty `HashMap`s, infallible) and use it; drop the `# Panics` doc and the `#[expect]`.
- `crates/llm-proxy-transport/src/client.rs:62-67` `ProxyClient::new` (+ `impl Default` L53-56) — `Self::try_new().expect(..)` — DECIDE, default: delete `new()` and `Default`; non-test callers use `try_new()?` (engine `EngineBuilder::new` stores `Option<Arc<ProxyClient>>` and builds it in `build()` returning `EngineError::Internal`; `crates/llm-proxy-sdk/src/lib.rs:180` and `crates/llm-proxy-bridge/src/lib.rs:472` use `ProxyClient::try_with_timeouts(&config.server.upstream_timeouts)` which also fixes plan 04 item 04-8; `apps/llm-proxy/src/commands/serve.rs` and `doctor.rs` use `try_*` with `anyhow` context). ~84 call sites are tests/test modules (server/engine/mcp/sdk tests and `#[cfg(test)]` modules in `server/src/state.rs`, `routes/models.rs`, `routes/mod.rs`, `mcp/src/lib.rs`): mechanical replace with `ProxyClient::try_new().expect("default transport client builds")` (allowed in tests). `(also: plan 04 item 04-8)`
- `crates/llm-proxy-core/src/provider_config.rs:2219-2225` `validate_name` slug regex — `.expect("valid provider name slug regex")` — replace with `name.len() <= 64 && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))` (the existing `name.trim().is_empty()` check above keeps the empty case; ensure `{1,64}` bounds are preserved); remove the `LazyLock` if `regex` becomes unused in this function.
- `crates/llm-proxy-core/src/provider_config.rs:2631` `scheme_region` — `&endpoint[..idx]` — replace the body with `endpoint.split_once("://").map_or("", |(scheme, _)| scheme)`.
- After these edits run `cargo shear` (regex may become unused in engine) and the three AGENTS.md gates.

## Rejected findings
- "`llm-proxy-protocol`/`adapters` have network deps" — not a gap: manifests list no network crate and the lock closure contains only `tiktoken-rs` (non-network). The remaining work is the CI guard (03-7) and the tiktoken feature (03-6).
- "Engine is not 'without HTTP' because it depends on transport" — consistent with the plan: the plan row says "request lifecycle without HTTP" meaning no HTTP *server* types; the engine must call the transport. No change; the `axum` check in 03-7 is the enforceable form.
- K4 "no README in crates" — not a gap by itself: `[workspace.package] readme = "README.md"` is inherited and cargo resolves it at the workspace root; verification is folded into 03-8 (publish dry-run).
- K6 "crates.io names free" — needs network, cannot be verified statically; leave as a release-time check (not a code item).
- M1/M3/M4/M5/M6/M9–M13 slots — in order in the code; no work.
- G13/"fake upstream binds a port" — wording only (03-11), no code gap.
