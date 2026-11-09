# Plan 04 — Routing and failover: fix spec

Paths are relative to the repository root. Line numbers re-checked 2026-11-09; symbols are authoritative.
Test file shorthand: `T = crates/llm-proxy-engine/tests/pools_and_failover.rs` (helpers there: `spawn_upstream`,
`upstream_json`, `upstream_429_with_retry_after`, `member`, `pool`, `ladder_retry()` (max_attempts 2, backoff 10ms),
`build_engine`, `engine_request`, `attempts(&bus)`, `RecordingBus`).
Cross-plan: the `expect()`/slice clean-up of `engine/src/lib.rs`, `transport/src/client.rs`, `core/src/provider_config.rs`
is in the plan 03 spec; this spec owns `classification.rs`, `error.rs`, `provider_registry.rs`.

## Done — leave alone
- Attempt ledger: `attempts` table, `insert_attempt` (`storage/src/sqlite.rs`), `emit_attempt` / `record_attempt_failure` (`engine/src/lifecycle.rs`), `credential_id` columns; tests `attempt_rows_persist_one_per_upstream_attempt_with_the_credential`, `pool_survives_a_429_from_its_first_member_and_the_ledger_shows_every_attempt`.
- One classifier: `classification.rs` `classify`, `status_is_transient`, `parse_retry_after` (seconds + HTTP-date, clamped), `next_wait`; `class_for_ladder` auth override.
- Cooldowns/health: `health.rs` `Cooldowns`, `CooldownStore`, `AffinityTable`; `apply_cooldowns`, `member_cooldown_applies`; optional probe `probe.rs` (off when `probe_interval` is `None`).
- Pools: `RegistryRouter::resolve` pool branch (`engine/src/traits.rs`), `validate_pools` collision check (`core/src/provider_registry.rs:344`), serving at `/providers/{pool}/v1/...`; `routing_pools.rs` HTTP tests.
- Credential pools: `api_keys`, `credential_pool`, `resolve_attempt`, `credential_feedback`, ledger `credential_id`; tests `credential_pools_rotate_away_from_a_rejected_key`, `a_rejected_key_cools_the_key_not_the_member`.
- Hold budget / commit boundary / in-band errors: `buffer_pre_commit`, `commit_and_stream`, `pre_commit_failure`; tests `a_stream_that_fails_before_commit_fails_over`, `a_stream_that_fails_after_commit_ends_with_an_in_band_error_and_a_failed_row`, `the_hold_budget_commits_a_silent_stream_and_covers_the_gap`, `without_a_hold_budget_a_silent_stream_keeps_http_error_semantics`.
- Timeout config + validation: `UpstreamTimeoutsConfig`, `validate_upstream_timeouts`, `ProxyClient::try_with_timeouts`, production wiring `apps/llm-proxy/src/state.rs:96`; tests `upstream_timeouts_are_cross_validated`, `a_configured_first_byte_deadline_governs_the_send`, `stalled_upstream_triggers_idle_timeout`.
- Session ids on usage rows, sticky pinning, refuse policy (`refuse_switch`), bare `/v1/*` gating, exhaustion signals (`exhausted`, `error_policy`, `an_exhausted_pool_carries_the_retry_terminality_signals`).

## Work items

