# Plan 05 — Admission and budgets: fix spec

Repo: the repository root. Plan doc: `docs/completed/05-admission-and-budgets.md`. Read `AGENTS.md` first
(no `unwrap`/`expect`/indexing in non-test code, no `#[allow]`, no re-export papering, tests assert behaviour).
Line numbers were re-checked against the current tree; use the symbol names if they drift.

## Done — leave alone
- Status table: `crates/llm-proxy-engine/src/error_policy.rs` `STATUS_POLICY`, `policy_for`, `should_retry`; `EngineError::status_code` / `should_retry` in `error.rs`; server renderer `routes/error_response.rs` `route_error_response` (sets `x-should-retry` + `Retry-After`). The renderer path itself is correct; only the paths that skip it are broken (05-1).
- Key lookup for prefixed tokens: `middleware.rs` `split_prefixed_token`, `verifier_for` (binary search), `authenticate` prefixed arm, test `a_bad_prefixed_token_costs_the_same_no_matter_the_key_count`; issuance in `crates/llm-proxy-admin/src/keys.rs`.
- Allowlists: `admission.rs` `allowed_by`, `PolicyAdmission::reserve` (403, fail closed with no model).
- One atomic reservation: `Admission` trait (`traits.rs`), `PolicyAdmission`, `MemoryAdmissionBackend::admit` (one mutex), `UnsettledLeaseGuard`, `UnestimatableCostPolicy`, rpm/tpm/rpd windows, `usd_per_day`, concurrency caps -> 503 + `Retry-After: 1`.
- Refund on failure before cost (`settle` with `charged_usd: None`); cache hits never reserve and are recorded as hits (`lifecycle.rs` cache lookup precedes `admit`).
- Preflight: `preflight.rs` warn by default, enforce only past margin, estimate computed once in `lifecycle.rs` `admit` and shared with the claim.
- `llm-proxy pricing-sync` command shell (`apps/llm-proxy/src/commands/pricing.rs` `cmd_pricing_sync`, `per_token`, `apply_pricing`), all-unpriced groups report `cost: null` + `unpriced` count (`usage_query.rs` `summary_sync`).
- `UnsettledLeaseGuard` wiring in `lifecycle.rs`, `routes/families.rs`, `mcp/src/upstream.rs`.

## Work items

### 05-1 [FIX] [P0] Put 508 in the status table and stamp `x-should-retry` on every client-router response
- **Where:** `crates/llm-proxy-engine/src/error_policy.rs` `STATUS_POLICY` (rows 400..504, no 508); `crates/llm-proxy-server/src/loop_guard.rs` `refuse_looped_requests` (returns `StatusCode::LOOP_DETECTED` with no header); `crates/llm-proxy-server/src/host_guard.rs` `enforce_host_origin` closure `refusal` (403 `forbidden_request_source`, no header); `crates/llm-proxy-server/src/routes/mod.rs` `router()` (assembles layers; `normalize_error_responses` is innermost, inside loop/host/auth, so it cannot see those two responses); `routes/error_response.rs` `route_error_response`.
- **Now:** 508 is not a row, so `policy_for(508)` is `None`. The loop-guard and host-guard bodies are built inline with `json!` and carry no `x-should-retry`. Plan Done says the server "renders its `x-should-retry` row on every error". It does not for these two. (Client-auth 401 and fallback 404/405/408/413 DO go through `route_error_response`; do not touch them.)
- **Wanted:** plan Done-when: "Every status code the proxy produces is in the policy table, and a test enforces it", and "the server renders its `x-should-retry` row on every error".
- **How:**
  1. Add to `STATUS_POLICY`, in code order after 504: `StatusCodePolicy { code: 508, should_retry: false, note: "the upstream config loops back to this proxy; a retry loops again until the operator fixes the config" }`. The existing `table_rows_are_unique_and_ordered` test keeps ordering honest.
  2. Add `pub(crate) fn stamp_retry_policy(response: &mut Response)` in `routes/error_response.rs`: for `status >= 400`, if `x-should-retry` is absent, insert `"true"`/`"false"` from `llm_proxy_engine::policy_for(status).is_some_and(|r| r.should_retry)`; if the status has no row, insert `"false"` and `tracing::error!(status, "response status has no policy row")`. Never overwrite an existing header (RetryExhausted relies on its own `false`).
  3. Add one axum middleware `stamp_error_policy` in `routes/mod.rs` that runs `next.run(req)` then calls `stamp_retry_policy`. Install it in `router()` as the OUTERMOST layer, i.e. after `.layer(CatchPanicLayer::new())` and after the optional `usage_on_main` merge, so it wraps `lightweight`, `api`, the 404/405 fallbacks, loop/host guards and a main-listener `/usage` mount. Keep `normalize_error_responses` as is.
  4. Do not hand-edit the loop guard or host guard bodies; the layer covers them. Leave their JSON bodies unchanged (tests assert them).
