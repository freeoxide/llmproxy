# Plan 06 — Cache and dedup: fix spec

Repo: the repository root. Plan doc: `docs/completed/06-cache-and-dedup.md`. Read `AGENTS.md` first.
Line numbers were re-checked against the current tree; use symbol names if they drift.

## Done — leave alone
- Dedup retired: no `RequestDeduplicator`/`dedup_window`/dedup metric in `crates/*/src`; `ServerConfig` has `deny_unknown_fields` (`provider_config.rs` ~205) and test `load_app_config_rejects_the_retired_dedup_window_field`; `core_pipeline.rs` `identical_requests_are_never_answered_409_by_default`.
- Engine order: idempotency claim -> cache lookup -> single-flight -> admission (`lifecycle.rs` `run_once_attempt`, stream path ~1509-1666); caller-scoped identity `cache.rs` `CacheIdentity`; `SingleFlight`, `StreamShares`, `FlightLead`/`StreamLead` drop backstops; `IdempotencyStore` (claim/complete/release, replay, 409 `IdempotencyInFlight`, 400 body/route mismatch).
- Response cache: `MemoryCache` behind `Cache` trait (TTL, FIFO bound, generations), wiring `crates/llm-proxy-server/src/state.rs` ~465, tests `expired_cache_entries_are_misses`, `the_cache_evicts_oldest_entries_at_its_bound`, `one_callers_cached_response_is_never_returned_to_another`, `a_cache_hit_answers_where_the_dedup_window_used_to_409`.
- Hits record a row with `cost: None`, `cache_hit: true`, never reserve (`record_served_copy`; `admission_budgets.rs` `cache_hits_never_reserve_and_are_recorded_as_hits`, `served_copies_record_no_cost`).
- Opt-ins off store nothing (`with_every_opt_in_off_nothing_is_stored_and_every_request_runs`; `NoopBus` when no `[storage]`, so the "no `[storage]` = discarded" doc claim is true).
- Mutation phases: `adapters/src/mutation.rs` module doc; call order enforced in `anthropic.rs` (`encode_request`: edit/encode then `apply_prompt_hygiene` last), `openai_chat.rs`, `gemini.rs`, `responses.rs`; `strip_pointer`, `split_pointer` (`~1`/`~0`), `inject_anthropic_markers` budget of 4, newest message never marked, client markers kept (unit tests in `mutation.rs`).
- No fingerprinting; `AppState` engine cell is `Arc<EngineSlot>` shared by clones (`server/src/state.rs` ~72, `engine()` ~372).

## Work items

### 06-1 [FIX] [P1] Reject invalid `~` escapes in `strip_volatile` pointers at config load
- **Where:** `crates/llm-proxy-core/src/provider_config.rs` `validate_prompt_cache` (~2684-2706); caller ~2202; error `ConfigValidationError::InvalidStripPointer`; consumer `crates/llm-proxy-adapters/src/mutation.rs` `split_pointer` (~82-98).
- **Now:** the validator rejects only empty, no leading `/`, trailing `/` and `//`. `/a~2`, `/a~`, `/a~x` load, then `split_pointer` replaces only `~1` and `~0`, so the token stays literal and the pointer either matches nothing or a key literally named `a~2`. The doc comment and the plan ("Malformed pointers are refused at config load") promise otherwise.
- **Wanted:** RFC 6901: in a reference token `~` must be followed by `0` or `1`.
- **How:** in `validate_prompt_cache` add a helper `fn has_invalid_escape(pointer: &str) -> bool` that scans `pointer.char_indices()`/`chars().peekable()`; for each `~` the next char must be `'0'` or `'1'`, else invalid. No slicing/indexing. Treat as invalid with the existing `InvalidStripPointer` error (its message already says "RFC 6901"). Defence in depth: make `split_pointer` return `None` when the leaf (after the split) or any parent segment has a bad escape, so a pointer that bypassed config validation (tests, embedders building `PromptCacheConfig` directly) is a no-op, not a literal-key match.
- **Test:** `provider_config.rs` `prompt_cache_strip_pointers_are_validated_at_load`: add rejected cases `"/a~2"`, `"/a~"`, `"/~"`, `"/a~x/b"`, `"/a/b~"`; add an accepted case `["/a~0b", "/a~1b/c"]` asserting `validate_provider_config` is `Ok`. In `mutation.rs` add `strip_ignores_a_pointer_with_a_bad_escape`: request `{"a~2": 1, "b": 2}`, `strip_pointer(&mut r, "/a~2")` leaves it unchanged. Both fail before the change (the first set loads, the second removes the key).
- **Done when:** `cargo test -p llm-proxy-core prompt_cache_strip_pointers` and `cargo test -p llm-proxy-adapters mutation` pass.
- **Risk / notes:** existing configs using a literal `~` in a key must now write `~0`. `config.toml.example`/docs mentioning `strip_volatile` should get one sentence (`rg strip_volatile config.toml.example docs`).