### 04-1 [FIX] [P0] Failover strip does not strip signed thinking, file ids, or anything on same-protocol failover
- **Where:** `engine/src/lifecycle.rs` `strip_provider_bound_state` (~L420-434), `Ladder::core_for` (~L508), callers at ~L1232 (`run_ladder_once`) and ~L2248 (`stream_attempt`); `adapters/src/client_wire.rs` `ClientWireRequest::same_format_as` / `force_core_encode` (L76, L108); `protocol/src/core.rs` `CoreContent::{Thinking{signature}, RedactedThinking, ToolUse{signature}, Image/Document/Audio/Video{source}, Opaque}` (L129-215).
- **Now:** two defects. (1) The function clears only `provider_hints.raw` (keeping `stream_options`); grep of `engine/src` for `signature`/`file_id` has zero hits, so `CoreContent::Thinking.signature`, `ToolUse.signature`, `RedactedThinking` and media `source` values holding provider file ids are sent unchanged to the new member. (2) Worse: the strip only rewrites the `CoreRequest`, but `lifecycle.rs` passes `req.inbound.wire.clone()` unchanged to `encode_request`; for a pool whose members share a protocol (the normal case) the adapter's same-format passthrough sends the client's RAW body (`wire.body`), so the stripped core is ignored entirely and the strip is a no-op for every same-protocol failover.
- **Wanted:** step 10: "Failover strips state tied to one account or provider, such as signed thinking and file ids, or refuses the switch." Done: "A provider switch strips provider-bound state (`strip_provider_bound_state`)."
- **How:**
  1. Change `strip_provider_bound_state(core) -> Result<CoreRequest, EngineError>`-style `StripOutcome`: walk `system` and every `messages[].content` (recurse into `ToolResult.content`): set `ToolUse.signature = None`; remove `Thinking` and `RedactedThinking` blocks from non-final assistant turns and drop their signature (a signed thinking block is rejected by another account, an unsigned one is rejected by Anthropic, so removing the block is the safe form); media blocks (`Image|Document|Audio|Video`) whose `source` JSON references a provider file (a `file_id` / `"type":"file"` / `file_uri` key at any depth of the `source` object) cannot be rewritten: return a refusal. Keep `provider_hints` handling and the `HINTS_KEPT_ACROSS_FAILOVER` allowlist. `CoreContent` is `#[non_exhaustive]`: use a wildcard arm that leaves unknown variants untouched.
  2. Refusal path: when a file reference is present and the switch would cross members, treat the candidate as skipped (continue the ladder to the next member, recording `EngineError::AffinityRefused`-style error as `last_error`) and, if no member remains, fail with a client error naming "file reference cannot move between providers". Implement beside `refuse_switch`.
  3. When the strip changed anything, set `wire.force_core_encode = true` on the per-attempt `wire` clone (both call sites) so the adapter re-encodes from the stripped core instead of passing the raw body through. When nothing was stripped keep passthrough (preserve byte-for-byte fidelity on the happy path).
  4. Make `core_for` return `(CoreRequest, bool /*stripped*/)` or add `Ladder::wire_for`.
- **Test:** `T`, new tests (anthropic-protocol fake upstreams that capture the request body; model the capture on `upstream_key_check` which records headers):
  - `failover_to_another_member_drops_signed_thinking_and_tool_signatures`: pool `[a, b]`, `a` returns 500, request core has an assistant turn with `Thinking{text, signature: Some("sig")}` and a `ToolUse{signature: Some("tsig")}`, and `wire.body` is the matching raw JSON (so passthrough is possible). Assert member `b`'s captured body contains neither `"signature"` nor the thinking block, while member `a`'s captured body (first attempt) DID contain them. Fails today (b receives the raw body).
  - `failover_refuses_to_move_a_file_reference_between_members`: media `source` `{"type":"file","file_id":"file_123"}`, `a` fails, assert `b` receives zero calls and the result is an error (not a 200).
  - `a_request_with_nothing_to_strip_keeps_same_format_passthrough`: no thinking/files; assert `b`'s captured body is byte-equal to the client body (regression guard that the strip does not force re-encode needlessly).
  - Unit test beside the function in `lifecycle.rs`/`#[cfg(test)]`: `strip_removes_provider_hints_but_keeps_stream_options` (existing behaviour pinned).
- **Done when:** the three engine tests pass; `grep -n "force_core_encode" engine/src/lifecycle.rs` shows the strip setting it.
- **Risk / notes:** Anthropic requires the thinking block of the final assistant turn when thinking is enabled with tool use; removing it can make the second member reject the request with a 400, which the ladder classifies terminal. If that proves common, prefer refuse for "thinking present in the last assistant turn" and strip only older turns. Mention in the commit. Sticky/affinity `Refuse` policy is unaffected.

### 04-2 [FIX] [P1] The ladder total deadline does not bound an in-flight attempt
- **Where:** `lifecycle.rs` `ladder_deadline` (~L543), `Ladder::{remaining,deadline_passed}` (~L475-483); non-stream send at ~L1243 (`inner.transport.client_for(&member).send(proxy_req).await`); stream `send_stream` (~L2278) and `buffer_pre_commit` (~L2540, awaiting the first event).
- **Now:** the deadline is checked before each attempt and clamps backoff only. An attempt waiting for headers/first event is bounded only by `first_byte`/`idle`, so one request can run past `total_timeout` (e.g. first_byte 60s inside a 5s total).
- **Wanted:** step 3 "one deadline across all attempts"; step 8 "the total deadline is new".
- **How:** add `async fn within_deadline<T>(deadline: Option<Instant>, fut: impl Future<Output = T>) -> Result<T, EngineError>` using `tokio::time::timeout_at(tokio::time::Instant::from_std(d), fut)`; map elapsed to `EngineError::UpstreamTimeout("retry ladder deadline exceeded during the attempt")`. Wrap (a) the non-stream `send(..).await`, (b) `send_stream(..).await`, (c) inside `buffer_pre_commit`'s `tokio::select!` add a branch that fires at `ladder.deadline` (before commit only; after commit the stream is bounded by `idle`, not `total`). Each timeout must flow through the existing failure path (`record_attempt_failure` / `pre_commit_failure`) so an attempt row is written; the next loop's `deadline_passed()` check then ends the ladder with `exhausted(..)`.
- **Test:** `T`: `the_ladder_deadline_bounds_an_attempt_in_flight`: member upstream handler awaits `futures::future::pending()`; pool/provider retry `total_timeout = 200ms`, `max_attempts = 3`, default `first_byte` (60s); assert `execute` returns `Err(EngineError::RetryExhausted{..})` (or `UpstreamTimeout` on a direct route) in `< 1s`, and `attempts(&bus)` has one `Failed` row with `error_kind` for a timeout. Streaming twin `the_ladder_deadline_bounds_a_stream_before_commit` with `upstream_silent_stream()` and `hold = 0`. Both fail today (they wait 60s / hit the test timeout).
- **Done when:** both tests pass; `grep -n "timeout_at" engine/src/lifecycle.rs` hits.
- **Risk / notes:** `UpstreamTimeout` is transient; confirm the deadline check prevents a further attempt (assert row count). Use `timeout_at` so a long backoff sleep is not double-counted.