- **Test:** `crates/llm-proxy-server/tests/host_guard.rs` (module `loop_guard_tests`): extend `a_request_carrying_the_hop_marker_is_refused_with_508` to assert header `x-should-retry == "false"`. Add in the same file `a_foreign_host_refusal_carries_the_retry_policy` (client mode `none`, `Host: evil.example`, expect 403, header `x-should-retry == "false"`). The router-level test in 05-2 is the enforcement for the whole class. Both host_guard tests must fail before steps 2-3 and pass after.
- **Done when:** `policy_for(508).is_some()`; the two updated tests pass; `grep -n "LOOP_DETECTED" crates/llm-proxy-server/src` still shows only `loop_guard.rs`.
- **Risk / notes:** header insertion on non-error responses must not happen (guard on `status >= 400`). Streaming error responses are fine (header only). `CatchPanicLayer` 500s get the header too, which is intended (500 row exists).

### 05-2 [TEST] [P0] Make "every status is a row" compiler-checked and router-level
- **Where:** `crates/llm-proxy-engine/src/error_policy.rs` tests `representative_errors()`, `every_engine_error_status_is_a_policy_row`, `should_retry_follows_the_table_except_exhausted_ladders`, `codes_stock_sdks_auto_retry_all_have_deliberate_rows`; `crates/llm-proxy-server/src/routes/error_response.rs` tests `every_route_error()`, `every_server_status_is_in_the_engine_policy_table`, `every_error_response_carries_the_tables_retry_choice`.
- **Now:** engine `representative_errors()` is a hand list that omits `RateLimited`, `BudgetExhausted`, `Overloaded`, `Forbidden`, `RetryExhausted`. The server `every_route_error()` is complete today (all 21 `RouteError` variants) but is also hand-maintained; `RouteError` is `#[non_exhaustive]`-marked but `pub(super)`, so same-crate matches can still be exhaustive. Nothing fails when someone adds a variant or emits a new `StatusCode` from a layer.
- **Wanted:** "a test enforces it".
- **How:**
  1. In each test module add `fn variant_slot(e: &EngineError) -> usize` (resp. `&RouteError`) that is an exhaustive `match` with NO wildcard arm returning a unique index per variant, and `const VARIANT_COUNT: usize = N`. Add a test `representative_errors_cover_every_variant` asserting that the set of `variant_slot` over the representative list equals `0..VARIANT_COUNT`. Adding a variant now breaks the build in the `match`, and after the author bumps the count the coverage assertion fails until a representative is added.
  2. Add the missing engine representatives: `RateLimited{..}`, `BudgetExhausted{..}`, `Overloaded{..}`, `Forbidden(..)`, `RetryExhausted{ last: Box::new(..) }`.
  3. Add router-level behaviour test `crates/llm-proxy-server/tests/error_policy_router.rs` `every_error_the_router_emits_is_a_policy_row_with_a_retry_header`: build the router (reuse the `build_router`/state helpers from `tests/host_guard.rs`), drive via `tower::ServiceExt::oneshot` these scenarios: hop marker (508), foreign Host under mode none (403), client-keys with no token (401), unknown path (404), wrong method (405), oversized body (413), plus one engine-originated error (unknown provider -> 404). For each response assert: `llm_proxy_engine::policy_for(status).is_some()` and header `x-should-retry` is exactly `"true"` or `"false"`. This is the check that fails today for 508 and the 403.
  4. Replace the mirrored assertions (see 05-12).
- **Done when:** `cargo test -p llm-proxy-engine error_policy` and `cargo test -p llm-proxy-server --test error_policy_router` pass; temporarily adding a dummy `EngineError` variant fails compilation of the test module.
- **Risk / notes:** keep `upstream_status_mapping_never_leaves_the_table` (400..600 scan) — it is a real property test.

