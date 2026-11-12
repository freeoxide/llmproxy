# Plan 01 — Bug fixes: fix spec

Plan file: `docs/completed/01-bug-fixes.md`. All paths are relative to the repository root.
Static verification was done against master @ `fe6e66f`. Line numbers below were re-checked
against the current tree.

## Done — leave alone
- BUG-1 shutdown deadline: `apps/llm-proxy/src/commands/serve.rs` `drain_within_deadline` (deadline starts only after the shutdown branch fires); tests `deadline_does_not_run_before_shutdown`, `drain_that_outlasts_the_deadline_is_cut_off`, `zero_deadline_waits_for_the_whole_drain`.
- BUG-2a `ttl` on cache markers: `protocol/src/client/anthropic.rs` `decode_cache_control`, `decode_system`, `decode_content_block` text arm; `adapters/src/anthropic.rs` `encode_cache_control_marker`.
- BUG-2b top-level and system blocks: `malformed_block` -> `ProtocolError::InvalidRequest` -> HTTP 400 (`routes/core_pipeline.rs:972` `protocol_error_to_route` maps both `InvalidRequest` and `Decode` to 400). Only the nested `tool_result` case is open (01-1).
- BUG-8 same-origin redirects: `transport/src/client.rs` `same_origin_redirect_policy`, three redirect tests.
- BUG-9a `validate_passthrough_auth_combination` and its three unit tests (`core/src/provider_config.rs:3200`, tests near `:3375`).
- BUG-3 Anthropic stream usage/stop reason: `adapters/src/anthropic.rs` `merge_delta_usage`, `map_anthropic_stop_reason`; `fixture_tests.rs` `anthropic_stream_usage`; `core_pipeline.rs` `documented_anthropic_stream_reports_usage_and_max_tokens`.
- BUG-7 chat stream usage: `adapters/src/openai_chat.rs` `stream_options` edit; `core_pipeline.rs` `chat_streams_always_request_usage_upstream`, `openai_client_without_include_usage_gets_no_usage_chunk`. `Accept-Encoding`: reqwest has no compression features (`transport/Cargo.toml`), nothing to do.
- BUG-6 terminal pair held to `finish()`: both client `StreamEncoder`s (`pending_stop`).
- BUG-4 `saw_terminal()` on all four decoders, `decode_stream_error_frame`, `lifecycle.rs` cut-off handling and its three `core_pipeline.rs` tests.
- BUG-5 cache markers on message text and `tool_result`: `CoreContent::ToolResult.cache`, test `message_level_and_tool_result_cache_markers_reach_upstream`.
- BUG-11 `SyntheticZero` for absent usage (chat, Responses, Gemini) with tests.
- BUG-12e Anthropic 5m/1h split: `core_usage_from_fields`, `anthropic_cache_creation_split_maps_to_total`.
- BUG-10 `ledger_model`, BUG-21 `record_client_cancelled`, BUG-26 `dropped_events()` in `storage/src/sqlite.rs`, each with a test.
- BUG-15a Responses decoder shapes, BUG-15b typed `FunctionCall`/`FunctionCallOutput` input items (tests `encode_tool_use_as_function_call`, `encode_tool_result_as_function_call_output`), BUG-16 Gemini camelCase + `alt=sse`, BUG-17 `tool_choice` string, BUG-22 empty `role` skipped.

## Work items

### 01-1 [FIX] [P0] Reject an undecodable block nested in `tool_result` content with HTTP 400
- **Where:** `crates/llm-proxy-protocol/src/client/anthropic.rs:200` `decode_tool_result_blocks` (called from `decode_tool_result_block`, `:238`); inner decode is `decode_wire_block` (`:182`) -> `decode_content_block`, which returns `ProtocolError::Decode` for `tool_use` without `id`/`name` (`:294-306`), `tool_result` without `tool_use_id` (`:258`), and unknown types after typed decode (`:328`).
- **Now:** The loop matches `Err(ProtocolError::Decode(msg))`, logs "skipping undecodable block inside tool_result content array" and continues. If every inner block fails it pushes an empty `CoreContent::Text` ("synthesizing empty text block"). Net effect: `{"type":"tool_result","tool_use_id":"t","content":[{"type":"tool_use","name":"x"}]}` returns HTTP 200 with the content gone. The sibling `InvalidRequest` arm already propagates (that is why `text: 42` is rejected today).
- **Wanted:** Plan "Done": "content the proxy can't parse gets a 400 instead of disappearing."
- **How:**
  1. In `decode_tool_result_blocks`, delete the `Err(ProtocolError::Decode(..))` arm. Convert the error instead: `Err(ProtocolError::Decode(msg)) => return Err(ProtocolError::InvalidRequest(format!("malformed tool_result content: {}", truncate_str_safe(&msg, 120))))`, so the variant matches the top-level path (`malformed_block` produces `InvalidRequest`). Keep `Err(other) => return Err(other)`.
  2. Keep the empty-text synthesis only for an input array that was itself empty: change the guard to `if arr.is_empty()` (an empty `content: []` is valid and must still produce one empty text block). After the change `blocks.is_empty()` with a non-empty `arr` is unreachable.
  3. Remove the now-unused `tracing::warn!` text about skipping.