### 04-3 [TEST] [P1] Server-level `upstream_timeouts.total` tightens the ladder
- **Where:** `lifecycle.rs` `ladder_deadline` (`config.server.upstream_timeouts.total` branch); `T:694 the_ladder_deadline_spans_all_attempts` covers only the route's `total_timeout`.
- **Now:** the server-total path is untested.
- **Wanted:** Done: "`ladder_deadline`, the tighter of the route's `total_timeout` and `server.upstream_timeouts.total`".
- **How:** none.
- **Test:** `T`: `the_server_total_tightens_a_looser_route_deadline`: build the engine with `test_config()` modified to `server.upstream_timeouts.total = Some(Duration::from_millis(100))` (validation is skipped when constructing config directly; keep `first_byte` default, so also set `first_byte = Duration::from_millis(50)` to stay self-consistent), route `total_timeout = 10s`, backoff 500ms, `max_attempts = 5`, always-500 upstream; assert `RetryExhausted` in `< 400ms`. Add the inverse `a_looser_server_total_does_not_loosen_the_route_deadline` (server total 10s, route 100ms, same assertion). Test fails if `ladder_deadline` ignored either bound.
- **Done when:** both tests pass.
- **Risk / notes:** needs `build_engine` to accept a config; add a `build_engine_with(config, providers, pools)` helper.

### 04-4 [FIX] [P1] Rate-limit reset parsing: scope by header name, parse RFC 3339 timestamps
- **Where:** `engine/src/classification.rs` `rate_limit_reset` (L135-150), `parse_duration_suffix` (L167), `DURATION_PAIR` regex (L157); caller `engine/src/error.rs:270` `retry_after_from_headers`; cooldown use `lifecycle.rs` `apply_cooldowns` (`advised`).
- **Now:** `retry-after` is parsed by name; every other response header value is tried as a Go-style duration with no name filter, and an RFC 3339 timestamp fails to parse. Anthropic's `anthropic-ratelimit-requests-reset` / `-tokens-reset` / `-input-tokens-reset` carry RFC 3339 timestamps, so Anthropic reset times are ignored (the cooldown falls back to the configured `cooldown`). The module doc claims "dialects" scoped by header name; the code is name-agnostic (an unrelated header like `x-trace: 5s` would extend a cooldown). Also note: the longest reset across all windows wins even when only one window is exhausted.
- **Wanted:** step 9 "Turn 429 and rate-limit headers into cooldowns based on real reset times".
- **How:**
  1. Split into `rate_limit_reset_at(headers, now: OffsetDateTime) -> Option<Duration>`; keep `rate_limit_reset(headers)` as a wrapper calling it with `OffsetDateTime::now_utc()` (it is used by `error.rs`).
  2. Name filter: a header counts when its lowercased name is `retry-after`, or contains `ratelimit` and `reset` (covers `x-ratelimit-reset-requests`, `x-ratelimit-requests-reset`, `anthropic-ratelimit-*-reset`, `x-ratelimit-reset`). Everything else is ignored.
  3. Value forms for non-`retry-after` names: Go duration (existing), else RFC 3339 via `time::OffsetDateTime::parse(value, &Rfc3339)` -> `(ts - now).max(0)`, clamped with `clamp_advised` (3600s). Past timestamps give `Some(Duration::ZERO)`.
  4. Replace the `DURATION_PAIR` regex with a hand-written scanner (digits, optional `.digits`, unit among `ns|us|µs|ms|s|m|h`; consume left to right, keep the existing semantics and the `µs` multi-byte handling via `str::strip_prefix`/`char_indices`) so the `#[expect(clippy::expect_used)]` and the regex disappear.
  5. Update the module/function docs to the real behaviour.