### 05-3 [FIX] [P0] Stop bare (unprefixed) tokens from costing one argon2 run per key
- **Where:** `crates/llm-proxy-server/src/middleware.rs` `ClientAuth::authenticate` (`None =>` arm of `split_prefixed_token`), `verify_against`, struct `ClientAuth`; `crates/llm-proxy-core/src/provider_config.rs` `AuthConfig` (line ~687, `deny_unknown_fields`); `config.toml.example` lines ~98-103; doc comment on `ClientAuth` (~line 314).
- **Now:** a token with no `.` (or `.x`, `x.`) goes to the legacy arm, which builds a candidate list of every verifier and runs `verify_password` on each. 1,000 keys means 1,000 argon2 runs per unauthenticated request. Only the IP rate limiter (`would_allow`) limits it. No test measures this path.
- **Wanted:** Done-when: "Lookup cost for a bad token doesn't grow with the number of keys" (unconditional). Plan item 2: "Use a key-id prefix and one verification."
- **How (decision made — DECIDE resolved): refuse bare tokens by default, keep an explicit opt-in.**
  1. Add `legacy_bare_tokens: bool` (`#[serde(default)]`, default `false`) to `AuthConfig` with a doc comment: when `true`, a token without a key-id prefix is checked against every configured key (cost grows with key count). Update every `AuthConfig { .. }` struct literal in the workspace (grep `AuthConfig {`; tests in `middleware.rs`, `core_pipeline.rs`, `secret_abuse.rs`, `admin_surface.rs`, `state.rs` use `..Default::default()` or full literals — fix each).
  2. Store the flag in `ClientAuth` (`from_config`). In `authenticate`, in the `None =>` arm: if `!self.legacy_bare_tokens` return `None` with zero argon2 runs (keep the digest-cache lookup before it). If enabled, keep today's scan but cap it: skip the scan when `verifiers.len() > LEGACY_SCAN_MAX` (const, 16) and `tracing::warn!` once per process (use an `AtomicBool`) naming the cap.
  3. Add a bounded negative cache of recent failed-token digests (SHA-256 already computed) in `ClientAuth`: `Mutex<VecDeque<([u8;32], Instant)>>`, max 256 entries, 30 s TTL. A repeat of the same bad token returns `None` with no argon2. Only populate it on a definitive `None` from `verify_against` (not on spawn failure). Never store the token itself. Constant-time compare like `cached_key_id`.
  4. `config.toml.example` (~98-103): replace "A bare secret with no dot still works…" with "A bare secret with no dot is refused unless `[auth] legacy_bare_tokens = true`".
  5. Check the managed-key path: `crates/llm-proxy-admin/src/keys.rs` issues only prefixed tokens, so nothing else changes. Existing configs that hand out bare secrets must set the flag; call this out in the CHANGELOG/doc if one exists (`grep -n "legacy" docs/README.md`).
- **Test:** `middleware.rs` tests (uses `auth_with_keys(count)` and `bad_token_cost`):
  - `a_bare_token_costs_no_argon2_by_default`: for counts `[3, 60]`, `bad_token_cost(&auth, "no-dot-wrong")` is `0`.
  - `a_bare_token_with_the_opt_in_authenticates`: config with `legacy_bare_tokens = true`, 3 keys, the real bare secret authenticates; a wrong bare token costs `<= 3`.
  - `the_legacy_scan_is_capped`: opt-in with 60 keys, bad bare token costs `0` (over the cap).
  - `a_repeated_bad_token_is_not_verified_twice`: send `"laptop.wrong"` twice, `argon2_verifications()` rises by 1 total, not 2.
  - Migrate existing tests that authenticate with a bare secret (grep `authenticate("` and `Bearer ` in `tests/core_pipeline.rs:1996`, `secret_abuse.rs:79`, `middleware.rs`) to `<key-id>.<secret>`.
- **Done when:** `rg "bad_token_cost" crates/llm-proxy-server/src/middleware.rs` shows a bare-token case with expected `0`; `cargo test -p llm-proxy-server middleware` passes.
- **Risk / notes:** breaking change for bare-secret deployments (hence the flag). Hot reload swaps `ClientAuth` (`SwappingAuthPair`) so the negative cache resets on swap; a freshly issued key must not be blocked by a stale negative entry: because the cache lives inside the `ClientAuth` that is replaced on key changes, this holds; assert it in a test by rebuilding `ClientAuth` (do not share the cache across instances).

### 05-4 [FIX] [P1] Rebuild day budget and rpd from the ledger at startup (and narrow the claim)
- **Where:** `crates/llm-proxy-engine/src/admission_backend.rs` `MemoryAdmissionBackend` (`State.keys: HashMap<String, KeyState>`, `DayWindow`); `apps/llm-proxy/src/state.rs` (~line 174 `MemoryAdmissionBackend::new()`; second construction in the non-managed path via `PolicyAdmission::over_config_backend`); `crates/llm-proxy-storage/src/sqlite.rs` (`requests` table has `caller`, `received_at`, `cost_total`, `cache_hit`, `error_kind`; `attempts` table has NO cost column); `crates/llm-proxy-storage/src/usage_query.rs`.
- **Now:** budgets and windows live only in `MemoryAdmissionBackend`. A restart zeroes `usd_per_day` spend and the rpd counter; a key that spent its day budget can spend it again. Nothing reads the ledger. Plan item 5 says "using the attempt rows from plan 04". Attempt rows carry no cost, so that wording cannot be implemented as written; cost lives on `requests` rows.
- **Wanted:** plan item 5 + Done-when 1 ("can't overspend a budget by more than the reserved amount") must survive a restart for a single-node deployment with `[storage]`.
- **How (DECIDE resolved: implement the seed, narrow the doc):**
  1. Storage: add `pub fn spend_since(path, since_unix_ms) -> Result<Vec<CallerDaySpend>, UsageQueryError>` to `usage_query.rs` returning per `caller`: `requests` (count of rows with `cache_hit = 0`) and `cost_total` summed as `Decimal` (reuse `parse_cost`/exact decimal sum like `sum_costs_by_group`; sum in Rust, never SQL float). Window: since 00:00 UTC of today. Rows with NULL cost contribute 0 spend but count as requests.
  2. Engine: add `MemoryAdmissionBackend::seed_day(&self, now: OffsetDateTime, caller: &str, requests: u64, spent: MicroUsd)` that creates/updates the `KeyState` for today (use `KeyState::fresh` then set `day.requests`/`day.spent`). Only seeds the day window; do not seed minute windows (60 s horizon; document).
  3. App wiring in `apps/llm-proxy/src/state.rs`: after building the backend, when `[storage]` is set, call `spend_since` (blocking task) and `seed_day` per caller; log `info!(callers, "seeded daily budgets from the usage ledger")`; on query error `warn!` and continue (fail open is acceptable here because the store is also what `/usage` uses; state this in the code comment).
  4. Doc: edit plan 05 item 5 to "per-key budgets and the rpd window are rebuilt from `requests` rows at startup when `[storage]` is set; rpm/tpm windows restart empty; without `[storage]` everything resets on restart", and add that fact to `docs/data-class-policy.md` under the ledger row.