- **Test:** In `crates/llm-proxy-protocol/src/client/anthropic.rs` tests, extend or add next to `malformed_message_and_tool_result_blocks_are_rejected_not_dropped` (`:1360`) a test `tool_result_with_undecodable_nested_block_is_rejected`: build `make_anthropic_request()` with a user message `[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"no_id"}]}]`. Assert `matches!(decode_request(req), Err(ProtocolError::InvalidRequest(m)) if m.contains("tool_result content"))`. Add a second case `content: []` asserting `Ok` with exactly one empty `CoreContent::Text` (guards the empty-array path). Add a route test in `crates/llm-proxy-server/tests/core_pipeline.rs` (style of `invalid_json_returns_400_anthropic_shape`, `:3202`): POST `/v1/messages` with the first body, assert `StatusCode::BAD_REQUEST` and that the error body mentions `tool_result content`. Both fail on the current code (200 / `Ok`).
- **Done when:** `grep -n "skipping undecodable block" crates/llm-proxy-protocol/src/client/anthropic.rs` returns nothing; the two tests above pass.
- **Risk / notes:** Only blocks of modeled types with a bad shape can fail; unmodeled types still become `Opaque` in `decode_wire_block`, so unknown nested block types keep passing. Same-format passthrough runs this decode first, so it also becomes a 400 there (intended).

### 01-2 [TEST] [P1] Prove `validate` and startup refuse passthrough + client-keys
- **Where:** `apps/llm-proxy/src/state.rs:538` (call inside `load_registry_with_checks`, def `:487`), reached from startup via `load_toml_state_from_config` (`:89`) and from `apps/llm-proxy/src/commands/validate.rs:64`. Existing test modules: `apps/llm-proxy/tests/cli_validate.rs` (uses `cmd_validate(Some(&config))`), `apps/llm-proxy/src/state.rs` `self_loop_startup_tests` (`:1357`, helper `write_config`, calls `load_toml_state(&path, ProviderAdapterRegistry::builtin(), None)`).
- **Now:** The wiring is correct but no test runs a passthrough provider under client-keys through either entry point. Only the pure function is tested.
- **Wanted:** Plan "Done": "refused at config validation ... checked by `validate` and at startup".
- **How:** Tests only.
- **Test:**
  1. `apps/llm-proxy/tests/cli_validate.rs`: `validate_rejects_passthrough_provider_with_client_keys`. Write `config.toml` identical to the one in `validate_accepts_client_keys_on_non_loopback_bind` (`:78`, argon2 verifier line included), plus `providers/pt.toml` next to it: `[provider]\nname = "pt"\npassthrough_auth = true\nauth_style = "bearer"\n[provider.adapters.chat]\nprotocol = "openai_chat_completions"\nendpoint = "http://127.0.0.1:9/v1/chat/completions"\n[provider.routes]\nchat_completions = "chat"\n` (confirm against `validate_api_key`, `provider_config.rs:2235`, that no `api_key` is required for passthrough; add `api_key = "unused"` only if the loader still demands it). Assert `cmd_validate` returns `Err` and `format!("{err:#}")` contains `pt` and `client key`. Add the control `validate_accepts_passthrough_provider_with_anonymous_auth` (same provider, no `[auth]` section, loopback bind) asserting `Ok`, so the refusal is attributable to the combination.
  2. `apps/llm-proxy/src/state.rs`, in `self_loop_startup_tests` (or a new sibling module reusing its `write_config` shape): `startup_refuses_passthrough_provider_with_client_keys` calling `load_toml_state(...)` with the same two files; `expect_err` and assert the message names the provider and contains `client key`.