- **Test:** `classification.rs` unit tests (pure functions, injectable `now`): `an_rfc3339_reset_header_becomes_the_remaining_duration` (`anthropic-ratelimit-requests-reset: <now+90s>` -> 90s +-1s), `a_past_rfc3339_reset_is_zero`, `unrelated_headers_never_extend_a_cooldown` (`x-trace-id: 5s`, `content-type`), `the_longest_ratelimit_reset_still_wins`; keep the existing `rate_limit_reset_takes_the_longest_advertised_window` green. Plus `T` end-to-end in 04-5.
- **Done when:** `grep -n "DURATION_PAIR\|expect(" engine/src/classification.rs` shows no non-test hits; tests pass.
- **Risk / notes:** OpenRouter's `x-ratelimit-reset` can be epoch milliseconds (unitless): it stays unparsed (`None`), same as today; do not guess.

### 04-5 [TEST] [P1] A 429's cooldown lasts until the advertised reset
- **Where:** `lifecycle.rs` `apply_cooldowns` (~L606-630), `health.rs` `soonest_reset` / `provider_available`; helper `upstream_429_with_retry_after` in `T:54` is a router builder, not a test (Done cites it as if it were).
- **Now:** no test asserts the cooldown length equals the advertised reset.
- **Wanted:** Done: "a 429 with a reset time cools the member until it (`upstream_429_with_retry_after`)".
- **How:** none (after 04-4).
- **Test:** `T`: `a_429_cools_the_member_until_the_advertised_reset`: single-attempt pool `[primary, secondary]` (max_attempts 1), `primary` returns 429 with `Retry-After: 2`; run one request (fails over to secondary); then `engine`'s cooldown view (`CooldownStore`, reachable through the builder `.cooldowns(Arc<CooldownStore>)` kept by the test) must report `provider_available("primary", now)` false and `soonest_reset(&["primary"], now)` within 1.5s..2.5s; after `tokio::time::sleep(2.2s)` it is available again. Second test `an_anthropic_rfc3339_reset_header_sets_the_cooldown` with a 429 carrying `anthropic-ratelimit-requests-reset: <now+2s RFC 3339>` and no `Retry-After` (fails before 04-4: cooldown would be the configured 50ms). Rename the helper to avoid the misleading `upstream_429_...` test-sounding name only if cheap; also fix the Done sentence (04-12).
- **Done when:** both tests pass.
- **Risk / notes:** real-time sleeps of ~2s; keep them to these two tests.

### 04-6 [FIX] [P1] Guard-rejected (billed) attempt has no attempt row
- **Where:** `lifecycle.rs` `run_ladder_once` post-call guard loop (~L1303-1313, `return Err(LadderFailure{ error, usage, cost })`) and the `guard.transform_response(&mut core_resp).await?` (~L1337); `emit_attempt` is called only on success (~L1329) and in `record_attempt_failure`.
- **Now:** the provider was called and billed, usage reaches settlement, but the ledger shows no `AttemptCompleted` row for that attempt (also true if `transform_response` errors).
- **Wanted:** step 1 "one row per upstream attempt ... Failed attempts can still cost money."
- **How:** before returning the `LadderFailure` (and on the `transform_response` error), call `emit_attempt(inner, &AttemptRecord{ request_id, seq: ladder.seq - 1, route, provider: &member, model: &attempt.target.upstream_model, credential_id: attempt.credential_id.as_deref(), outcome: AttemptOutcome::Failed, error: Some(&error), latency: attempt_start.elapsed() })`. Do NOT call `record_attempt_failure` (no cooldown or credential feedback: a guard refusal is proxy policy, not an upstream fault). Credential feedback for this attempt stays `Succeeded` (the upstream answered).
- **Test:** `T` (or `engine/tests/response_masking_passthrough.rs`, which has a response guard): `a_guard_rejected_response_still_writes_a_failed_attempt_row`: rejecting `inspect_response` guard, fake upstream returns the OK body; assert `execute` returns `Err(InvalidRequest)`, `attempts(&bus)` has exactly one row with `outcome == Failed`, `http_status == Some(400)`, and the member is NOT cooling (`provider_available` still true). Fails today (zero rows).
- **Done when:** test passes.
- **Risk / notes:** the row's `error_kind` will be the guard error's kind; no schema change. Streaming twin: check `inspect_event` rejections (`lifecycle.rs` ~L2522) already end in a failed row via `record_failed_attempt`; if not, apply the same fix.