### 06-2 [FIX] [P1] `inject_anthropic_markers`: only mark eligible blocks, and only the last one per message
- **Where:** `crates/llm-proxy-adapters/src/mutation.rs` `inject_anthropic_markers`, `mark_block`, `has_marker`, `existing_markers`.
- **Now:** (a) `mark_block` marks ANY JSON object, including `{"type":"text","text":""}` (Anthropic rejects `cache_control` on empty text blocks), `thinking` and `redacted_thinking` blocks (they cannot carry `cache_control` directly). A single bad marker turns an opted-in request into an upstream 400. (b) The message loop does `for block in blocks.iter_mut().rev() { budget -= mark_block(block, budget) }`, so after marking a message's last block it keeps marking the earlier blocks of the SAME message until the budget is spent. The doc comment says "the last content block of each earlier message". With one multi-block earlier message, all four markers land inside it and the rest of the stable prefix gets none. (c) The `system` step marks only the last system block even if it is empty/ineligible.
- **Wanted:** plan item 7 "marker injection within the four-breakpoint budget (newest message never marked)"; module doc "last content block of earlier messages".
- **How:** add `fn markable(block: &Value) -> bool`: object; `type` not in {`thinking`, `redacted_thinking`}; if `type == "text"` the `text` must be a non-whitespace string. Tool definitions (no `type`) are markable. Add `fn last_markable_mut(items: &mut [Value]) -> Option<&mut Value>` (`iter_mut().rev().find(|b| markable(b))`). Tools step and system step use it (so an ineligible last system block falls back to the previous eligible one). Messages step: per earlier message take `last_markable_mut`; if it already `has_marker`, count nothing and go to the next message; else mark it (budget -= 1) and go to the next message. `mark_block` keeps its `has_marker` guard.
- **Test:** `mutation.rs` unit tests:
  - `empty_and_thinking_blocks_are_never_marked`: earlier assistant message content `[thinking, text "answer", text ""]` -> marker on `"answer"`, none on the thinking or empty block; a `redacted_thinking`-only message gets no marker.
  - `at_most_one_marker_lands_in_each_message`: earlier user message with 3 text blocks, budget 4 -> exactly 1 `cache_control` in it; the next-older message also gets one.
  - `an_ineligible_last_system_block_falls_back_to_the_previous_one`.
  Existing tests (`markers_land_on_the_stable_prefix_within_the_budget`, `client_markers_count_against_the_budget_and_are_kept`, `more_client_markers_than_the_budget_add_none_and_do_not_panic`) must still pass unchanged.
- **Done when:** the three tests pass; `prefix_stability.rs` `opted_in_marker_injection_marks_only_unmarked_stable_blocks` still passes.
- **Risk / notes:** I could not re-verify Anthropic's current rules in-repo (no static source); the empty-text and thinking exclusions follow Anthropic's published prompt-caching rules — confirm against current docs before merging; excluding them is safe either way (a missed breakpoint only loses cache savings).

### 06-3 [DECIDE] [P2] String-form `system` / message `content` get no markers today
- **Where:** `mutation.rs` `inject_anthropic_markers` (`system` and `content` are read only with `as_array_mut`).
- **Now:** a request with `"system": "text"` or `"content": "text"` (very common simple clients) is silently skipped, so `inject_markers = true` does nothing for it.
- **Wanted:** plan item 7 "marker injection" for opted-in providers.
- **Recommended default:** do NOT convert string forms (conversion changes prompt bytes structurally; the opt-in covers marker injection, not rewriting content shape). Instead document the limit in the `inject_anthropic_markers` doc comment and in `docs/data-class-policy.md` prompt-cache section: "markers are injected only into array-form `system`/`content`". Add a unit test `string_form_content_is_left_alone` asserting a string-form request is byte-identical after injection.
- **Alternative if the operator wants it:** convert a non-empty string to `[{"type":"text","text":..}]` before marking, for the system prompt and earlier (non-newest) messages only.
- **Done when:** the doc sentence and the test exist.
- **Risk / notes:** none for the default.

