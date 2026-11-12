# Plan 08 — Client dialects: fix spec

Source: verification report `reports/plan-08.md`, plan `docs/completed/08-client-dialects.md`.
All paths are relative to the repository root. Line numbers were re-checked statically; trust symbol names.
Test file shorthand: `CD` = `crates/llm-proxy-server/tests/client_dialects.rs`.

## Done — leave alone
- Routes and admission: `crates/llm-proxy-server/src/routes/mod.rs` (`/providers/{provider}/v1/responses`, `/v1beta/models/{model_method}`, `/api/chat|generate|tags`; `FAMILY_RESPONSES|GENERATE_CONTENT|OLLAMA`, tags under `FAMILY_MODELS`), handlers `routes/responses.rs`, `routes/gemini_generate.rs`, `routes/ollama.rs`, all through `core_pipeline::handle_core_once|handle_core_stream`.
- Responses codec: `crates/llm-proxy-protocol/src/client/responses.rs` (`decode_request`, `encode_usage` with always-present `cached_tokens`/`reasoning_tokens`, `StreamEncoder` event set and Codex tool-call ordering); test `CD::responses_tool_call_ordering_cross_formats`.
- Gemini codec: `.../client/gemini.rs` (`decode_request`, `encode_response`, stream encoder, name correlation, `thoughtSignature`); adapter `crates/llm-proxy-adapters/src/gemini.rs` (`gemini_stream_url`, same-format branch).
- Ollama codec: `.../client/ollama.rs` (`MAPPED_OPTION_KEYS`, `check_unmapped_options`, `decode_think`, NDJSON encoder, `encode_tags_response`).
- Route kinds and wire formats: `ProviderRouteKind::{Responses,GenerateContent}`, `ClientWireFormat` variants, `field_disposition`, `store`-rejected-by-name (`CD::responses_store_is_rejected_by_name_cross_format`), per-dialect error bodies (`error_response.rs`), stream framing (`core_pipeline.rs` `StreamWire`).
- Usage write path: `crates/llm-proxy-engine/src/lifecycle.rs` `client_dialect_str` and `emit_request_received` (asserting it is 08-2).
- Codex replay: `crates/llm-proxy-server/tests/contract_replay.rs:1413,1489` and fixtures under `crates/llm-proxy-server/tests/fixtures/contract/captured/codex/`.
- SDK without server crate: `crates/llm-proxy-sdk/tests/dialects_without_server.rs`, CI `--no-default-features` job.
- Codex provenance (row 17, real CLI 0.167.0 capture): NOT-STATICALLY-VERIFIABLE; no action.

## Work items