- **Test:** `usage_query.rs`/`crates/llm-proxy-storage/tests` `spend_since_sums_exact_decimals_and_skips_cache_hits` (seed three rows incl. a cache hit and a NULL cost; assert exact decimal total and request count). `crates/llm-proxy-engine/src/admission.rs` `a_seeded_day_budget_refuses_when_spent` (seed `spent` = budget, assert next `reserve` -> `BudgetExhausted` 402; seeded for yesterday rolls over: `seed_day` with a past date's `now`). App-level: `apps/llm-proxy/tests` if a harness exists; otherwise cover with the two tests above.
- **Done when:** the three tests pass; plan item 5 text matches the behaviour.
- **Risk / notes:** Redis/Postgres backends are already durable — do not seed them. Unflushed in-flight requests at crash time are lost from the ledger; acceptable and stated in the doc. Time source: use the backend's `now`, not wall clock, so tests are deterministic.

### 05-5 [FIX] [P0] Charge billed cost on pre-commit stream failure
- **Where:** `crates/llm-proxy-engine/src/lifecycle.rs`: `stop_with` (~2496-2511, `settle(inner, &stream.ticket, false, usage.as_ref(), None)`), `pre_commit_failure` (~2690-2720, two `settle(..., stream.last_usage.as_ref(), None)` calls for the Terminal and exhausted branches), `stash_prefix_usage` (~2474), struct `StreamLadder.last_usage` (~2354), callers of `stash_prefix_usage` (~2610, 2620, 2627) inside the attempt function that owns `ctx` (with `ctx.pricing`), `billed_cost` (~355).
- **Now:** a stream that fails before commit but whose buffered prefix carried provider-reported usage records the tokens (TPM window) and passes `cost: None`, so `usd_per_day` is never charged for tokens the provider billed. The doc comment of `stash_prefix_usage` says the funnel charges what the provider billed — false for cost.
- **Wanted:** plan item 6 "Budgets charge what providers bill, per attempt"; Done "Settlement charges exactly what the provider billed".
- **How:**
  1. Replace `StreamLadder.last_usage: Option<Usage>` with `last_billed: Option<(Usage, Option<Cost>)>`. Change `stash_prefix_usage(stream_ladder, pending, pricing: Option<&ModelPricing>)` to compute `billed_cost(&usage, pricing)` at stash time using the attempt's `ctx.pricing` (the member that produced the usage — not whichever member is last), and store both.
  2. The three settle sites pass `last_billed.as_ref().map(|(u, _)| u)` and `.and_then(|(_, c)| c.as_ref())` instead of `None`.
  3. Semantics stay "latest report wins; a report-less attempt never erases one" (unchanged). Add a one-line doc note that an EARLIER failed attempt's billed prefix is superseded and not charged (a limitation; the serving attempt's usage is still charged at success). Do not accumulate across attempts in this item.
  4. Update the `stash_prefix_usage` doc comment accordingly.
- **Test:** `crates/llm-proxy-engine/tests/cache_and_dedup.rs` already has `a_pre_commit_failure_charges_the_usage_its_prefix_already_reported` with `UsageRecordingAdmission`; extend it (or add `a_pre_commit_failure_charges_the_billed_cost_not_just_tokens`) so the recording admission also captures `outcome.cost` and the provider has pricing configured: assert `cost == Some(total = tokens * price)` computed by hand for the failing prefix, on all three exits: terminal error (e.g. 400 after a usage frame), exhausted ladder (transient 500 on the last member), and decode garbage after a usage frame. Fails today with `cost == None`.
- **Done when:** the extended test passes for the three exits; `rg "None,\s*\);" crates/llm-proxy-engine/src/lifecycle.rs` no longer shows a stream settle with `None` cost after `last_usage`.
- **Risk / notes:** unpriced models stay `cost: None` (existing warn in `PolicyAdmission::settle`). Synthetic/estimated usage is not priced (`billed_cost` guards on provenance).