### 04-7 [FIX] [P1] `DEFAULT_UPSTREAM_HEADERS_TIMEOUT` still public and used by SDK/bridge clients
- **Where:** `transport/src/client.rs:20` (const), `:80` (`try_new`), `:123` doc, `:1870` test; re-export `transport/src/lib.rs:13`; fixed-60s clients built by `crates/llm-proxy-sdk/src/lib.rs:180` (`ProxyBuilder::engine`) and `crates/llm-proxy-bridge/src/lib.rs:472` (`from_config`); engine `EngineBuilder::new` (`engine/src/lib.rs:367`); MCP/tests use `ProxyClient::new()` too.
- **Now:** only the daemon wires the configured phases (`apps/llm-proxy/src/state.rs:96`). SDK and bridge users get the fixed 60s headers timeout and 10s connect regardless of `[server.upstream_timeouts]` in their `AppConfig`.
- **Wanted:** step 8 "This replaces the fixed `DEFAULT_UPSTREAM_HEADERS_TIMEOUT`".
- **How:** (1) SDK `ProxyBuilder::engine` and bridge `from_config`: build the client with `transport::ProxyClient::try_with_timeouts(&config.server.upstream_timeouts)` (map the `ProviderError` to `EngineError::Internal` / `BridgeError::config`). Bridge's `embedded_server_config()` already supplies a `ServerConfig` with defaults; if the bridge JSON has no timeout knob, defaults apply and that is fine. (2) `EngineBuilder::new(config)` (needs a client): build via `ProxyClient::try_with_timeouts(&config.server.upstream_timeouts)` inside `build()` (see the plan 03 spec rule-violation entry for `ProxyClient::new`; do both together). (3) Make the constant private (`const DEFAULT_UPSTREAM_HEADERS_TIMEOUT` used only by `try_new`) and remove it from the `pub use` in `transport/src/lib.rs:13`; keep `DEFAULT_STREAM_IDLE_TIMEOUT` only if still imported elsewhere (`grep -rn DEFAULT_STREAM_IDLE_TIMEOUT crates apps`). `try_new` remains the documented "default phases" constructor and should now delegate to `try_with_timeouts(&UpstreamTimeoutsConfig::default())` so the numbers live in one place (`UpstreamTimeoutsConfig::default()`: connect 10s, first_byte 60s, idle 60s).
- **Test:** `sdk/tests/mount_and_build.rs`: `the_builder_honours_configured_upstream_timeouts`: `AppConfig.server.upstream_timeouts.first_byte = 100ms`, a hanging fake upstream; the SDK-built engine returns a timeout error in `< 2s` (today it waits 60s; guard the test with `tokio::time::timeout(5s)`). Transport: update `client.rs:1870` to assert against `UpstreamTimeoutsConfig::default().first_byte` instead of the removed constant.
- **Done when:** `grep -rn "DEFAULT_UPSTREAM_HEADERS_TIMEOUT" crates apps` returns no match outside `transport/src/client.rs` (private use) and the plan doc; test passes.
- **Risk / notes:** semver: removing a `pub use` in 0.1.x is acceptable. `(also: plan 03 rule violations: ProxyClient::new)`.

### 04-8 [DECIDE] [P1] Per-family upstream-timeout overrides (claimed in Done, absent)
- **Where:** `core/src/provider_config.rs:366-372` `EndpointFamilyConfig { max_body_bytes, request_timeout }` (both client-facing); `docs/completed/04-routing-and-failover.md` Done bullet "Connect/first-byte/idle/total timeouts come from `server.upstream_timeouts` (per-family overrides in `server.endpoint_families`)".
- **Now:** no per-family upstream phases exist. `request_timeout` per family exists from plan 03 and bounds the whole client request.
- **Wanted:** plan 04 step 8 itself only requires server-wide configurable and validated phases; the per-family override appears only in the Done text.
- **Recommended default: narrow the claim (DOC).** Reason: the plan body does not ask for it, the per-family `request_timeout` already gives operators a per-family outer bound, and per-family upstream phases would need a per-family `ProxyClient` (reqwest freezes the connect timeout at client construction) for no stated requirement.
- **How (default):** edit the Done bullet to: "Connect/first-byte/idle/total timeouts come from `server.upstream_timeouts`, validated at config load (`validate_upstream_timeouts`); `server.endpoint_families.<name>.request_timeout` bounds the whole request per family." If the alternative is wanted: add optional `upstream_timeouts: Option<UpstreamTimeoutsConfig>` to `EndpointFamilyConfig`, validate with `validate_upstream_timeouts`, and apply first_byte/idle per request in the transport (connect stays global) — then add a test mirroring `a_configured_first_byte_deadline_governs_the_send`.
- **Test:** default: none. Alternative: engine/server test that a family with `first_byte = 100ms` times out while another family with defaults does not.
- **Done when:** `grep -n "per-family overrides" docs/completed/04-routing-and-failover.md` returns nothing.
- **Risk / notes:** none for the doc path.