- **Done when:** the three tests pass; deleting the `validate_passthrough_auth_combination` call at `state.rs:538` makes tests 1 and 2 fail.
- **Risk / notes:** None. `cli_validate.rs` tests are plain `#[test]` with `tempfile`.

### 01-3 [TEST] [P1] Pin chat usage mapping: `cached_tokens`, `reasoning_tokens`, DeepSeek hit/miss, no cache creation
- **Where:** `crates/llm-proxy-adapters/src/lib.rs:491` `build_usage_from_openai`; weak test `crates/llm-proxy-adapters/src/openai_chat.rs:1264` `decode_text_response`; parse-only test `crates/llm-proxy-protocol/src/openai.rs:396`.
- **Now:** `decode_text_response` feeds prompt 100 / hit 10 / miss 20, which violates DeepSeek's `prompt_tokens = hit + miss`, and asserts only input 90 and `cache_read == 10`. `cache_creation_input_tokens` is never asserted, so a regression `cache_creation = miss` would pass. Nothing exercises `prompt_tokens_details.cached_tokens` or `completion_tokens_details.reasoning_tokens` through `build_usage_from_openai`. `adapters/src/lib.rs` has no `#[cfg(test)]` module.
- **Wanted:** Plan "Usage mapping": "DeepSeek miss tokens count as plain input, never cache creation; `prompt_tokens_details.cached_tokens` ... `completion_tokens_details.reasoning_tokens` ... map into the core fields".
- **How:** Add tests and fix the test data. Do not change `build_usage_from_openai`.
- **Test:** In `adapters/src/lib.rs` add `#[cfg(test)] mod usage_tests` (the function is `pub(crate)`) with:
  - `deepseek_hit_and_miss_map_to_cache_read_and_plain_input`: `UsageInfo { prompt_tokens: 30, completion_tokens: 7, total_tokens: 37, prompt_cache_hit_tokens: Some(10), prompt_cache_miss_tokens: Some(20), prompt_tokens_details: None, completion_tokens_details: None }` -> assert `input_tokens == 20`, `cache_read_input_tokens == Some(10)`, `cache_creation_input_tokens == None`, `provenance == ProviderReported`.
  - `openai_cached_tokens_detail_maps_to_cache_read`: prompt 50, no `prompt_cache_hit_tokens`, `prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: Some(12) })` -> `input_tokens == 38`, `cache_read_input_tokens == Some(12)`, `cache_creation_input_tokens == None`.
  - `completion_reasoning_tokens_map_to_core`: `completion_tokens_details: Some(CompletionTokensDetails { reasoning_tokens: Some(9) })` -> `reasoning_tokens == Some(9)`; and with the detail absent -> `None`.
  - `cached_tokens_larger_than_prompt_does_not_underflow`: prompt 5, cached 9 -> `input_tokens == 0`.
  Also fix `decode_text_response` data to `prompt_tokens: 30`, `prompt_cache_hit_tokens: Some(10)`, `prompt_cache_miss_tokens: Some(20)`, and assert `input_tokens == 20` plus `cache_creation_input_tokens == None`.
- **Done when:** tests above pass; mutating `cache_creation_input_tokens: None` to `usage.prompt_cache_miss_tokens` in `build_usage_from_openai` makes `deepseek_hit_and_miss...` and `decode_text_response` fail.
- **Risk / notes:** Check how the struct types are named in `llm_proxy_protocol::openai` (`PromptTokensDetails`, `CompletionTokensDetails`) before writing literals.

### 01-4 [TEST] [P1] Pin Responses usage mapping for reasoning tokens
- **Where:** `crates/llm-proxy-adapters/src/responses.rs:747` `build_responses_usage`; existing test `documented_completed_event_carries_usage_inside_response` (`:842`) asserts input 7 / cache_read 3 only.
- **Now:** `output_tokens_details.reasoning_tokens` is mapped (`reasoning_tokens:` at `:757`) but no test asserts a non-null mapped value; fixtures carry `reasoning_tokens: null`.
- **Wanted:** Plan "`completion_tokens_details.reasoning_tokens` ... map into the core fields" (Responses side).
- **How:** Tests only.
- **Test:** In `responses.rs` tests add `usage_details_map_cache_and_reasoning`: build `ResponsesUsage { input_tokens: 20, output_tokens: 11, input_tokens_details: Some(.. cached_tokens: Some(8) ..), output_tokens_details: Some(.. reasoning_tokens: Some(6) ..), .. }` (use the real struct names from `llm-proxy-protocol/src/zen.rs`), call `build_responses_usage`, assert `input_tokens == 12`, `cache_read_input_tokens == Some(8)`, `reasoning_tokens == Some(6)`, `cache_creation_input_tokens == None`. Second case with both details `None` -> `reasoning_tokens == None`, `cache_read == None`, `input_tokens == 20`.
- **Done when:** test passes; setting `reasoning_tokens: None` in `build_responses_usage` makes it fail.
- **Risk / notes:** None.