### 08-1 [DECIDE] [P1] Responses tool-level `strict`: reject `strict: true` by name cross-format, carry it same-format
- **Where:** `crates/llm-proxy-protocol/src/client/responses.rs:456` `decode_tools` (reads `name`/`description`/`parameters` only); `crates/llm-proxy-protocol/src/core.rs:368` `CoreTool` (no strict field, `deny_unknown_fields`); policy site `crates/llm-proxy-adapters/src/client_wire.rs:281` `apply_field_policy` and `:331` `check_cross_format_tools`; plan Done text "function tools with strict schemas".
- **Now:** a Responses function tool `{"type":"function","name":"f","strict":true,"parameters":{...}}` crossing to Anthropic/Chat/Gemini silently loses `strict`; the structured-output guarantee the client asked for disappears with no error. Same-format passthrough (raw body) keeps it (`crates/llm-proxy-server/tests/contract_replay.rs:1347` asserts `tools[0].strict == true`). The only unit-level `strict` (`responses.rs:1262`) sits inside `parameters`, so the tool-level flag is never exercised. (The same silent loss exists for Chat `function.strict` and Anthropic tool `strict`; out of scope here, note for plan 02.)
- **Options:** (a) add `strict: Option<bool>` to `CoreTool` and carry it through every encoder that supports it (Chat `function.strict`, Anthropic, Responses), rejecting for Gemini; touches 21 `CoreTool {` literals (`rg -c 'CoreTool \{' crates apps`) and four codecs; (b) reject `strict: true` by name when crossing formats, accept `strict: false`/absent.
- **Recommended default: (b).** It honours the cross-format policy ("rejected by name, never silently dropped") with a small change; real Codex traffic sends `"strict": false` on every tool (25 occurrences under `fixtures/contract`, only 1 `true`), so rejecting `false` would break the primary client, while `false` is the default meaning and loses nothing.
- **How:**
  1. In `client_wire.rs` add `fn check_strict_function_tools(client:&ClientWireRequest, provider:&ProtocolName, lossy:bool)->Result<(),ProviderError>`: only for `ClientWireFormat::OpenAiResponses`; iterate `body.tools[]` objects whose `type` is absent or `"function"`; collect `name` of those with `strict == Value::Bool(true)`. Empty -> `Ok`. Under `lossy`, `tracing::warn!` and `Ok`. Else `ProviderError::InvalidRequest(format!("function tool(s) [{names}] set strict: true, which has no {provider} equivalent; remove strict or enable the route's lossy mode"))`.
  2. Call it in `apply_field_policy` right after `check_cross_format_tools(client, provider, lossy)?;` (line ~303). Same-format Responses never reaches `apply_field_policy` (adapters/responses.rs:~473 returns early), so passthrough is unaffected.
  3. Update the `decode_tools` doc comment to say `strict` is intentionally not part of the core tool and is policed by `check_strict_function_tools`; fix the plan Done text to "function tools (a tool-level `strict: true` is carried same-format and rejected by name cross-format)".
  4. Fix the misleading unit test input at `responses.rs:1262`: move `"strict": true` out of `parameters` to the tool object (keep decode assertions, which must still pass because decode ignores it).
- **Test:** `client_wire.rs::tests`: `strict_function_tools_are_rejected_by_name_cross_format` (Responses body with `{"type":"function","name":"get_weather","strict":true,"parameters":{}}` vs `ProtocolName::new(protocols::ANTHROPIC_MESSAGES)` -> `Err` whose text contains `get_weather` and `strict`; repeat for `OPENAI_CHAT_COMPLETIONS` and `GEMINI_GENERATE_CONTENT`); `strict_false_and_absent_pass_cross_format` (`false` and missing -> `Ok`); `strict_tools_pass_on_lossy_routes`. Server-level `CD::responses_strict_tool_is_rejected_by_name_cross_format`: provider `anthropic_messages` + `ProviderRouteKind::Responses`, request from `responses_request(false)` with `tools[0]["strict"] = true` -> 400, body text contains `strict` and `get_weather`; and `CD::responses_strict_tool_survives_same_format`: `openai_responses` provider, captured upstream body has `tools[0].strict == true`. The reject tests fail before the change (request succeeds, `strict` lost).
- **Done when:** the four tests pass; `rg -n 'check_strict_function_tools' crates/llm-proxy-adapters/src/client_wire.rs` shows definition and call.
- **Risk / notes:** if the owner picks (a) later, delete the check and carry the field; the tests above flip to asserting the encoded upstream body.