### 04-9 [DECIDE] [P1] Heartbeats "before commit"
- **Where:** `engine/src/lifecycle.rs` `buffer_pre_commit` (hold branch ~L2573-2604 commits an empty head); `server/src/routes/core_pipeline.rs:40` `HEARTBEAT_INTERVAL = 3s`, `:686` `KeepAlive::new().interval(HEARTBEAT_INTERVAL)`; `execute_stream` awaits the first-byte/commit signal before the server builds any response.
- **Now:** no bytes reach the client before commit by design: failover and status codes are only possible before the response starts. With `hold > 0` the silence is held for `hold`, then committed with an empty head and keep-alive comments flow; with the default `hold = 0` nothing is sent until the first event or an error. There is no code path that heartbeats before commit.
- **Wanted:** step 6 "buffer for up to N seconds, then commit and send heartbeats" and step 7 "Cover silence before the commit". These conflict unless "cover" means "from the commit onward".
- **Recommended default: DOC (narrow) plus a test of what exists.** Reason: sending headers early would forfeit the pre-commit failover and status-code semantics that steps 6 and 12 depend on; the hold budget is the plan's own answer for silent upstreams, and 0 is the deliberate HTTP-error-semantics default.
- **How (default):** reword Done "committing an empty head when it expires in silence, heartbeats covering the gap" to "heartbeats start at the commit; with `hold = 0` (default) there is no response, hence no heartbeat, until the first event". Add the test below. If the alternative (early heartbeat) is wanted later, it needs a new config (`hold` semantics) and is a separate plan.
- **Test:** `server/tests/routing_pools.rs` (HTTP, real listener or `oneshot` + body stream): `a_committed_silent_stream_emits_keepalive_comments`: pool/provider with `hold = 200ms` and `upstream_silent_stream`-style upstream (copy from `T`), POST a streaming request, read the body for `> 3.5s` (one `HEARTBEAT_INTERVAL` + margin) and assert the status is 200 and the received bytes contain a keep-alive comment frame (axum emits `:\n\n`). Also `a_stream_with_no_hold_budget_sends_nothing_before_the_first_event`: `hold = 0`, silent upstream, `tokio::time::timeout(300ms, app.oneshot(req))` elapses (no response yet). Covers 7b (heartbeat wire assertion, currently untested).
- **Done when:** both tests pass; the Done sentence is reworded.
- **Risk / notes:** the first test takes ~4s; keep it single. If `HEARTBEAT_INTERVAL` is made configurable in `ServerConfig` later, shrink the test.

### 04-10 [TEST] [P1] Done-when: a pool survives a 5xx AND a connect error from its FIRST member, ledger shows every attempt
- **Where:** `T:455 pool_survives_a_5xx_then_a_connect_error_from_its_first_members` (members `broken`(500), `dead`(connect), `good`; `_bus` unused, response-only assert).
- **Now:** the connect error comes from the second member, not the first, and the ledger is never asserted. Only the 429 case (`T:407`) satisfies the done-when line.
- **Wanted:** "A pool request survives a 429, a 5xx and a connect error from its first member, and the ledger shows every attempt."
- **How:** none (code handles it, `lifecycle.rs` ~L1243-1288).
- **Test:** replace the test by two, using `ladder_retry()` (max_attempts 2):
  - `pool_survives_a_5xx_from_its_first_member_and_the_ledger_shows_every_attempt`: pool `[broken(500), good]`; assert response ok and `attempts(&bus)` = 3 rows: `(broken, 0, Failed, http_status 500)`, `(broken, 1, Failed, 500)`, `(good, 2, Succeeded)`, all with `route == "<pool>"`.
  - `pool_survives_a_connect_error_from_its_first_member_and_the_ledger_shows_every_attempt`: pool `[dead(127.0.0.1:9 / a closed port), good]`; same 3-row shape; `rows[0].http_status == Some(502)` and `error_kind` equals the connect-error kind (read `EngineError` mapping at `error.rs` ~L276-281). Bind a listener, take its port, drop it to get a reliably closed port instead of hard-coding port 9.
  - Keep a three-member `5xx then connect` test only if desired, now with its `bus` asserted.
- **Done when:** both tests pass; the old test name is gone or asserts the ledger.
- **Risk / notes:** the tests must fail if the ladder skipped recording a failed attempt.

### 04-11 [TEST] [P2] Direct provider route never fails over for 5xx and connect errors
- **Where:** `T:486 a_direct_provider_route_never_fails_over_or_wraps_the_error` (429 only); `routing_pools.rs:273` also 429.
- **Now:** only the 429 path of "never fails over" is pinned.
- **Wanted:** done-when 1 "behaves exactly as before and never fails over".
- **How:** none.
- **Test:** `T`: `a_direct_provider_route_surfaces_a_500_unwrapped_with_one_attempt` and `..._a_connect_error_...`: single provider, direct route; assert `EngineError::Upstream{status:500,..}` (not `RetryExhausted`), `attempts(&bus).len() == 1`.
- **Done when:** both pass.
- **Risk / notes:** a direct route with `[provider.retry]` configured retries the same member by design; the tests use a provider with `retry: None` (as `member()` does).