### 01-5 [FIX] [P1] OpenAI-client usage must report prompt tokens including cache, and must not call cache creation "miss"
- **Where:** `crates/llm-proxy-protocol/src/client/openai_chat.rs:572` `encode_usage` (callers `:531` non-stream, `:652` and `:792` stream); its test `usage_mapping` (`:1521`) pins the current numbers.
- **Now:** `prompt_tokens = usage.input_tokens`, `prompt_cache_hit_tokens = cache_read`, `prompt_cache_miss_tokens = cache_creation_input_tokens`, `prompt_tokens_details = None`, `completion_tokens_details = None`; reasoning tokens are dropped with a warning. Core `input_tokens` is net of cache everywhere (OpenAI/DeepSeek adapter subtracts cache reads, `lib.rs:500`; Anthropic reports `input_tokens` excluding cache). So an OpenAI SDK client served by an Anthropic or DeepSeek upstream sees `prompt_tokens` smaller than the real prompt, hit+miss that do not sum to it, and cache creation reported as "miss". This is the client-side mirror of BUG-12 ("miss tokens count as plain input, never cache creation").
- **Wanted:** Plan "Usage mapping": miss tokens are plain input, never cache creation. Coherent client numbers follow from the same rule.
- **How (decision already made, do not ask):** Compute with `saturating_add`/`i32`:
  - `prompt_tokens = input + cache_read.unwrap_or(0) + cache_creation.unwrap_or(0)`.
  - `prompt_cache_hit_tokens = cache_read` (unchanged).
  - `prompt_cache_miss_tokens = Some(input + cache_creation.unwrap_or(0))` when `cache_read.is_some() || cache_creation.is_some()`, else `None` (keeps the DeepSeek invariant `prompt = hit + miss`; creation tokens are uncached prompt tokens from the client's view).
  - `prompt_tokens_details = Some(PromptTokensDetails { cached_tokens: cache_read })` when `cache_read.is_some()` (this is the field OpenAI SDKs read).
  - `completion_tokens_details = Some(CompletionTokensDetails { reasoning_tokens })` when `reasoning_tokens` is `Some`; remove the drop-warning block.
  - `total_tokens = prompt_tokens + completion_tokens` (saturating).
  Check that `ChatCompletionChunk`/response serialization of `UsageInfo` (`protocol/src/openai.rs:238`) skips `None` details so clients with no cache see byte-identical output to today.
- **Test:** Replace `usage_mapping` (`client/openai_chat.rs:1521`) and add cases in the same module: (a) `input 100, cache_read 5, cache_creation 10, output 200` -> `prompt_tokens == 115`, `hit == Some(5)`, `miss == Some(110)`, `prompt_tokens_details.cached_tokens == Some(5)`, `total == 315`; (b) property check over several tuples that `hit + miss == prompt_tokens` whenever `hit` is `Some`; (c) no cache fields at all -> `prompt_tokens == input`, hit/miss/details all `None`; (d) `reasoning_tokens: Some(9)` -> `completion_tokens_details.reasoning_tokens == Some(9)`. Add one end-to-end assertion in `crates/llm-proxy-server/tests/core_pipeline.rs` (Anthropic upstream with `cache_read_input_tokens` + `cache_creation_input_tokens`, OpenAI client) that the JSON `usage.prompt_tokens` equals `input + read + creation`. Existing streaming tests that assert exact usage JSON need updating only if their fixtures carry cache fields.
- **Done when:** the tests above pass; `grep -n "does not map reasoning_tokens" crates/llm-proxy-protocol/src/client/openai_chat.rs` returns nothing.
- **Risk / notes:** Behaviour change for OpenAI clients on cached prompts (`prompt_tokens` goes up to the true value). That is the OpenAI contract, so it is a correction, not a regression. The ledger is unaffected (it reads core usage).

### 01-6 [TEST] [P1] Assert the Responses encoder carries `temperature`, `top_p`, `max_output_tokens`
- **Where:** `crates/llm-proxy-adapters/src/responses.rs:607-609` (`ResponsesRequest { temperature, top_p, max_output_tokens, .. }` in `encode_request`).
- **Now:** No test asserts these in an encoded body; `encode_tool_choice_forwarded` checks only `tool_choice`; the Responses encode fixtures set no sampling.
- **Wanted:** Plan BUG-15: "encoder emits ... temperature/top_p/max_output_tokens".
- **How:** Tests only (the fixture runner change in 01-7 also covers this).
- **Test:** In `responses.rs` tests add `encode_sampling_fields_are_carried`: `core.sampling.temperature = Some(0.3)`, `top_p = Some(0.8)`, `max_tokens = Some(256)`; `adapter.encode_request(&core, &make_target(), &client_wire(&core))`; parse the body as `serde_json::Value`; assert `body["temperature"] == 0.3`, `body["top_p"] == 0.8`, `body["max_output_tokens"] == 256`, and `body.get("max_tokens").is_none()` (the old, wrong name). Second case with all three `None` asserts the keys are absent.
- **Done when:** test passes; renaming `max_output_tokens` in the encoder breaks it.
- **Risk / notes:** None.

### 01-7 [TEST] [P1] Make the encode-fixture runner compare `input`; refresh the stale Responses fixtures
- **Where:** `crates/llm-proxy-adapters/tests/fixture_tests.rs:517` `run_encode_request_fixture` (compares `messages`, `system`, `tools`, `temperature`/`top_p`/`max_tokens` only); fixtures `crates/llm-proxy-adapters/tests/fixtures/responses/{text_request,tool_result_request,system_prompt_request,thinking_request,tool_request}/output.json`.
- **Now:** `tool_result_request/output.json` expects the pre-BUG-15 wrapped shape `{"role":"assistant","content":{"type":"function_call",...}}` and `{"role":"user","content":{"type":"function_call_output",...}}`; `text_request/output.json` expects `"input": "Hello"` (string). The encoder emits typed items (`ResponsesInput::FunctionCall`/`FunctionCallOutput`, `{"type":"function_call","call_id","name","arguments"}`) and a message list. The drift is invisible because `input` and `reasoning`/`max_output_tokens` are never compared.
- **Wanted:** "Each fix has a regression test"; fixtures must describe the shipped wire shape (BUG-15b).
- **How:**
  1. In `run_encode_request_fixture`, after the `messages` block, add: if `output_json` has `"input"`, `assert_eq!(actual.get("input"), Some(expected_input), "{adapter_name}/{case}: input mismatch")` (exact equality). Do the same for `"reasoning"` and extend the sampling loop list `["temperature","top_p","max_tokens"]` with `"max_output_tokens"`.
  2. Run the Responses encode cases once, read the actual encoded body from the failure output, and rewrite each `output.json` to the real shape: typed `function_call` item `{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"SF\"}"}`; typed `{"type":"function_call_output","call_id":"call_1","output":"Sunny, 72F"}`; plain messages as the encoder emits them (check whether `input` is a message array for `text_request`, and fix the `"Hello"` string expectation accordingly). Review each rewritten file by hand against the Responses API docs, do not paste blindly.
  3. Add sampling (`temperature`, `top_p`, `max_tokens` in `core.json` -> `max_output_tokens` in `output.json`) to one Responses request fixture, e.g. `text_request`, so the fixture path also covers 01-6.
- **Test:** The runner change is the test. Verify it fails on the current stale fixtures (revert step 2 locally to confirm) and passes after.
- **Done when:** `responses_encode_tool_result_request` and `responses_encode_text_request` (`fixture_tests.rs:1294`, `:1314`) pass with `input` compared; `grep -n '"content": { "type": "function_call"' crates/llm-proxy-adapters/tests/fixtures/responses` returns nothing.
- **Risk / notes:** Other adapters' `output.json` files have no `input` key, so the new branch is inert for them. If an Anthropic/Chat fixture happens to contain `input`, it will start being compared; check with `grep -rln '"input"' crates/llm-proxy-adapters/tests/fixtures/*/*/output.json`.

### 01-8 [TEST] [P2] Assert OpenAI client `cache_control.ttl` reaches the core
- **Where:** `crates/llm-proxy-protocol/src/client/openai_chat.rs:86` and `:95` (system and user decode map `ttl: cc.ttl`); test `cache_control_decodes_to_core` (`:1268`) sets `ttl: None`.
- **Now:** The ttl mapping is unasserted; the plan says "the OpenAI `cache_control` shape carries `ttl` too".
- **Wanted:** BUG-5c.
- **How:** Tests only.
- **Test:** Change `cache_control_decodes_to_core` to use `ttl: Some("1h".into())` and assert `cache.as_ref().unwrap().ttl.as_deref() == Some("1h")`. Add the same for a `system` message (`messages[0].role = "system"` with `cache_control`) asserting `core.system[0]` carries the ttl.
- **Done when:** test passes; dropping `ttl: cc.ttl` at `:86` or `:95` fails it.
- **Risk / notes:** None.

### 01-9 [DOC] [P2] Correct stale/inexact claims in the plan text
- **Where:** `docs/completed/01-bug-fixes.md`.
- **Now:** (a) "Usage mapping" says `prompt_tokens_details.cached_tokens` for "chat and Responses"; the Responses API (and code, `responses.rs:747`) uses `input_tokens_details.cached_tokens`. (b) It says the 5m/1h split "map into the core fields"; core has one `cache_creation_input_tokens`, so the two buckets are summed (`core_usage_from_fields`). (c) "the OpenAI `cache_control` shape carries `ttl` too" is true for the wire type and client decode only; the OpenAI Chat adapter does not emit cache markers upstream (`adapters/src/openai_chat.rs:537-540`). (d) "Done when" says `run.sh` "shows correct results"; `run.sh` and the two probe scripts only print, they assert nothing (the only automated check is the deadline grep at `run.sh:69`).
- **Wanted:** The doc must describe the code.
- **How:** Edit the four sentences: (a) "`prompt_tokens_details.cached_tokens` (chat), `input_tokens_details.cached_tokens` (Responses), `completion_tokens_details.reasoning_tokens` / `output_tokens_details.reasoning_tokens`"; (b) "...Anthropic's 5m/1h `cache_creation` split (summed into one `cache_creation_input_tokens`)"; (c) "...the OpenAI client `cache_control` shape carries `ttl` into the core (Anthropic upstreams only; the chat adapter does not forward markers)"; (d) replace the first Done-when bullet with "Each of rows 4 to 7, 10, 11 and 13 is covered by a named CI test (see the Done list); `run.sh` remains a manual probe for a human to read." Keep it short; no design narrative.
- **Done when:** `grep -n "input_tokens_details" docs/completed/01-bug-fixes.md` matches and `grep -n "shows correct" docs/completed/01-bug-fixes.md` returns nothing.
- **Risk / notes:** If 01-5 lands, add one clause under "Usage mapping": "OpenAI clients receive `prompt_tokens` including cached tokens."

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: RV-1, RV-2, RV-3, RV-4, RV-6, RV-9.
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings
- "BUG-1 test is timing-sensitive; no signal-to-exit test" — the unit tests are deterministic in both directions: `deadline_does_not_run_before_shutdown` uses a never-firing shutdown future with a 1 ms deadline against 50 ms of work (a regressed implementation returns `None` and fails), and the cut-off test uses `pending()` work. `start_paused` would need tokio's `test-util` feature, which the workspace does not enable (`Cargo.toml:68`), so adding it only for this is not worth a new feature flag.
- "Row 24: 5m/1h breakdown lost in core" — a design limit of the single `cache_creation_input_tokens` field, not a defect; handled as a wording fix in 01-9(b). Splitting the field would be a new feature outside this plan.
- "Row 18 scope: OpenAI adapter drops cache markers" — matches the plan's Anthropic-only intent (`cache_control` has no OpenAI upstream meaning); only the wording changes (01-9(c)).
- "Row 36 fail-before cannot be shown" — not a gap; each new test above names the mutation that must break it.
- "Row 35 `run.sh` not pass/fail" — not converted into an assertion script; the CI tests are the pass/fail evidence and the doc is corrected instead (01-9(d)).
- "Row 14: a leading chat error frame still returns 502 with the message hidden" — intentional (documented in the report: nothing has been sent to the client yet), covered by `leading_chat_error_frame_is_marked_failed_in_ledger`.
- "Row 11 Accept-Encoding has no test" — negative claim about absent code; nothing to assert.