### 08-2 [TEST] [P1] Assert a usage-ledger row, with the dialect, for Responses, Gemini and Ollama requests
- **Where:** `CD` `state_with_provider` (builds `AppConfig{storage: None,..}` with no event bus); engine writer `crates/llm-proxy-engine/src/lifecycle.rs` `client_dialect_str` (`openai_responses`, `gemini`, `ollama`), `route_kind_str`; reader `crates/llm-proxy-storage/src/usage_query.rs` `UsageReader::requests`, `RequestFilter`, `RequestRow{client_protocol,route_kind,provider,streaming,tokens,state}`; bus helpers as in `crates/llm-proxy-server/tests/secret_abuse.rs:267` `open_ledger`/`close_ledger`.
- **Now:** the write path is real but nothing asserts a row with a new dialect (`rg 'openai_responses|"gemini"|"ollama"' crates/*/tests` finds none). Plan Done: "accounting ... gets limits, routing and accounting without new pipeline code."
- **Wanted:** each new dialect produces a completed ledger row carrying its dialect, route kind and the usage numbers.
- **How/Test:** in `CD` add helpers `open_ledger(path) -> Arc<SqliteEventBus>` (`llm_proxy_storage::SqliteEventBus::open(path, RetentionPolicy{max_age:None,max_rows:None,prune_every:3600s})`), `close_ledger(bus)` (`tokio::task::spawn_blocking(move || bus.close()).await`), and `async fn ledger_rows(db:&Path)->Vec<RequestRow>` (`UsageReader::new(db).requests(RequestFilter{limit:50,..Default::default()}).await.expect("rows").rows`). Build each state with `state_with_provider(...).with_event_bus(bus.clone() as Arc<dyn EventBus>)`. New tests (all use the Anthropic fake upstream, which reports input 21, output 6, cache_read 8, cache_creation 4):
  - `responses_request_is_recorded_with_its_dialect`: non-stream `POST /providers/dialect-provider/v1/responses`; after `close_ledger`, exactly one row with `client_protocol == Some("openai_responses")`, `route_kind == Some("responses")`, `provider == Some("dialect-provider")`, `streaming == Some(false)`, `state == "completed"`, `tokens` = `{input:21, output:6, cache_read:8, cache_creation:4}`.
  - `gemini_request_is_recorded_with_its_dialect`: `:generateContent` -> `"gemini"` / `"generate_content"`, same tokens.
  - `ollama_request_is_recorded_with_its_dialect`: `/api/chat` (route kind `ChatCompletions`, `chat_route=true`) -> `"ollama"` / `"chat_completions"`.
  - `streamed_dialect_request_is_recorded_after_the_stream_ends`: Responses stream (`AnthropicStream`): read the whole body via `body_text` BEFORE `close_ledger`; row has `streaming == Some(true)`, `state == "completed"`, `tokens.input == 21`.
  Each fails before only if the dialect string/route kind is wrong; today they are missing coverage, so write them to pass and verify they would fail if `client_dialect_str` returned `"openai_chat"` (document this in the test comment).
- **Done when:** the four tests pass; `rg -n 'openai_responses' crates/llm-proxy-server/tests/client_dialects.rs` shows a ledger assertion.
- **Risk / notes:** `crates/llm-proxy-server/Cargo.toml` dev-deps already include `llm-proxy-storage` with `test-utils` and `sqlite` features and `tempfile`. Use one temp dir per test.

### 08-3 [TEST] [P1] Streaming error tests for all three dialects
- **Where:** `CD` `Behavior` enum and `respond` (only `Error400`, used non-streaming at `CD::dialect_errors_map_to_their_wire_shapes`); server pipeline `core_pipeline.rs` `encode_head`, `encode_tail`, `encode_terminal_error`, `dialect_error_chunk`; codec branches `gemini.rs` (`CoreEvent::Error` ~line 592) and `ollama.rs` (~line 705); Responses stream error unit test only at `responses.rs:1630`.
- **Now:** plan Done-when: "Each dialect passes the shared streaming, tool-call, usage and error tests." Streaming x error is untested for Gemini and Ollama (and for Responses at the route level).
- **How/Test:** two groups in `CD`.
  1. Pre-stream HTTP error: reuse `Behavior::Error400` with stream requests. `streaming_http_errors_use_the_dialect_error_shape`: Responses `stream:true` -> status 400, JSON `error.message` and `error.type` strings, content-type is JSON (NOT `text/event-stream`); Gemini `...:streamGenerateContent?alt=sse` -> 400, `error.code == 400`, `error.status == "INVALID_ARGUMENT"`; Ollama `ollama_chat_request(true)` -> 400, `error` is a string and content-type is not `application/x-ndjson` stream of `done` objects.
  2. Mid-stream error after bytes were committed: add `Behavior::AnthropicStreamThenError` whose SSE is `message_start`, a text `content_block_start`, one `text_delta` ("it is "), then `event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}\n\n`. Tests (status 200 for all three, error is in-band):
     - `responses_stream_error_renders_response_failed`: body contains `event: response.failed`, does NOT contain `response.completed`; the `response.failed` payload's error message contains `Overloaded`.
     - `gemini_stream_error_renders_an_error_chunk`: SSE lines parsed as JSON; one has `error.message` containing `Overloaded`; no chunk has a `finishReason`.
     - `ollama_stream_error_renders_an_error_line`: NDJSON lines parsed; the last has `error` (string) containing `Overloaded`; no line has `"done": true`.
     Also one for `gemini_stream_without_alt_error_closes_the_json_array`: the body parses as a JSON array whose last element has `error` (array framing must still be valid JSON after an error).
  If any encoder emits slightly different text, assert on the observable properties above (error surfaced in dialect framing, no success terminal), not on exact strings.