### 05-6 [FIX] [P1] Family routes: reserve a real input estimate
- **Where:** `crates/llm-proxy-server/src/routes/families.rs` `run_family` (`AdmissionClaim { estimated_input_tokens: 0, max_output_tokens: None, .. }`, ~line 270; the comment above it says families have no estimator); `FamilySpec { kind, json }`; `prepare_body` returns `outbound_body: Vec<u8>`.
- **Now:** TPM windows and priced budgets see 0 input tokens at reservation time; only settlement in `account` charges real usage. A burst of large embeddings/rerank calls passes every window check, so "cannot overspend by more than the reserved amount" holds only against a zero reservation.
- **Wanted:** plan Done-when 1; item 4 (reserve then settle).
- **How:** compute `estimated_input_tokens` for JSON families (`spec.json == true`) as `ceil(outbound_body.len() / 4)` via `u64::try_from(..).unwrap_or(u64::MAX)` (no float; use `len().div_ceil(4)`). Non-JSON bodies (audio upload, files) stay `0` with a comment saying bytes are not tokens. Keep `max_output_tokens: None` (the key's `assumed_max_output_tokens` still applies). Replace the stale comment above the claim.
- **Test:** `crates/llm-proxy-server/tests/endpoint_family_handlers.rs` (has a recording admission or add one): `a_json_family_call_reserves_an_input_estimate_from_the_body` — POST an embeddings body of 4,000 bytes, assert the recorded `AdmissionClaim.estimated_input_tokens == 1000`; and `a_binary_family_call_reserves_no_input_estimate` for audio transcription (`0`). Second test (end to end): key with `tpm = 500`, an embeddings body of 4,000 bytes is refused 429 before reaching the upstream (upstream call counter stays 0).
- **Done when:** both tests pass; `rg "estimated_input_tokens: 0" crates/llm-proxy-server/src` has no hit in `families.rs`.
- **Risk / notes:** the estimate is deliberately coarse (documented chars/4 style). The MCP gateway claim (`crates/llm-proxy-mcp/src/upstream.rs`) is a separate site: leave it, but check it is not also hard-coded to 0 on a body that has size; if it is, apply the same rule (tag in the PR).

### 05-7 [FIX] [P1] Re-price the budget reservation on failover (`rekey`)
- **Where:** `crates/llm-proxy-engine/src/traits.rs` `AdmissionTicket` (carries `reserved`, `lease`, `caller` only) and `Admission::rekey`; `admission.rs` `PolicyAdmission::reserve` / `rekey` / `estimate_usd`; `admission_backend.rs` trait `AdmissionBackend::rekey` and `MemoryAdmissionBackend::rekey` (moves slots, leaves `Lease.reserved_usd`/`DayWindow.reserved` untouched); same-trait impls in `crates/llm-proxy-store-redis/src/lib.rs` (`rekey` ~665; `LeaseRecord.reserved_usd` ~127) and `crates/llm-proxy-store-postgres/src/lib.rs` (`rekey` ~741); callers `lifecycle.rs` ~1198 and ~2204.
- **Now:** `lifecycle.rs` `admit` prices the reservation with the FIRST candidate. When the ladder fails over to a member with higher per-token prices, `rekey` only moves concurrency slots; the day reservation stays at the first member's estimate, so the final charge can exceed the reservation.
- **Wanted:** Done-when 1 "can't overspend a budget by more than the reserved amount".
- **How:**
  1. `AdmissionTicket`: add `estimated_input_tokens: u64` and `max_output_tokens: Option<u64>` set in `PolicyAdmission::reserve` (needed to re-estimate). `AdmissionTicket` derives `Default`, so other constructors keep compiling.
  2. `PolicyAdmission::rekey`: when the caller has a budget, recompute `estimate_usd` for `(member, model)` from the ticket's token fields (reuse `estimate_usd` with a synthetic claim) and pass `new_estimate_usd: Option<MicroUsd>` to the backend. Unpriced new member under `Reject` policy returns `BudgetExhausted` (ladder skips the member like a saturated cap; `continue 'ladder` already handles any `Err`).
  3. `AdmissionBackend::rekey` gains a `new_estimate_usd: Option<MicroUsd>` parameter. Memory backend, under the same lock: compute `delta = new - lease.reserved_usd`; if `delta > 0` and `spent + reserved + delta > budget` return `BudgetExhausted` and change nothing; else update `lease.reserved_usd` and `day.reserved`. Need the budget in the call: also pass `budget_usd_per_day: Option<MicroUsd>`.
  4. Redis and Postgres backends: apply the same delta update to their lease record/day counter atomically inside their existing rekey transaction/script; if that is not feasible in the same change, make those backends reject a `new_estimate_usd` that differs from the lease's by returning an explicit `EngineError::Internal("rekey repricing unsupported")` ONLY when the delta is positive and a budget applies — never silently ignore. Prefer the real implementation.
  5. Update the trait doc and the plan Done text ("reservation follows the serving member").
- **Test:** `admission.rs` tests: `rekey_to_a_pricier_member_reprices_the_reservation` — pricing for member `a` = 1 USD/Mtok output, `b` = 10 USD/Mtok, budget sized so `a`'s estimate fits twice but `b`'s fits once: reserve on `a`, rekey to `b`, then a second reserve on `a` is refused 402 (reservation grew); and `rekey_to_a_member_that_would_exceed_the_budget_is_refused` (returns `BudgetExhausted`, lease unchanged, a later settle refunds the original estimate). Engine-level test in `tests/admission_budgets.rs`: pool `[cheap(500), pricey]` with budget tuned so the pricey member alone would breach; the request ends 402 (or serves from another member), never settles above budget.
- **Done when:** the tests pass; `MemoryAdmissionBackend::rekey` mutates `day.reserved`; `cargo test -p llm-proxy-store-redis -p llm-proxy-store-postgres --no-run` compiles.
- **Risk / notes:** touches the backend trait (3 implementors). Keep the memory behaviour as the reference and mirror it in the store crates' own tests if they have an admission conformance suite (`rg "AdmissionBackend" crates/llm-proxy-store-*/tests`).

### 05-8 [DOC] [P2] Preflight: narrow the claim, no provider token-count call
- **Where:** `docs/completed/05-admission-and-budgets.md` To-do item 8 ("Reject only with a safety margin or a provider token count"); `crates/llm-proxy-engine/src/preflight.rs` (only `Counter`; zero `count_tokens` calls in `crates/llm-proxy-engine/src`).
- **Now:** plan lists a provider-count rejection that does not exist. Done section already says margin only.
- **Wanted (DECIDE resolved: narrow, do not implement):** a provider count is an extra upstream round trip per request and the server already exposes `routes/token_count.rs` for clients that want it.
- **How:** edit item 8 to: "Warn by default. Reject only past a configured safety margin: non-OpenAI estimates use chars/4 and ignore tools, tool results and images. A provider token count is not used." Add the same limitation sentence to the module doc of `preflight.rs` if absent.
- **Test:** none (doc).
- **Done when:** `rg "provider token count" docs/completed/05-admission-and-budgets.md` has no hit that implies it is implemented.
- **Risk / notes:** none.

### 05-9 [FIX] [P1] Flag mixed priced/unpriced summary groups
- **Where:** `crates/llm-proxy-storage/src/usage_query.rs` `SummaryGroup` (fields `cost`, `unpriced`) and `summary_sync` (cost set whenever any priced row exists); consumers: `crates/llm-proxy-api` (`rg "SummaryGroup\|unpriced" crates/llm-proxy-api crates/llm-proxy-admin`).
- **Now:** a group with 3 priced and 2 unpriced completions reports `cost = priced sum` (looks like the full cost) with only `unpriced = 2` as a hint. Only an all-unpriced group is tested (`usage_api.rs` `summary_groups_by_provider_with_exact_decimal_costs`, group `prov-free`).
- **Wanted:** "Unpriced rows never show as $0 in summaries"; the summary must not present a partial sum as complete.
- **How:** add `pub cost_partial: bool` to `SummaryGroup`, set `true` iff `cost.is_some() && unpriced > 0` after the cost merge loop in `summary_sync`; document: "`cost` covers only priced rows when true". Serialization is automatic (`Serialize`). Update any admin UI/MCP tool JSON schema or docs listing summary fields (`rg "cache_hits" crates/llm-proxy-admin docs`).
- **Test:** `crates/llm-proxy-api/tests/usage_api.rs`: seed one provider `prov-mixed` with one priced completion and one unpriced completion (usage tokens, no cost) and assert `cost.total` equals the priced row's exact decimal, `unpriced == 1`, `cost_partial == true`; assert `prov_a` (all priced) has `cost_partial == false` and `prov-free` has `cost` null and `cost_partial == false`.
- **Done when:** the test passes.
- **Risk / notes:** additive JSON field; no breaking change.

### 05-10 [FIX] [P1] pricing-sync: never write a silent $0 for a missing dimension
- **Where:** `apps/llm-proxy/src/commands/pricing.rs` `pricing_entries` (`input.unwrap_or(Decimal::ZERO)`, `output.unwrap_or(Decimal::ZERO)`, and `cache_write`/`cache_read` `.unwrap_or(Decimal::ZERO)`); `apply_pricing` (omits zero cache fields); `cmd_pricing_sync` summary output.
- **Now:** a model priced on input only is written with `output = "0"`; budgets then charge nothing for output. The module promises "unpriced, never `$0`", and only the "neither dimension" case is handled.
- **Wanted:** plan item 9 "explicit unpriced state".
- **How:** require BOTH `input` and `output`; a model with exactly one of them is skipped (unpriced) and counted. Return the skipped-partial model ids from `pricing_entries` (change to return `(Vec<(String, ModelPricing)>, Vec<String>)` or a small struct) and print `skipped N model(s) with a partial price (unpriced): a, b` in `cmd_pricing_sync`. Cache dimensions absent from the catalogue stay `0` (a model that reports no cache pricing reports no cache tokens); add a comment saying so, and that `reasoning` is `0` because models.dev prices reasoning as output.
- **Test:** `pricing.rs` tests: `a_model_priced_on_one_dimension_only_stays_unpriced` (fixture entry with `cost.input` only -> absent from output and listed in the skipped list; the rewritten TOML has no entry for it) and extend `unpriced_models_stay_unpriced_never_zero`. Add the entry to `apps/llm-proxy/tests/fixtures/models-dev-sample.json` if the fixture is shared.
- **Done when:** both tests pass; `rg "unwrap_or\(Decimal::ZERO\)" apps/llm-proxy/src/commands/pricing.rs` only matches cache fields.
- **Risk / notes:** a model dropped from the file after a re-sync loses a previously hand-written price: the existing doc already says the sync replaces the whole table; keep.

### 05-11 [TEST] [P2] Engine-level proof that a key restricted to models fails closed with no model
- **Where:** `crates/llm-proxy-engine/tests/admission_budgets.rs` (`per_key_allowlists_are_enforced_through_the_engine` covers the 403 for a wrong model only); `admission.rs` `allowlists_fail_closed_without_an_identifiable_model` covers the policy level.
- **Now:** fail-closed without a model is tested only against `PolicyAdmission::reserve`.
- **Wanted:** item 3 "fail closed when a request has no identifiable model".
- **How/Test:** add `a_restricted_key_with_no_identifiable_model_is_refused_403_without_an_upstream_call`: a models-restricted key sends a request whose `requested_model` and resolved upstream model are empty (build via the engine request helper used by sibling tests); assert `EngineError::Forbidden`, status 403, and the upstream counter is 0.
- **Done when:** test passes.
- **Risk / notes:** if the engine always resolves a model, the test documents that the scenario is only reachable through the family route; then put the test in `tests/endpoint_family_handlers.rs` with a body lacking `model`.

### 05-12 [TEST] [P2] Replace mirrored tests with property/behaviour tests
- **Where:** `error_policy.rs` `should_retry_follows_the_table_except_exhausted_ladders` (recomputes expected from `policy_for`, the implementation's own source); `error_response.rs` `every_error_response_carries_the_tables_retry_choice` (same); `error_policy.rs` `codes_stock_sdks_auto_retry_all_have_deliberate_rows` (presence only).
- **Now:** these assert `x == f(table)` where `x` is computed from `f(table)`; they cannot fail on a wrong table.
- **Wanted:** AGENTS.md: "A test that ... mirrors the implementation's branches is worthless — test a property instead."
- **How:** replace with properties the plan states:
  - `budget_exhaustion_is_never_retried`: the 402 response from `RouteError::BudgetExhausted` has `x-should-retry: false`.
  - `an_exhausted_ladder_never_asks_the_client_to_retry`: for every representative error wrapped in `RetryExhausted`, header is `false` (keep existing logic; this is a stated rule).
  - `capacity_and_rate_refusals_ask_the_client_to_retry_later`: 429 (`RateLimited`) and 503 (`Overloaded`) say `true` AND carry a `Retry-After` when an advised wait is present (already partly in `advised_waits_render_as_retry_after_only_when_carried`).
  - `every_response_has_a_valid_header_value` (value in {"true","false"}) for the full `every_route_error()` list.
  - Delete `should_retry_follows_the_table_except_exhausted_ladders` and `codes_stock_sdks_auto_retry_all_have_deliberate_rows` if the router test in 05-2 and the properties above cover them.
- **Done when:** the mirrored tests are gone, the property tests pass, and mutating a table row from `false` to `true` for 402 fails `budget_exhaustion_is_never_retried`.
- **Risk / notes:** do after 05-2.

### 05-13 [FIX] [P2] Remove the double public paths in the engine crate root
- **Where:** `crates/llm-proxy-engine/src/lib.rs`: `pub mod admission; admission_backend; cache; catalog; classification; error; error_policy; headers; health; traits; types` AND `pub use admission::{..}`, `admission_backend::{..}`, `cache::{..}`, `catalog::{..}`, `classification::{..}`, `error::{..}`, `error_policy::{..}`, `headers::..`, `health::{..}`, `traits::{..}`, `types::{..}`. (`lifecycle`, `preflight`, `probe` have no `pub use`.)
- **Now:** e.g. `PolicyAdmission` is reachable as `llm_proxy_engine::PolicyAdmission` and `llm_proxy_engine::admission::PolicyAdmission`.
- **Wanted:** AGENTS.md "Public items should be reachable through exactly one path."
- **How (DECIDE resolved: root re-exports are the one path, because ~130 call sites already use them):** for each module in the list make it private (`mod x;`) and make its `pub use` list complete (add any `pub` item the module has that is not in the list, e.g. `LeaseId`-adjacent items, `UnsettledLeaseGuard` already there). Then fix callers that use a module path: `rg "llm_proxy_engine::(traits|error|error_policy|admission|admission_backend|cache|catalog|classification|headers|health|types)::"` (known: `llm_proxy_engine::traits`, `llm_proxy_engine::error`, `llm_proxy_engine::lifecycle` — `lifecycle` stays a pub module with no re-export, leave it). In-crate `crate::error_policy::` paths are unaffected. `#![deny(missing_docs)]` still applies to the re-exported items.
- **Test:** build only (`cargo clippy --workspace --all-targets --all-features -- -D warnings`); no new test.
- **Done when:** `rg "^pub mod (admission|admission_backend|cache|catalog|classification|error|error_policy|headers|health|traits|types);" crates/llm-proxy-engine/src/lib.rs` returns nothing, and the workspace builds.
- **Risk / notes:** also affects plan 06 (`cache`, tagged: also plan 06 for 06-13). If an out-of-workspace consumer needs a non-listed item, add it to the `pub use` rather than reopening the module. Run `cargo shear` not needed (no deps change).

### 05-14 [DOC] [P2] Align plan 05 prose with the code
- **Where:** `docs/completed/05-admission-and-budgets.md` Done section.
- **Now:** "the bare-secret scan remains as the legacy fallback" (changes with 05-3); "Settlement charges exactly what the provider billed" (true only after 05-5); item 5 "attempt rows" (see 05-4).
- **Wanted:** doc text equals shipped behaviour.
- **How:** after 05-3/05-4/05-5/05-7 land, edit: (a) bare tokens are refused unless `[auth] legacy_bare_tokens = true` (capped, negative-cached); (b) windows and day budget: seeded from `requests` rows at startup with `[storage]`, rpm/tpm start empty; (c) the reservation follows the serving member on failover; (d) an earlier failed attempt's billed prefix is not charged (only the latest failed or the serving attempt).
- **Test:** none.
- **Done when:** the four statements appear in the Done section.
- **Risk / notes:** do last; do not add design narrative (AGENTS.md).

## Rule violations to clean up
- `crates/llm-proxy-engine/src/error.rs:299-302` — "No `expect()` in non-test code" — `#[expect(clippy::expect_used, ..)] static URL_REDACT_REGEX ... Regex::new(r"https?://\S+").expect("valid URL redaction regex")` — The attribute form is the allowed escape in AGENTS.md, so this is low priority. If cleaned: replace `redact_urls` with a small non-regex scanner (find `http://`/`https://`, replace through the next whitespace with `[url-redacted]`) and delete the regex static; keep the existing `sanitize_*` tests as the behaviour check. (The same attribute pattern exists at `lib.rs` `PROVIDER_NAME_REGEX`, `provider_config.rs:~2225`, `adapters/src/anthropic.rs:31,40,60` — other plans; leave unless doing the sweep.)
- `crates/llm-proxy-engine/src/lib.rs` `pub mod X` + `pub use X::..` pairs — see 05-13.
- No `unwrap`/`panic`/indexing/`#[allow]`/`let _ =`-on-Result found in the non-test code of the plan-05 files.

## Rejected findings
- "Server `every_route_error()` omits `RateLimited`/`BudgetExhausted`/`Overloaded`/`Forbidden`" — wrong: `routes/error_response.rs` `every_route_error()` lists all 21 `RouteError` variants including those four. Only the ENGINE `representative_errors()` omits them (fixed in 05-2).
- "Admin/api 401 and MCP errors bypass `route_error_response`" as a table gap — the statuses used there (400, 401, 404, 405, 409, 500, 502, 202, 200) are all rows or non-errors, so the "every status is a row" half of Done-when holds. They are separate listeners/crates (`llm-proxy-api`, `llm-proxy-admin` depend on core only, not the engine, so cannot call `policy_for`; the MCP gateway has its own JSON-RPC error shape). DECIDE resolved: the `x-should-retry` rendering claim is narrowed to the client-facing proxy router; edit the plan's Done sentence to say "on every error from the proxy router" in 05-14. A `/usage` mount on the MAIN listener is covered by the outer layer in 05-1.
- "Client-auth 401 and layer 408/413 skip the renderer" — false: `require_client_auth` builds `RouteError::Unauthorized` and `normalize_error_responses` rewrites 408/413 through `route_error_response`.
- "Constant-cost property tested at two key counts only" (T2a) — adequate: the property is "unknown id = 0, known id = 1", independent of N; 3 vs 60 is enough.
- "Windows count requests at admission instead of attempt rows" as a standalone gap — folded into 05-4 (the attempts table has no cost/token columns, so the plan wording cannot be met literally).
- "Provider-token-count preflight missing" as a code gap — resolved as a doc narrowing (05-8).