### 06-4 [TEST] [P1] Prefix-stability tests for reasoning translation, cross-format, Gemini, Responses and Chat
- **Where:** `crates/llm-proxy-adapters/tests/prefix_stability.rs` (7 tests, all Anthropic same-format default path or hygiene); production edit points: `anthropic.rs` `edit_raw_anthropic_request` (reasoning, plus Edit 3 which strips `signature` from client `tool_use` blocks) and `encode_normalized_request`, `openai_chat.rs` `encode_request` (~433), `gemini.rs` `encode_request` (~359, same-format passthrough then cross-format), `responses.rs` `encode_request` (~462).
- **Now:** the plan's Done-when ("no enabled feature changes bytes before the last breakpoint unless opted in") is demonstrated only for Anthropic same-format with default features. The mutation.rs claim "phases 1-2 never alter prompt content: tools/system/messages survive byte-for-byte" is untested for phase 2 (reasoning translation) and untested for every cross-format encode.
- **Wanted:** the plan's done-when, across adapters.
- **How/Test:** add to `prefix_stability.rs` a helper `fn prefix_bytes(items: &[Value], k: usize) -> Vec<u8>` (serialize the first `k` conversation items) and these tests; each encodes a conversation of N turns and the same conversation extended by one turn through the SAME adapter/target and asserts the first N items serialize to identical bytes (the property providers cache on), with every phase-1/2 feature turned on and hygiene OFF:
  - `reasoning_translation_never_changes_the_prompt_prefix` (Anthropic same-format): client body carries `thinking {type:"enabled", budget_tokens: N}` and also a variant with `reasoning_effort: "high"`; target model with different thinking limits. Assert `tools`, `system`, `messages` of the client body equal those of the encoded body (`prefix_of(&client) == prefix_of(&encoded)`); only the top-level `thinking` may differ.
  - `foreign_signature_stripping_is_deterministic_across_turns` (Anthropic same-format): a `tool_use` block with `signature` in turn 1; encode turn-2 and turn-3 bodies; the encoded turn-1 message bytes are identical in both and have no `signature`. (This documents the one same-format content edit; see 06-10 for the doc note.)
  - `extending_a_conversation_keeps_the_encoded_prefix_cross_format` for each pair: OpenAI Chat client -> Anthropic target (`messages` and `system`), Anthropic client -> OpenAI Chat target (`messages`), Anthropic client -> Gemini target (`contents`), Chat client -> Responses target (`input`), Responses client -> Anthropic target. Include a tool call + tool result turn and a system prompt.
  - `gemini_same_format_passthrough_keeps_contents_byte_identical`, `responses_same_format_passthrough_keeps_input_and_instructions_byte_identical` (raw client body in, assert `contents` / `input` + `instructions` equal ignoring the model alias).
  - `chat_same_format_passthrough_keeps_messages_byte_identical`.
  Reuse the `target()` builder (change `protocol`, `endpoint`, `upstream_model` per adapter like the existing `chat_adapter_strips_volatile_bytes_but_injects_no_markers` does). All of these must pass today unless they expose a real prefix drift — if one fails, treat the drift as a bug, fix it in the adapter (not the test), and note it in the PR.
- **Done when:** the new tests exist and pass; `rg -c "#\[test\]" crates/llm-proxy-adapters/tests/prefix_stability.rs` is >= 14.
- **Risk / notes:** uses only public adapter APIs already used in this file. Keep fixtures small and inline.