- **Done when:** the six tests pass.
- **Risk / notes:** `dialect_error_chunk` hard-codes Gemini code `500` for every in-band error; do not assert the code. The fake upstream serves all routes from one handler already (`/v1/messages`).

### 08-4 [TEST] [P1] Server-tool rejection for Responses `web_search` and Gemini `googleSearch`
- **Where:** `crates/llm-proxy-adapters/src/client_wire.rs:331` `check_cross_format_tools` (Responses and Gemini arms) and its tests (`server_tools_are_rejected_by_name_cross_format`, ~line 744, Anthropic only); server-level `CD`.
- **Now:** the code exists; `rg googleSearch crates` and `rg '"type": "web_search"' crates` find no Responses/Gemini rejection test. Plan Done: "the Responses and Gemini server-tool checks reject `web_search`/`googleSearch`-style tools by name cross-format."
- **How/Test:**
  - `client_wire.rs::tests`: `responses_server_tools_are_rejected_by_name_cross_format` (Responses body `tools:[{"type":"web_search"},{"type":"function","name":"keep","parameters":{}}]` vs `ANTHROPIC_MESSAGES`, `OPENAI_CHAT_COMPLETIONS`, `GEMINI_GENERATE_CONTENT` -> `Err` containing `web_search`); `gemini_server_tools_are_rejected_by_name_cross_format` (`tools:[{"googleSearch":{}}]` -> `Err` containing `googleSearch`); `gemini_function_declarations_are_not_server_tools` (`[{"functionDeclarations":[...]}]` -> `Ok`) and `responses_function_tools_are_not_server_tools`; lossy variants return `Ok`.
  - `CD::responses_web_search_is_rejected_by_name_cross_format`: Anthropic provider, `responses_request(false)` plus a second tool `{"type":"web_search"}` -> 400, text contains `web_search`. `CD::gemini_google_search_is_rejected_by_name_cross_format`: `gemini_request()` with `tools:[{"googleSearch":{}}]` -> 400, Gemini shape (`error.code == 400`), message contains `googleSearch`. `CD::responses_web_search_survives_same_format`: `openai_responses` provider -> captured upstream `tools` still contains `{"type":"web_search"}`.
- **Done when:** the tests pass.
- **Risk / notes:** none.