### 04-12 [DOC] [P2] Pool config shape and validation names in the Done text
- **Where:** `docs/completed/04-routing-and-failover.md` Done bullet 2; `core/src/provider_config.rs:3064` (broken intra-doc link), `:3076 validate_pool_config_against`, `core/src/provider_registry.rs:220-228,344-362`.
- **Now:** Done says "Pools live in `[pool.*]` config" and attributes the name-collision check to `validate_pool_config_against, provider_registry.rs`. Reality: one `[pool]` table per file in the providers directory (`PoolFile`, `provider_config.rs` ~L601), with sub-tables `[pool.retry]`, `[pool.health]`, `[pool.affinity]`; `validate_pool_config_against` checks structure and member names only; the provider-name collision is `ProviderRegistry::validate_pools` (called from `load_from_dir` ~L278 and `from_parts` ~L332). The doc comment at `provider_config.rs:3064` links to `validate_registry_pools`, which does not exist.
- **Wanted:** docs match code.
- **How:** rewrite the Done bullet: "Pools are `[pool]` tables in per-file TOML under the providers directory ... config validation rejects a duplicate member, an empty member list (`validate_pool_config_against`) and a pool name colliding with a provider (`ProviderRegistry::validate_pools`)". Fix the link to ``[`ProviderRegistry::validate_pools`]``. Also correct "(`upstream_429_with_retry_after`)" in the Done: it is a test helper; cite the real test from 04-5 once added. Also the "Done" sentence "the ladder consumes it through `next_wait` for backoff and cooldown decisions": `next_wait` serves backoff only; the cooldown uses the advised wait directly (`apply_cooldowns`). Reword to "backoff decisions; cooldowns use the advised reset directly".
- **Test:** none; `cargo doc -p llm-proxy-core --no-deps` must have no broken-link warning (AGENTS gate `clippy -D warnings` does not cover rustdoc, so run it once).
- **Done when:** `grep -rn "validate_registry_pools" crates docs` returns nothing; Done text updated.
- **Risk / notes:** none.

### 04-13 [TEST] [P2] Probe negative control and process-global counter
- **Where:** `T:1585 the_health_probe_clears_a_members_cooldown_when_it_recovers`, `T:1650 probed_upstream` (`static CALLS: AtomicU32` inside the function).
- **Now:** nothing shows that with the probe off the cooldown persists; the static counter is shared across tests in the binary (order-dependent if reused).
- **Wanted:** step 9 "optional synthetic probe, off by default".
- **How:** change `probed_upstream` to take an `Arc<AtomicU32>` argument created per test.
- **Test:** `T`: `without_a_probe_a_cooled_member_stays_cooled_after_the_upstream_recovers`: same upstream (429 then 200), `probe_interval = None`, cooldown 300s via `Retry-After`; after waiting longer than the probe interval used in the positive test, `provider_available` remains false.
- **Done when:** both tests pass and no `static CALLS` remains.
- **Risk / notes:** time-based, reuse the positive test's timings.

### 04-14 [TEST] [P2] A clean stream writes its terminal `ResponseCompleted` row
- **Where:** `lifecycle.rs` `record_stream_success` (~L1857), `emit_response_failed` (~L162), `record_client_cancelled` (~L1888); tests at `T:545`, `T:629` do not assert the completed row of a clean stream.
- **Now:** no test pins the clean-stream terminal row and its usage.
- **Wanted:** step 7 "guarantee a terminal usage event on every stream" (ledger-side: exactly one terminal row per stream).
- **How:** none.
- **Test:** `T`: `a_clean_stream_writes_exactly_one_completed_row_with_the_streamed_usage`: `upstream_ok_stream()` (usage input 7 / output 1), drain `tail`; assert the bus holds exactly one `ResponseCompleted` with `usage.input_tokens == 7`, `output_tokens == 1`, zero `ResponseFailed`, and the attempt row is `Streaming`/`Succeeded` per `AttemptOutcome` semantics. Add `a_stream_dropped_by_the_client_writes_one_terminal_row` (drop `tail` after the first event; exactly one terminal row, a cancellation, never zero or two).
- **Done when:** tests pass.
- **Risk / notes:** note in the test that the in-band error path emits a `CoreEvent::Error` but no `UsageDelta` to the client; the ledger row is the guarantee.