### 06-5 [TEST] [P1] Make the shared-fate and leader-vanish tests prove sharing (counters), and fix the lazy-future bug
- **Where:** `crates/llm-proxy-engine/tests/cache_and_dedup.rs`: `a_single_flight_leader_failure_fails_its_joiners` (~392, uses `CountingUpstream::failing()` whose delay is `Duration::ZERO`), `SlowRefusingAdmission` (unit struct, ~1191) with `an_admission_refusal_reaches_single_flight_joiners` (~1207; `calls` Arc is never wired to anything, so `assert_eq!(calls, 0)` is vacuous) and `a_streaming_leaders_admission_refusal_reaches_its_joiners` (~1483), `a_streaming_leaders_guard_rejection_reaches_its_joiners` (~1525, `SlowRejectingGuard`), `a_single_flight_follower_reexecutes_when_its_leader_vanishes` (~1240), `a_streaming_follower_reruns_when_its_leader_vanishes_before_the_first_byte` (~1557), `a_streaming_joiner_gets_the_leaders_pre_stream_error_not_a_broken_200` (~1275).
- **Now:**
  1. Vanish tests: `let joiner = engine.execute(...)` is an `async fn` future, which is LAZY. It is not polled during `tokio::time::sleep(40ms)`, so the joiner never joins the flight; after `leader.abort()` it runs as a fresh leader. The test ends with 2 upstream calls (leader + joiner) that an independent run also produces, so the follower-retry branch (`lifecycle.rs` `run_once` ~941-959, `execute_stream` ~1451-1485) is never exercised.
  2. Failure/refusal tests assert error kinds that a deterministic independent run also returns; the failing upstream has zero delay so the second request may arrive after the first finished.
  3. The admission-refusal tests have no reserve counter, so "the joiner never reserved" is unproven.