### 08-5 [FIX] [P1] Gemini `promptTokenCount` must include cache reads (and expose `cachedContentTokenCount`)
- **Where:** `crates/llm-proxy-protocol/src/client/gemini.rs:397` `encode_usage` (used by `encode_response` ~line 457 and the stream `terminal_chunks` ~line 614); tests `CD::gemini_usage_fields_are_wire_exact` (asserts 21/6/27) and the codec tests at `gemini.rs:~813,~851`; comparison `responses.rs:547` `encode_usage` (adds the cache read back).
- **Now:** `promptTokenCount = usage.input_tokens` where the core `input_tokens` EXCLUDES cache reads (Anthropic wire semantics; documented in the plan's Responses paragraph). With 21 core input + 8 cache read, Responses reports 29 but Gemini reports 21. Google documents `promptTokenCount` as the total prompt size including cached content, with `cachedContentTokenCount` as the cached part. Gemini clients therefore under-report and cannot see caching.
- **Decision (recommended):** align Gemini with its documented wire contract and with the Responses choice. `promptTokenCount = input_tokens + cache_read_input_tokens.unwrap_or(0)`; when `cache_read_input_tokens` is `Some(n>0)` add `cachedContentTokenCount = n`; `totalTokenCount = promptTokenCount + candidatesTokenCount` (saturating, as today). Do not add cache-creation tokens (the Responses encoder does not either; keep the three dialects consistent and note it).
- **How:** rewrite `encode_usage` accordingly with `i64` saturating adds and `i32::try_from(..).unwrap_or(i32::MAX)`. Add a doc comment citing the semantics. Same-format Gemini passthrough returns the upstream's raw numbers and is untouched.
- **Test:** update `CD::gemini_usage_fields_are_wire_exact` to assert `promptTokenCount == 29`, `cachedContentTokenCount == 8`, `candidatesTokenCount == 6`, `totalTokenCount == 35` (fails before). Add a streaming counterpart `CD::gemini_stream_usage_includes_cache_reads` (Anthropic stream upstream, `cache_read_input_tokens: 8` in message_start) asserting the terminal chunk's `usageMetadata.promptTokenCount == 29`. Add a codec unit test in `gemini.rs`: usage with no cache read has NO `cachedContentTokenCount` key. Keep `gemini.rs:~813` (no cache) at 10.
- **Done when:** tests pass; `rg -n 'cachedContentTokenCount' crates/llm-proxy-protocol/src/client/gemini.rs` matches.
- **Risk / notes:** (also: plan 02) the Chat dialect encoder `client/openai_chat.rs:572-587` has the same exclude-cache shape (`prompt_tokens = input_tokens`, `prompt_tokens_details: None`); and the Gemini UPSTREAM decode (`crates/llm-proxy-adapters/src/gemini.rs:~720`) never reads `cachedContentTokenCount`, so cached usage from a Gemini upstream is not recorded. Record both as follow-ups; do not fix here.

### 08-6 [FIX] [P1] Gemini streaming terminal chunk drops `thoughtsTokenCount`
- **Where:** `crates/llm-proxy-protocol/src/client/gemini.rs` `encode_response` (inserts `thoughtsTokenCount` after `encode_usage`) vs stream `terminal_chunks` (calls `encode_usage(&usage)` only).
- **Now:** non-streaming responses carry `thoughtsTokenCount` when `reasoning_tokens` is set; the streamed terminal chunk never does. Plan: "encodes ... usageMetadata (with `thoughtsTokenCount`)."
- **How:** move the `reasoning_tokens` insertion into `encode_usage` (single source) and delete the special case in `encode_response`.
- **Test:** `gemini.rs::tests`: `stream_terminal_chunk_carries_thoughts_token_count` — feed `CoreEvent::UsageDelta{usage}` with `reasoning_tokens: Some(3)` then `MessageStop`; terminal `usageMetadata.thoughtsTokenCount == 3`. Fails before (key absent).
- **Done when:** the test passes; `rg -n thoughtsTokenCount crates/llm-proxy-protocol/src/client/gemini.rs` shows one production insertion.
- **Risk / notes:** none.

### 08-7 [DECIDE] [P2] Ollama `prompt_eval_count`: keep excluding cache reads, document it
- **Where:** `crates/llm-proxy-protocol/src/client/ollama.rs:481` `usage_counts`; `CD::ollama_non_streaming_usage_fields` (asserts 21/6).
- **Now:** `prompt_eval_count = input_tokens` (excludes the 8 cache-read tokens). Unlike Gemini/Responses, Ollama's wire has no cached-token field and its `prompt_eval_count` counts tokens actually evaluated (KV-cache hits reduce it), so excluding cache reads matches Ollama's documented meaning.
- **Recommended default:** keep behaviour. Add a doc comment on `usage_counts`: "Ollama counts evaluated prompt tokens; cache-read tokens are not evaluated and are deliberately excluded (contrast Responses/Gemini, whose wire totals include them)." Pin the rationale in the test comment of `CD::ollama_non_streaming_usage_fields` with a cache read present (already 8 in the fixture) so a future change must be deliberate.
- **Test:** no new behaviour; existing assertions stay.
- **Done when:** the doc comment exists.
- **Risk / notes:** if the owner prefers parity, change to `input + cache_read` and update the two assertions (`CD` ~line 984 and the stream terminal at ~602).

### 08-8 [TEST] [P1] Make the `/api/tags` route test non-vacuous and cover pools
- **Where:** `CD::ollama_tags_lists_models_from_the_catalog` (~line 1277); `state_with_provider` (no `catalog`); route `crates/llm-proxy-server/src/routes/ollama.rs:141-205` `handle_ollama_tags` / `handle_tags_inner`; `ProviderConfig::catalog: Option<ProviderCatalogConfig{ mode, models: Vec<StaticModelCatalogEntry> }>`.
- **Now:** `catalog: None` yields an empty model list, so the `for model in &models` loop asserts nothing; the test passes for any output shape, and pool union (documented) is untested.
- **How/Test:** refactor `state_with_provider` into `provider_config(name, upstream, protocol, route, chat_route, catalog: Option<ProviderCatalogConfig>) -> ProviderConfig` plus `state_from(providers: Vec<ProviderConfig>, pools: Vec<PoolConfig>) -> AppState` (use `ProviderRegistry::from_parts`); keep `state_with_provider` as a thin wrapper. Then:
  - Rewrite `ollama_tags_lists_models_from_the_catalog`: catalog `mode: Static`, models `alpha-model`, `beta-model` -> `value["models"]` has exactly two entries, `[{name: "alpha-model", model: "alpha-model"}, ...]` (assert the full array equality, sorted as the route returns them).
  - `ollama_tags_for_a_pool_lists_the_deduplicated_union`: two providers `dialect-a` (models `alpha`, `shared`) and `dialect-b` (`shared`, `beta`), pool `duo` (`toml::from_str::<PoolFile>("[pool]\nname=\"duo\"\nmembers=[\"dialect-a\",\"dialect-b\"]\n")`, `toml` is already a dev-dep) -> `GET /providers/duo/api/tags` returns names `["alpha","shared","beta"]`.
  - `ollama_tags_for_an_unknown_provider_is_a_not_found_error_in_ollama_shape`: `GET /providers/nope/api/tags` -> 404, body `error` is a string.
- **Done when:** the tests pass and `assert!(!models.is_empty())` style non-emptiness is covered by exact array equality.
- **Risk / notes:** catalog enforcement is irrelevant for tags. `StaticModelCatalogEntry` fields: `id`, `display_name`, `supports`, `context_length`.

### 08-9 [TEST] [P2] Make the Gemini same-format stream test actually check the stream URL
- **Where:** `CD::gemini_same_format_stream_selects_the_stream_url` (~line 1169); fake upstream `spawn_fake_upstream` (~line 248) and `Capture` type (headers + body only).
- **Now:** the test name promises URL selection, but the fake upstream answers any method/query and the capture stores no URI; the real check lives only in adapter unit tests (`crates/llm-proxy-adapters/src/gemini.rs` `stream_requests_use_stream_generate_content_with_alt_sse`).
- **How/Test:** extend `FakeUpstream` with `uri: Arc<tokio::sync::Mutex<Option<String>>>`; change the shared handler closure to take `axum::http::Uri` and record `uri.path_and_query().map(|p| p.as_str().to_owned())`; update the four route registrations (`move |uri, headers, body| handler(uri, headers, body)`); add `async fn captured_uri(&FakeUpstream)->String`. In the stream test assert `captured_uri == "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"`. Add `gemini_same_format_non_stream_keeps_generate_content_url`: `:generateContent` request -> captured URI `"/v1beta/models/gemini-2.5-flash:generateContent"` (no `alt`). Also add the cross-format counterpart `gemini_client_stream_to_gemini_upstream_requests_alt_sse` only if cheap. Fails if `gemini_stream_url` regressed.
- **Done when:** the test asserts on `captured_uri`.
- **Risk / notes:** the provider endpoint template in `state_with_provider` is `.../v1beta/models/{model}:generateContent`; `{model}` expands to the upstream model (`gemini-2.5-flash` after alias/identity).

### 08-10 [TEST] [P1] `previous_response_id` survives same-format and is rejected by name cross-format
- **Where:** `crates/llm-proxy-adapters/src/responses.rs:466-486` same-format branch (comment at 471 names `previous_response_id`); `RESPONSES_REQUEST_KEYS` in `.../client/responses.rs:32` does not list it, so cross-format it is an "extra" and `field_disposition` returns `Rejected` for Anthropic/Chat/Gemini; plan Done: "`previous_response_id` ... survive byte-for-byte."
- **Now:** `rg previous_response_id crates` finds only the comment; neither side is tested.
- **How/Test:**
  - Extend `CD::responses_same_format_passthrough_forwards_the_raw_body` with `body["previous_response_id"] = json!("resp_prev_01"); body["prompt_cache_key"] = json!("k1"); body["include"] = json!(["reasoning.encrypted_content"])` and assert `captured(&upstream)["previous_response_id"] == "resp_prev_01"` (the existing whole-body equality already holds the rest).
  - New `CD::responses_previous_response_id_is_rejected_by_name_cross_format`: Anthropic provider, body with `previous_response_id` -> 400 and text contains `previous_response_id`.
- **Done when:** both pass.
- **Risk / notes:** none.

### 08-11 [TEST] [P2] Body limits apply to the new families
- **Where:** `crates/llm-proxy-server/tests/endpoint_families.rs:73-104` (chat/messages only; helper `config_with_chat_limit`); `routes/mod.rs` `admission(family)` uses `family_max_body_bytes(config, family)`; family keys `responses`, `generate_content`, `ollama`, `models`.
- **Now:** no test exercises `FAMILY_RESPONSES`, `FAMILY_GENERATE_CONTENT`, `FAMILY_OLLAMA` limits.
- **How/Test:** generalise the helper to `config_with_limits(&[(&str, usize)])`. `new_dialect_families_have_their_own_body_limit`: configure `responses`, `generate_content` and `ollama` at 1 KiB, post an 8 KiB body (valid headers, empty provider registry like the existing tests) to `/providers/mock/v1/responses`, `/providers/mock/v1beta/models/m:generateContent`, `/providers/mock/api/chat` and `/providers/mock/api/generate` -> each `413 PAYLOAD_TOO_LARGE`; the same body to `/providers/mock/v1/messages` (no override) is not 413. A second test `limits_on_one_new_family_do_not_leak_to_another`: only `responses` at 1 KiB -> `/api/chat` with 8 KiB is not 413.
- **Done when:** the tests pass.
- **Risk / notes:** the timeout half of the plan item shares `family_request_timeout`, already covered by config tests; add nothing.

### 08-12 [DOC] [P2] "Byte-for-byte" claims are value-for-value, modulo model and prompt hygiene
- **Where:** `docs/completed/08-client-dialects.md` Done paragraph "...survive byte-for-byte" and "must reach the fake Responses upstream byte-for-byte apart from the model alias"; code `crates/llm-proxy-adapters/src/responses.rs:480-482` (`apply_prompt_hygiene`) and `serde_json::to_vec(&value)`; tests `contract_replay.rs:1489-1586` use `serde_json::Value` equality.
- **Now:** the passthrough re-serialises JSON (key order and whitespace may change) and, when the provider has `prompt_cache` hygiene (`strip_volatile`), removes configured pointers.
- **How:** reword to "are forwarded unchanged (JSON-value equal) apart from the model rewrite and any operator-configured `prompt_cache` hygiene". No code change.
- **Test:** none; optionally pin the hygiene exception with `CD::responses_passthrough_applies_configured_prompt_hygiene` only if `ProviderConfig::prompt_cache` is simple to set (`strip_volatile` pointer removed from the captured body, other fields intact).
- **Done when:** `rg -n 'byte-for-byte' docs/completed/08-client-dialects.md` returns nothing.
- **Risk / notes:** none.

### 08-13 [FIX] [P2] Remove the `let _ = index;` placeholder
- **Where:** `crates/llm-proxy-protocol/src/client/responses.rs:980-986` (`CoreEvent::ContentStart { index, kind }` arm, `ContentKind::ToolUse => { let _ = index; }`).
- **Now:** `index` is bound only to be discarded in one sub-arm.
- **How:** change the pattern to `CoreEvent::ContentStart { kind, .. }` and the arm to `ContentKind::ToolUse => {}` keeping the explanatory comment (tool items open on `ToolCallStart`). Check that no other sub-arm of that match uses `index` (the Text and Thinking arms use `self.output_index`).
- **Test:** none (compile + existing `responses.rs` stream tests at ~1536 stay green).
- **Done when:** `rg -n 'let _ = index' crates/` returns nothing.
- **Risk / notes:** none.

### 08-14 [DOC] [P2] Stale ledger doc comments list only the old dialects/routes
- **Where:** `crates/llm-proxy-storage/src/usage_query.rs:~145-151` `RequestRow::route_kind` ("`chat_completions` or `messages`") and `client_protocol` ("`openai_chat` or `anthropic`"); same wording may exist in `crates/llm-proxy-api/src/` docs and the admin UI strings (`rg "openai_chat" crates/llm-proxy-api crates/llm-proxy-admin`).
- **Now:** since plan 08 the ledger also holds `responses`, `generate_content`, ... and `openai_responses`, `gemini`, `ollama`.
- **How:** update the doc comments to list the full sets (`client_dialect_str`, `route_kind_str` in `engine/src/lifecycle.rs` are the source of truth).
- **Test:** none; 08-2 pins the values.
- **Done when:** the comments name `openai_responses`, `gemini`, `ollama`.
- **Risk / notes:** none.

### 08-15 [DOC] [P2] Plan 08 text corrections
- **Where:** `docs/completed/08-client-dialects.md`.
- **Now / How:** (1) "(21 tests)" in "Done-when verification" becomes stale once 08-2..08-11 land: update to the actual `#[tokio::test]` count in `client_dialects.rs` (`rg -c '#\[tokio::test\]' crates/llm-proxy-server/tests/client_dialects.rs`). (2) The fixtures path `fixtures/contract/captured/codex/` is relative to `crates/llm-proxy-server/tests/`; write the full path. (3) Add after 08-5/08-6: "Gemini `promptTokenCount` includes cache reads and carries `cachedContentTokenCount`; Ollama's `prompt_eval_count` counts evaluated tokens only." (4) The strict wording from 08-1. (5) State that `/api/tags` is admitted under the `models` family (generation routes use `ollama`).
- **Test:** none.
- **Done when:** the plan contains the full fixtures path and no stale count.
- **Risk / notes:** do this last so the count is right.

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: none of its own; `let _ = index;` is 08-13.
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings
- "`response.in_progress` is never emitted" (report note on row 3) — the plan lists only `response.created` ... terminal events and the captured Codex session replays against the route (`contract_replay.rs:1489`); Codex does not require `in_progress`. Not a gap; revisit only if a client needs it.
- "Tool-call ordering is asserted cross-format only" (row 5) — same-format Responses is a raw passthrough of the upstream's own events (`CD::responses_same_format_stream_round_trips`, contract replay); the synthesized ordering exists only cross-format, which is where it is tested.
- "Plan says `/api/tags` is under the `ollama` family" (row 10 informational) — the plan sentence is about "both generation routes ... under the `ollama` endpoint family"; tags is not claimed. Only the wording clarification in 08-15 (5).
- Row 17 (live Codex 0.167.0 capture provenance) and "suites pass, incl. `--no-default-features`" — NOT-STATICALLY-VERIFIABLE; no source change possible. CI job `.github/workflows/ci.yml:33-35` is the check.
- Row 19 (SDK without server crate) — IMPLEMENTED; only the passing status needs a CI run.