### 04-15 [DECIDE] [P2] Public paths for `classification`, `error_policy`, `health` (AGENTS: one path per item)
- **Where:** `engine/src/lib.rs:39-47` `pub mod classification; pub mod error_policy; pub mod health;` and `:67,71,73` `pub use classification::{..}`, `error_policy::{..}`, `health::{..}`; docs name the module paths (`docs/completed/04-routing-and-failover.md:59,106`, `05-admission-and-budgets.md:48`).
- **Now:** each of those items is reachable twice (`llm_proxy_engine::classify` and `llm_proxy_engine::classification::classify`). The same pattern exists for `admission`, `cache`, `catalog`, `error`, `traits` (only the three above were flagged, but the rule applies to all).
- **Wanted:** "Public items should be reachable through exactly one path. Do not add re-exports to paper over a refactor."
- **Recommended default:** keep the module paths (docs cite them and they namespace `classify`/`policy_for`), delete the root `pub use` lines for `classification`, `error_policy` and `health`, then fix importers (`grep -rn "llm_proxy_engine::{" crates apps bindings`, `sdk/src/lib.rs` re-exports). Leave `admission`/`cache`/`catalog`/`error`/`traits` re-exports for a separate sweep: those root names (`EngineError`, `Admission`, ...) are the crate's main API and moving them is a larger breaking change. Alternative: make the three modules private and keep root re-exports (then the doc references must change).
- **How:** edit `lib.rs`, compile the workspace, repair imports in `sdk`, `server`, `store-redis`, `store-postgres`, `mcp`, tests.
- **Test:** compile-only.
- **Done when:** `grep -n "pub use classification\|pub use error_policy\|pub use health" crates/llm-proxy-engine/src/lib.rs` is empty.
- **Risk / notes:** breaking for external users in 0.1.x, acceptable; skip if it touches > ~15 files and instead record it in `docs/lint-backlog.md`.

## Rule violations to clean up
- `crates/llm-proxy-engine/src/classification.rs:157-159` `DURATION_PAIR` — no `expect()` in non-test code — `Regex::new(r"^(\d+(?:\.\d+)?)(ns|us|µs|ms|s|m|h)").expect("valid duration regex")` — replace the regex with a hand-written scanner as part of 04-4.
- `crates/llm-proxy-engine/src/error.rs:296-302` `URL_REDACT_REGEX` — no `expect()` — `Regex::new(r"https?://\S+").expect("valid URL redaction regex")` — replace with a scan: find each `http://` / `https://` occurrence (`str::find` on the remaining slice via `split_at`-free iteration using `char_indices` or `match_indices("http")` then check the prefix with `strip_prefix`), replace through the next whitespace char with the existing placeholder. This is security-relevant (it redacts upstream URLs from client messages): keep the existing unit tests for `sanitize_upstream_error_body` and add `redacts_a_url_followed_by_punctuation_and_two_urls_in_one_message`. After this and 04-4 the engine `regex` dependency may be unused: run `cargo shear` (also see the plan 03 entry for `lib.rs:84-90`).
- `crates/llm-proxy-core/src/provider_registry.rs:466-470` — no slicing/indexing — `keys[..MAX_ADAPTERS_IN_ERROR].join(", ")` and `keys.len() - MAX_ADAPTERS_IN_ERROR` — replace with `keys.iter().take(MAX_ADAPTERS_IN_ERROR).copied().collect::<Vec<_>>().join(", ")` and `keys.len().saturating_sub(MAX_ADAPTERS_IN_ERROR)`; add a test that an unknown adapter among > MAX adapters names exactly MAX of them plus "(+N more)" (message text assertion).
- `engine/src/lib.rs:39,41,43` + `:67,71,73` multiple public paths — see 04-15.
- `transport/src/client.rs:62-67`, `core/src/provider_config.rs:2219-2225` — owned by plan 03 spec `(also: plan 03)`.

## Rejected findings
- "4c source-guard tests only grep strings" — the guard was tightened (forbids `pool.members` and `.route(` in `messages.rs`, requires `route_target_exists`), matching "allow this path only". Behavioural coverage exists in `routing_pools.rs` (`a_pool_route_survives_its_first_members_429_over_http`, `bare_v1_*`), so no item.
- "5xx+connect Done name `pool_survives_a_5xx_then_a_connect_error…` pins the done-when" — superseded by 04-10; the plan's done-when wording is the standard, not the test name.
- "8a connect timeout has no direct test" — not worth a test: a deterministic connect-timeout needs a non-routable address (network-dependent, flaky offline). Validation (`upstream_timeouts_are_cross_validated`) and wiring (`try_with_timeouts`) are covered.
- "Server-level `total` validated only against first_byte" — behaves as designed (`validate_upstream_timeouts`, total >= first_byte); the missing enforcement is 04-2, the missing test is 04-3.
- "Only two bare routes exist" — consistent with Done ("bare `/v1/*` is those two"); no item.
- The direct-route 5xx claim in DW1 is covered by 04-11 as a small test, not a code gap.