- **Wanted:** plan: "A leader that is refused or fails before its first byte fails its followers with the same error; a leader that vanishes ... makes its followers re-run the request once."
- **How/Test (edit the existing tests):**
  - Vanish (both): `let joiner = tokio::spawn({ let engine = engine.clone(); async move { engine.execute(...with_request_id("req-joiner")).await } });` BEFORE the sleep. After 40 ms and before `leader.abort()`, assert `upstream.calls.load() == 1` (the joiner is parked on the leader's flight; an independent run would already be at 2) and `!joiner.is_finished()`. Abort the leader, await the joiner, then assert `calls == 2`, response text, and the joiner's completed row. Same for the streaming twin (spawn `execute_stream` and collect inside the task or await the handle then `collect_stream`).
  - `a_single_flight_leader_failure_fails_its_joiners`: add `CountingUpstream::failing_after(delay: Duration)` (status 500 with the delay). First run one lone request on a fresh engine to learn `lone = calls` (the retry ladder may retry); then on another fresh engine run leader+joiner concurrently and assert `calls == lone` (shared) — not `2 * lone` — plus both get `EngineError::Upstream { status: 500, .. }`.
  - Admission refusal (non-streaming and streaming): give `SlowRefusingAdmission` a `reserves: Arc<AtomicUsize>` incremented in `reserve`; assert `reserves == 1` after leader+joiner (the joiner never reached admission). Delete the dead `calls` variable or wire it to the upstream.
  - Guard rejection (streaming): add an `inspections: Arc<AtomicUsize>` to `SlowRejectingGuard`; assert it ran for one request only.
  - Audit every other test in the file for the same lazy-future pattern: `rg -n "let joiner = engine" -A3 crates/llm-proxy-engine/tests/cache_and_dedup.rs` — any joiner created and then separated from its first poll by a `sleep` must be `tokio::spawn`ed or polled via `futures::future::join`.
- **Done when:** each edited test asserts a call/reserve counter that would be doubled by independent execution; reverting the single-flight join in `lifecycle.rs` (treat every acquire as Lead) makes them fail.
- **Risk / notes:** timing: keep generous margins (leader upstream 150-200 ms, observe at 40 ms). Do not use `tokio::time::pause` with real sockets.

### 06-6 [TEST] [P1] Registry-drain test: nothing is retained after completion
- **Where:** `crates/llm-proxy-engine/src/cache.rs` `SingleFlight`/`StreamShares` Debug impls (`in_flight` count, ~276 and ~480), `release` (~310, ~517), `FlightLead::drop`, `StreamLead::drop`; `Engine` derives `Debug` (prints `singleflight` and `stream_shares` through `EngineInner`).
- **Now:** plan Done says "Nothing is retained after completion". No test asserts the registries empty.
- **Wanted:** that statement.
- **How/Test:** in `cache_and_dedup.rs` add helper `fn in_flight(engine: &Engine) -> (usize, usize)` that parses `format!("{engine:?}")` for `SingleFlight { in_flight: N }` and `StreamShares { in_flight: M }` (use `str::find` + `parse`, no regex dependency). Add `single_flight_registries_drain_after_every_kind_of_end`:
  1. mid-flight probe: while a 150 ms leader is running, `in_flight(&engine).0 == 1` (proves the probe is not vacuous);
  2. after a successful shared run (4 callers): `(0, 0)`;
  3. after a failing shared run (`failing_after`): `(0, 0)`;
  4. after a leader aborted mid-flight and its joiner re-ran: `(0, 0)`;
  5. streaming: after a clean shared stream and after the `mid_stream_failure_upstream` stream: `(0, 0)`.
  If parsing Debug output proves brittle, instead add `#[doc(hidden)] pub fn in_flight_counts(&self) -> (usize, usize)` on `Engine` — prefer the Debug route first (no new public API; AGENTS.md "one path" rule).
- **Done when:** the test passes; commenting out `SingleFlight::release` in `FlightLead::drop` makes it fail.
- **Risk / notes:** await the spawned tasks fully (and `collect_stream`) before sampling.

### 06-7 [TEST] [P1] Idempotency replay and in-flight 409 never reach admission
- **Where:** `cache_and_dedup.rs` `idempotency_key_replays_after_completion_and_409s_while_in_flight` (~623) and `a_streaming_outcome_replays_as_a_stream_for_a_matching_retry` (~792); `lifecycle.rs` `idempotency_step` (~880-905), claim at ~979, `admit` at ~1094.
- **Now:** plan Done: the key "is claimed at slot 4, before cache, single-flight and budget checks". Ordering is only indirect (upstream call count). No test proves a replay or a 409 skips the budget reservation, i.e. does not consume rpm/tpm/budget.
- **Wanted:** that ordering.
- **How/Test:** add `struct CountingAdmission { reserves: AtomicUsize, settles: AtomicUsize }` (impl `Admission`: `reserve` increments and returns `AdmissionTicket::default()`, `settle` increments). Extend (or add `an_idempotent_replay_and_an_in_flight_409_do_not_reserve_budget`) the idempotency test: build the idempotent engine with `CountingAdmission` (via `engine_with(.., Some(admission), ..)` using `idempotency_config()`), run first request (spawned, 150 ms), the conflicting retry (409), then the replay. Assert `reserves == 1` and `settles == 1` at the end, and that the 409 attempt did not change either (sample after the 409). Add the streaming variant on `a_streaming_outcome_replays_as_a_stream_for_a_matching_retry`.
- **Done when:** tests pass; moving `idempotency_step` below `admit` in `lifecycle.rs` makes them fail.
- **Risk / notes:** none.

### 06-8 [TEST] [P1] Cache-hit metric asserted on the hit path
- **Where:** `crates/llm-proxy-core/src/metrics.rs` `record_cache_hit` (~132), export `llm_proxy_cache_hits_total` (~302, help text already says "cache hit, single-flight join, idempotent replay"); called from `lifecycle.rs` `record_served_copy` (~918); only direct unit tests exist (`metrics.rs` ~494, 567, 600).
- **Now:** nothing proves a real hit increments the counter.
- **Wanted:** plan item 4 "bump a cache-hit metric".
- **How/Test:** `cache_and_dedup.rs` `a_cache_hit_a_single_flight_join_and_a_replay_each_bump_the_hit_metric`: (1) cache engine: first request -> `engine.metrics().get_snapshot().cache_hits == 0`; identical second request -> `== 1`; (2) single-flight: 4 concurrent identical requests -> `cache_hits == 3`; (3) idempotency engine: first -> 0, replay -> 1. Use fresh engines per case.
- **Done when:** passes; removing the `record_cache_hit()` call in `record_served_copy` fails it.
- **Risk / notes:** the counter intentionally counts all served copies (help text); do not rename it.

### 06-9 [TEST] [P2] Direct regression test for the shared engine cell
- **Where:** `crates/llm-proxy-server/src/state.rs` tests (`clone_shares_arc_references` ~837 checks only `metrics` and `build`); `core_pipeline.rs` ~3848 reads `state.engine()` twice from the SAME `state`.
- **Now:** the BUG the plan's last bullet describes (per-clone cell = fresh engine per request) is caught only indirectly by `core_pipeline.rs:2956 response_cache_opt_in_serves_the_second_identical_request`.
- **Wanted:** a direct test.
- **How/Test:** in `state.rs` tests add `clones_share_one_engine_instance`: build a state with a minimal valid provider (reuse helpers `make_app_config`, `make_provider_registry` used by the neighbouring test; if the engine cannot build with the empty registry, add one mock provider), `let a = state.clone(); let b = state.clone();` then `let ea = a.engine().expect(..); let eb = b.engine().expect(..); assert!(Arc::ptr_eq(&ea, &eb))`. Order matters: call `a.engine()` first (lazy build) then `b.engine()` — a per-clone cell yields two builds. Also assert `Arc::ptr_eq(&state.engine()?, &ea)`.
- **Done when:** passes; changing `engine: Arc<EngineSlot>` to a plain `EngineSlot` (per-clone) fails it.
- **Risk / notes:** hot-reload swaps the slot; do not call the reload path in this test.

### 06-10 [TEST] [P2] Known limitation X1: leader disconnect after commit
- **Where:** `lifecycle.rs` `replay_next`/`joined_stream` (~3144-3215) pushes `CoreEvent::Error` "the shared upstream stream ended early" when the shared outcome is `Err` with no terminal; plan Done documents this limitation.
- **Now:** untested.
- **Wanted:** pin the documented behaviour.
- **How/Test:** `cache_and_dedup.rs` `a_leader_dropped_after_commit_ends_its_joiners_with_collected_events_and_an_error`: upstream streams message_start immediately then stalls (`tokio::time::sleep` 500 ms before the terminal); spawn leader (`execute_stream`, read the first event so it is committed, then drop its stream), spawn a joiner that joined before the drop; collect the joiner's stream; assert it contains the events collected so far (the `MessageStart`), ends with a `CoreEvent::Error` whose message mentions "ended early", and contains no `MessageStop`. Assert upstream calls == 1.
- **Done when:** passes.
- **Risk / notes:** timing as in 06-5; if the joiner cannot join after the commit deterministically, spawn it before the leader reads its first event.

### 06-11 [DOC] [P2] Data-class policy wording: TTL start and the anonymous caller
- **Where:** `docs/data-class-policy.md`: response cache "Retention: `ttl` (default 10m) from storage" (~line 32), "Served to: the same caller only" (~34), semantic-cache block (~88/90), idempotency "Served to" (none stated); caller identity row (~15).
- **Now:** (a) "from storage" reads ambiguously next to idempotency's "from completion" (the clock does start when the entry is stored, i.e. when the response completes: `cache.rs` `MemoryCache::store` stamps `Instant::now()`); (b) under `auth.mode = "none"` every request is the `anonymous` caller (`middleware.rs` `CallerId::anonymous`, as_str -> `ANONYMOUS_CALLER`), so "same caller only" means "everyone on that loopback listener" there. The server refuses a non-loopback bind without client keys, which is why this is acceptable, but the doc does not say it.
- **Wanted:** doc states what is stored, for how long, and who it is served to.
- **How:** change "from storage" to "from the moment it is stored (when the response completes)" in the response-cache and semantic-cache blocks; add to the response-cache, idempotency and semantic-cache "Served to" lines: "Under `auth.mode = \"none\"` every request is the `anonymous` caller, so these stores are shared by all clients of that (loopback-only) listener; use client keys to separate callers." Add the same sentence to `docs/completed/06-cache-and-dedup.md` item 4 Done bullet.
- **Test:** none (doc).
- **Done when:** `rg -n "anonymous" docs/data-class-policy.md` shows the caveat in the three sections.
- **Risk / notes:** no code change.

### 06-12 [DOC] [P1] `remodel-proxy.md` still documents `dedup_window`, which now fails to load
- **Where:** `docs/completed/remodel-proxy.md` ~lines 125-132: the TOML example has `dedup_window = "0ms"` and prose "`dedup_window = "0ms"` disables identical-request rejection."
- **Now:** copy-pasting the example makes the config fail with `unknown field dedup_window` (`ServerConfig` denies unknown fields).
- **Wanted:** docs match the code.
- **How:** delete the `dedup_window = "0ms"` line from the example and replace the sentence with "Identical requests are never rejected; see plan 06 for single-flight, `Idempotency-Key` and the opt-in response cache." Leave `docs/research/**` (historical audit snapshots; `rg dedup_window docs` otherwise lists only those and plan 06 itself).
- **Test:** add `config.toml.example`-parse guard only if one exists; otherwise none. A cheap guard: `rg -n "dedup_window" --glob '!docs/research/**' --glob '!target/**' .` must only match `provider_config.rs` (the rejection test) and `docs/completed/06-cache-and-dedup.md`.
- **Done when:** that grep matches only those two files.
- **Risk / notes:** none.

### 06-13 [FIX] [P2] Double public path for the cache types (also: plan 05)
- **Where:** `crates/llm-proxy-engine/src/lib.rs`: `pub mod cache;` plus `pub use cache::{IdempotencyClaim, IdempotencyStore, MemoryCache, SingleFlight, StoredOutcome, StreamShares};`.
- **Now:** each of those types has two paths. Same pattern for the other modules; the fix is crate-wide and specified in plan 05 item 05-13.
- **Wanted:** AGENTS.md "reachable through exactly one path."
- **How:** do as 05-13 (make `cache` a private `mod` and keep the root re-exports as the one path; check every `pub` item in `cache.rs` is either re-exported or `pub(crate)`). No behaviour change.
- **Test:** build only.
- **Done when:** `rg "^pub mod cache;" crates/llm-proxy-engine/src/lib.rs` is empty and the workspace builds.
- **Risk / notes:** do together with 05-13 to avoid conflicting edits to `lib.rs`.

### 06-14 [DOC] [P2] mutation.rs module doc overstates "byte-for-byte"
- **Where:** `crates/llm-proxy-adapters/src/mutation.rs` lines 1-22 (phase 2: "`tools`, `system` and `messages` survive byte-for-byte"); `anthropic.rs` `edit_raw_anthropic_request` Edit 3 removes `signature` from `tool_use` blocks in `messages`.
- **Now:** one same-format phase-2 edit does alter message content (foreign Gemini thought-signature field on client-facing `tool_use` blocks).
- **Wanted:** the doc states the real invariant: deterministic per block, so the prefix is stable across turns.
- **How:** reword phase 2 to: "...survive byte-for-byte, with one deterministic exception: a foreign `signature` field on `tool_use` blocks is dropped on every turn alike, so the prefix stays stable across turns." The test for it is in 06-4 (`foreign_signature_stripping_is_deterministic_across_turns`).
- **Test:** covered by 06-4.
- **Done when:** the doc sentence exists.
- **Risk / notes:** none.

### 06-15 [TEST] [P2] End-to-end check on the SQLite ledger that no prompt/response bytes are stored
- **Where:** `cache_and_dedup.rs` `with_every_opt_in_off_nothing_is_stored_and_every_request_runs` (~282) inspects the serialized in-memory `RecordingBus` events, not the SQLite store; `crates/llm-proxy-storage/src/sqlite.rs` `SqliteEventBus::open`.
- **Now:** Done-when 1 is verified on the event bus; the persisted schema is not checked.
- **Wanted:** "with every opt-in off, the proxy stores no prompt or response, and a test checks this."
- **How/Test:** `crates/llm-proxy-server/tests/core_pipeline.rs` (has a full-router harness) or `crates/llm-proxy-storage/tests`: `the_sqlite_ledger_never_contains_prompt_or_response_text`: run a request through an engine wired with a real `SqliteEventBus` on a temp file, with a prompt containing the marker `SECRET-PROMPT-ZQ7` and an upstream reply containing `SECRET-REPLY-ZQ7`; after the run (and a WAL checkpoint via opening a reader), read the db file bytes (and `-wal` if present) and assert neither marker occurs, and that at least one `requests` row exists (so the test is not vacuous).
- **Done when:** passes.
- **Risk / notes:** use the existing temp-dir helper in the storage tests.

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: none of its own; the `cache` double path is 06-13 / 05-13.
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings
- "Data-class policy says `ttl` from storage; should be from completion" — the clock starts at `MemoryCache::store` (`Instant::now()` stamped when the completed response is stored), i.e. at completion. Only the wording is ambiguous (06-11, doc-only).
- "The 'no `[storage]` ledger row' claim was not verified" — verified: `apps/llm-proxy/src/state.rs` `build_event_bus` returns `llm_proxy_storage::NoopBus` when `[storage]` is absent, so rows are discarded as the doc says.
- "`docs/research/*` describes the dedup window as current" — those are dated audit snapshots (`docs/research/audit-2026-11-05/...`); leave alone.
- "8b: direct test absent" as an IMPLEMENTED gap — the indirect catch in `core_pipeline.rs` is real, but a direct test is cheap, so it is kept as 06-9 (P2) rather than rejected.
- "3a-3g, 4a-4g, 6b, 7a-7d, 7f, 8a" — verified IMPLEMENTED; see "Done — leave alone".
