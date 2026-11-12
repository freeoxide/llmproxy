# Plan 02 — Request fidelity: fix spec

Plan file: `docs/completed/02-request-fidelity.md`. All paths are relative to the repository root
and were re-checked against master @ `fe6e66f`. "Adapters" = `crates/llm-proxy-adapters/src`,
"protocol" = `crates/llm-proxy-protocol/src`. Test helpers in each adapter's `#[cfg(test)]` module:
`make_target()` (field `lossy: false`), `make_core_request(vec![CoreMessage..])`, `client_wire(&core)`
(an Anthropic-dialect client, so the encode is cross-format). To simulate another client dialect build
`ClientWireRequest::new(ClientWireFormat::OpenAiChatCompletions, serde_json::json!({...}))` directly.

## Done — leave alone
- Contract fixtures and replay (4 clients, synthesized + captured, secrets scrubbed, fake loopback upstream, CI `cargo test --workspace --all-features`): `crates/llm-proxy-server/tests/contract_replay.rs`, `tests/fixtures/contract/**`.
- Same-format raw-body passthrough with surgical edits: `adapters/src/anthropic.rs` (~:491 branch), `openai_chat.rs` (`:440`-`:505`), `responses.rs` (`:473`-`:484`), `gemini.rs` (`:371`-`:383`); unknown fields survive (`MessageRequest`/`Tool` `#[serde(flatten)] extra`).
- `deny_unknown_fields` is absent from `MessageRequest`, `Tool`, `SystemContentBlock`. Remaining hits are internal/serialize-only/admin types (see Rejected findings).
- Usage read without rewriting same-format responses: `engine/src/lifecycle.rs` (~:1401), tests in `client_dialects.rs` and `engine/tests/response_masking_passthrough.rs`.
- Rejection by name + `lossy` drop machinery: `adapters/src/client_wire.rs` `apply_field_policy`, `check_cross_format_tools`, `check_cross_format_content`, `check_output_config_subkeys`; tests `rejected_fields_are_named_in_the_error`, `lossy_mode_drops_rejected_fields_instead_of_failing`, `contract_replay.rs` rejection/lossy cases.
- `stop_sequences` decode + encode (Anthropic, Gemini, Chat).
- Reasoning translation at the provider edge: `adapters/src/reasoning.rs` (`thinking_limits`, `intent_from_sampling`, `budget_for`, `anthropic_thinking`, `chat_effort`, `gemini_budget`) with unit tests `reasoning.rs:270-405`; no translation in `llm-proxy-protocol`.
- Header allowlist for the core pipeline: `core/src/provider_config.rs` (`forward_inbound_headers`, `NEVER_FORWARDED_HEADERS`, defaults for `anthropic_messages`), `engine/src/headers.rs` `select_forwarded_headers`, `lifecycle.rs` call sites, `ProxyRequest.forwarded_headers`, tests `headers.rs` and `forwarded_headers_honour_the_allowlist_end_to_end`.
- Client request ids are never adopted (`routes/mod.rs` `inject_request_id`): intent met without a sanitizer.
- `anthropic-beta` reaches Anthropic upstream (Done-when 3): `contract_replay.rs` tests.
- Gemini signature *replay encode* (non-streaming path) and Anthropic wire `tool_use.signature`: `gemini.rs` `encode_content_part`, `build_tool_signature_map`, test in `contract_replay.rs:826`.

## Work items

### 02-1 [FIX] [P0] Do not silently drop `tool_result.is_error` on Chat, Responses and Gemini
- **Where:** `adapters/src/openai_chat.rs` `push_tool_result_messages` (~:400-421: pattern `..` ignores `is_error`, builds `ChatMessage { is_error: None, .. }`); `adapters/src/responses.rs` `encode_content_item` `CoreContent::ToolResult` arm (~:408-432: `..`, builds `FunctionCallOutput { output }`); `adapters/src/gemini.rs` `encode_content_part` `ToolResult` arm (`:257-290`: warns "is_error flag ... will be dropped"), payload from `encode_tool_result_payload` (`:324`, always `{"result": <text>}`).
- **Now:** A failed tool result is sent upstream as an ordinary success. Chat and Responses emit no warning at all; Gemini warns and drops. `CoreContent::ToolResult.is_error` is the only signal the model gets that the tool failed.
- **Wanted:** Done-when 2: "A cross-format request either carries each field or fails with an error naming it."
- **How (decision made):** CARRY, do not reject. Tool failures are routine in agent loops (Claude Code), so a 400 on every conversation that contains one would break the main use case; the field's purpose is a model-visible failure marker, which can be carried faithfully in-band.
  1. Add `pub(crate) const TOOL_ERROR_PREFIX: &str = "[tool error] ";` and `pub(crate) fn mark_tool_error(text: String, is_error: bool) -> String` (prefix when `is_error`, so an empty body becomes `"[tool error]"` trimmed of the trailing space) in `adapters/src/lib.rs`.
  2. Chat: in `push_tool_result_messages` bind `is_error` and wrap `collect_text(result_content)` with `mark_tool_error`. Keep `ChatMessage.is_error: None` (not a documented OpenAI field).
  3. Responses: same wrap on `result_text` before building `FunctionCallOutput`.
  4. Gemini: use the native convention: when `is_error` is true, the `functionResponse.response` object is `{"error": <text>}` instead of `{"result": <text>}`. Change `encode_tool_result_payload(tool_use_id, result_content, is_error)`; remove the "will be dropped" warning.
  5. Same-format paths are untouched (raw body).
- **Test:** One per adapter, in each file's test module, modelled on `encode_tool_result_as_function_call_output` (`responses.rs:~1432`) and `encode_tool_result_as_function_response` (`gemini.rs:1275`): build a `CoreMessage` with `ToolResult { is_error: true, content: [Text "disk full"] }` (plus the matching `ToolUse` for Gemini so the name resolves). Assert: Chat -> the `role:"tool"` message `content` starts with `"[tool error] "` and contains `disk full`; Responses -> `FunctionCallOutput.output` same; Gemini -> `parts[0].functionResponse.response == {"error": "disk full"}` and `result` key absent. Each also has an `is_error: false` twin asserting no prefix / `result` key, so the marker is not unconditional.
- **Done when:** `grep -rn "will be dropped" adapters/src/gemini.rs` returns nothing; the three tests pass and fail on the old code.
- **Risk / notes:** The marker text is model-visible; keep it a single constant. Document the marker in `docs/` request-fidelity text only as a one-line fact (see 02-21). Lossy mode needs no special case.

### 02-2 [FIX] [P0] Carry `tool_choice` to Gemini as `toolConfig.functionCallingConfig`
- **Where:** `adapters/src/gemini.rs:528-533` (`if core.tool_choice.is_some() { tracing::warn!("... not yet forwarded ...") }`); request type `protocol/src/zen.rs` `GeminiRequest` (`:217`, fields `contents`, `generation_config`, `tools`, `stream`); inverse decode `protocol/src/client/gemini.rs:297` `decode_tool_config`.
- **Now:** `tool_choice` (including `none` and a forced tool) is ignored; the model may call tools the client forbade or skip a required one.
- **Wanted:** Done-when 2 (carry or fail naming).
- **How (decision made):** carry, since Gemini supports it exactly.
  1. Add to `GeminiRequest` an optional `tool_config: Option<serde_json::Value>` with `#[serde(rename = "toolConfig", default, skip_serializing_if = "Option::is_none")]` (match how `generation_config` is named on the wire today; the REST API accepts either case, but use `toolConfig`).
  2. In `encode_request`, replace the warn block: `Auto` -> `{"functionCallingConfig":{"mode":"AUTO"}}`; `Any` -> `mode:"ANY"`; `None` -> `mode:"NONE"`; `Tool{name}` -> `mode:"ANY"` with `allowedFunctionNames:[name]`. Only emit when `tools` is non-empty (Gemini rejects `toolConfig` without tools for ANY); for `None`/`Auto` with no tools, omit.
  3. `CoreToolChoice::Raw(_)` and any unknown (`non_exhaustive`) variant: `Err(ProviderError::InvalidRequest("tool_choice <debug name> has no gemini equivalent; remove it or enable the route's lossy mode"))`, unless `target.lossy`, in which case warn and omit.
  4. Fix every `GeminiRequest { .. }` literal (grep `GeminiRequest {`) for the new field.
- **Test:** `gemini.rs` tests: `encode_tool_choice_maps_to_tool_config` — table over the four variants asserting the serialized `toolConfig` JSON exactly (including `allowedFunctionNames == ["get_weather"]` for `Tool`); `encode_tool_choice_none_without_tools_omits_tool_config`; `encode_raw_tool_choice_is_rejected_by_name` (error message contains `tool_choice`) and its `lossy: true` twin returns `Ok` with no `toolConfig`. Round-trip assertion with the decoder: encoding `Tool{name}` then feeding the JSON to `protocol::client::gemini::decode_request` yields `CoreToolChoice::Tool{name}`.
- **Done when:** `grep -n "not yet forwarded" adapters/src/gemini.rs` returns nothing; tests pass.
- **Risk / notes:** Tool-name sanitisation: Gemini function names allow `[A-Za-z0-9_.-]`; use the same name string the `functionDeclarations` entry uses (see how tool names are emitted at `gemini.rs:~480`) so the forced name matches.

### 02-3 [FIX] [P0] Unknown `tool_choice` variants: reject by name (Chat, Responses, Anthropic)
- **Where:** `adapters/src/openai_chat.rs:~700-712` (`tool_choice` closure, `_ => { warn "unknown tool_choice variant, omitting"; None }`); `adapters/src/responses.rs:~626-640` (`_ => { warn ...; None }`); `adapters/src/anthropic.rs:1107` `encode_tool_choice` (`_ => None`, silent).
- **Now:** `CoreToolChoice` is `#[non_exhaustive]`; a future variant is silently omitted (Anthropic: with no log at all).
- **Wanted:** Done-when 2.
- **How:** Make the three encoders return `Result<Option<Value>, ProviderError>`; the wildcard arm returns `ProviderError::InvalidRequest(format!("tool_choice {tc:?} has no {protocol} equivalent; remove it or enable the route's lossy mode"))`, except `target.lossy` which warns and returns `Ok(None)`. Thread `target.lossy` into the helper (for Anthropic, `encode_tool_choice_for_target` at `:805` already has `core`/`client`; add `target`). Today's variants are all handled by named arms, so no existing request changes behaviour.
- **Test:** The wildcard is unreachable with today's enum, so test the helper seam: extract the per-variant mapping into a function `fn tool_choice_value(tc: &CoreToolChoice) -> Option<Value>` and the policy into `fn resolve_tool_choice(tc, lossy, protocol) -> Result<Option<Value>, ProviderError>`; unit-test `resolve_tool_choice` with a closure/stub mapper returning `None` (non-lossy -> `Err` whose text contains `tool_choice`; lossy -> `Ok(None)`). Keep the existing `encode_tool_choice*` tests green.
- **Done when:** no `omitting` warn remains for tool_choice in `openai_chat.rs`/`responses.rs`; `encode_tool_choice` in `anthropic.rs` has no `_ => None`.
- **Risk / notes:** Low value today (unreachable), but it removes the last silent-drop path in the Done-when-2 sweep. Keep the change minimal; do not add new variants.

### 02-4 [FIX] [P0] Fail (or honour lossy) when requested thinking cannot fit under `max_tokens`
- **Where:** `adapters/src/reasoning.rs` `anthropic_thinking` (~:196-215): `if headroom < limits.min { warn!("thinking budget cannot fit under max_tokens; dropping thinking"); return Ok(None) }`; callers `adapters/src/anthropic.rs:642` (same-format) and `:740` (cross-format, passes `max_tokens.unwrap_or(4096)`).
- **Now:** The client asked for reasoning (`thinking`, `reasoning_effort` or `output_config.effort`) and silently gets none.
- **Wanted:** Done-when 2.
- **How (decision made: reject unless lossy):** add a `lossy: bool` parameter to `anthropic_thinking`. When the budget cannot fit and `!lossy`, return `ProviderError::InvalidRequest(format!("thinking budget {budget} cannot fit under max_tokens {max_tokens} (minimum thinking budget for {model} is {min}); raise max_tokens above {min_plus_one} or remove thinking/reasoning_effort"))`. When `lossy`, keep the current warn-and-drop. Pass `target.lossy` from both callers.
- **Test:** `reasoning.rs` tests: `thinking_that_cannot_fit_is_an_error_naming_the_limit` (sampling `reasoning_effort="high"`, model `claude-sonnet-4-5`, `max_tokens=Some(1000)`, `lossy=false` -> `Err` containing `max_tokens` and `1024`); `thinking_that_cannot_fit_is_dropped_when_lossy` (same input, `lossy=true` -> `Ok(None)`); `thinking_that_fits_is_unchanged` (`max_tokens=Some(8000)` -> `Some` with `budget_tokens < 8000`). Plus a cross-format adapter test in `anthropic.rs` with a Chat-style client `reasoning_effort:"low"` and `max_tokens: 512` asserting the encode errors.
- **Done when:** the tests pass; no code path returns `Ok(None)` from `anthropic_thinking` on a fit failure while `lossy` is false.
- **Risk / notes:** Behaviour change: clients that send tiny `max_tokens` plus a reasoning knob now get a 400 instead of a non-thinking answer. This is the "fail naming it" half of Done-when 2; `lossy` preserves the old behaviour. When `max_tokens` is absent the 4096 default applies (cross-format), so the common case is unaffected.

### 02-5 [FIX] [P1] Clamp Gemini same-format `thinkingBudget` per model
- **Where:** `adapters/src/gemini.rs:371-383` (same-format branch returns `(*client.body).clone()` unchanged); helper `reasoning.rs` `thinking_limits` / `clamp_budget`.
- **Now:** A Gemini-dialect client's `generationConfig.thinkingConfig.thinkingBudget` (or `generation_config.thinking_config.thinking_budget`) reaches the wire unclamped, contradicting step 5 "Stop forwarding these fields unfiltered" and the Done text "same-format passthrough re-derives the fields through the same filter" (true for Anthropic and Chat only).
- **Wanted:** Plan step 5.
- **How:** In the same-format branch, before serialization, locate the budget in both spellings (`generationConfig`/`generation_config` -> `thinkingConfig`/`thinking_config` -> `thinkingBudget`/`thinking_budget`). If it is an integer: leave `-1` (Gemini dynamic) and `0` (disable) untouched; for any other value apply `clamp_budget(value, thinking_limits(&target.upstream_model))` and `tracing::warn!` when it changed. Non-integer values are rejected: `ProviderError::InvalidRequest("thinkingBudget must be an integer")`. Do not touch any other field.
- **Test:** `gemini.rs` tests with a `ClientWireRequest::new(ClientWireFormat::GeminiGenerateContent, ...)` body: `same_format_thinking_budget_is_clamped_to_model_max` (`thinkingBudget: 100000`, model `gemini-2.5-pro` -> `32768` on the wire); `same_format_thinking_budget_minus_one_and_zero_pass_through`; `same_format_without_thinking_config_is_byte_identical` (assert the body bytes equal `serde_json::to_vec(&client.body)`, protecting the passthrough contract); snake_case spelling case.
- **Done when:** tests pass.
- **Risk / notes:** Keeps the "unknown fields survive" guarantee; only one numeric leaf is edited. Update the Done text in 02-21.

### 02-6 [DECIDE] [P1] `response_format` cross-format (Gemini, Responses)
- **Where:** `adapters/src/client_wire.rs:238` `field_disposition` (no entry; falls to `Rejected`); encoders `adapters/src/responses.rs` and `gemini.rs`; Gemini config type `protocol/src/zen.rs:401` `GeminiGenerationConfig`.
- **Now:** Chat -> Responses/Gemini/Anthropic with `response_format` is a 400 naming it (test `contract_replay.rs:727`, `client_wire.rs:575`). Both Responses (`text.format`) and Gemini (`generationConfig.responseMimeType` + `responseSchema`/`responseJsonSchema`) support structured output natively. Plan step 4: "Carry `response_format` ... wherever the provider supports them."
- **Options:** (A) implement Responses and Gemini mappings; (B) keep rejection and narrow the plan text.
- **Recommendation: (A) for Responses and Gemini, keep Anthropic rejected.** The mapping is mechanical and structured output is a common client need; rejecting it makes OpenAI-SDK structured-output code unusable through a non-OpenAI-chat upstream. Anthropic stays rejected (its structured output is a different beta surface).
- **How:**
  1. `field_disposition`: `response_format` -> `Mapped` for `OPENAI_RESPONSES` and `GEMINI_GENERATE_CONTENT` when the client is Chat (see 02-11 for the per-pair table; until then, key on provider only).
  2. Responses: `{"type":"text"}` -> omit; `{"type":"json_object"}` -> `text: {"format": {"type":"json_object"}}`; `{"type":"json_schema","json_schema":{name,schema,strict,description}}` -> `text: {"format": {"type":"json_schema","name":..,"schema":..,"strict":..,"description":..}}` (flattened one level). Put it in `ResponsesRequest.extra["text"]`. Unknown `type` -> `InvalidRequest` naming `response_format.type`.
  3. Gemini: add `response_mime_type` (`responseMimeType`) and `response_json_schema` (`responseJsonSchema`) optional fields to `GeminiGenerationConfig`; `json_object` -> mime `application/json`; `json_schema` -> mime `application/json` plus `responseJsonSchema = json_schema.schema`. Before coding, verify `responseJsonSchema` against the current Gemini API reference; if it is not available, reject `json_schema` by name and map only `json_object`.
  4. Add `response_format` to the shared "mapped, not carried" guard so it is not also copied verbatim.
- **Test:** `responses.rs`: `chat_json_schema_response_format_maps_to_text_format` (assert exact `body["text"]["format"]`), `chat_json_object_maps`, `unknown_response_format_type_is_rejected_by_name`. `gemini.rs`: `chat_json_object_sets_response_mime_type`, `chat_json_schema_sets_response_json_schema`. Update `contract_replay.rs:727` rejection test: keep `response_format` in the Anthropic-provider case only; Responses/Gemini cases now assert the mapped wire field. Same-format Chat/Responses/Gemini tests (verbatim) stay unchanged.
- **Done when:** `response_format` is no longer in the rejected-field list for Responses/Gemini in `contract_replay.rs`; the new tests pass.
- **Risk / notes:** Schema dialect differences (OpenAI strict schemas vs Gemini) can cause upstream 400s; those surface as upstream errors, not silent loss. If (B) is chosen instead: add `response_format` to the Done text as "rejected by name cross-format" and add no code.

### 02-7 [DECIDE] [P1] `seed` cross-format
- **Where:** `adapters/src/client_wire.rs:238` (no `seed` entry); `adapters/src/gemini.rs:503-522` (`GeminiGenerationConfig` literal); `protocol/src/zen.rs:401`.
- **Now:** Chat -> Gemini/Responses/Anthropic with `seed` is a 400 (test `client_wire.rs:604-620`). Gemini supports `generationConfig.seed` (int); Responses and Anthropic have no seed.
- **Options:** (A) map `seed` to Gemini; (B) reject everywhere.
- **Recommendation: (A) Gemini only.** One optional field, exact semantics; Responses and Anthropic keep rejecting by name.
- **How:** add `seed: Option<i64>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`) to `GeminiGenerationConfig`; `field_disposition(GEMINI, "seed") -> Mapped`; in `encode_request` read `client.body["seed"].as_i64()` (non-integer -> `InvalidRequest` naming `seed`) like the existing `top_k` fallback at `gemini.rs:~501`.
- **Test:** `gemini.rs`: `chat_seed_maps_to_generation_config_seed` (Chat-dialect `ClientWireRequest` with `"seed": 7` -> serialized `generation_config.seed == 7`). Keep `client_wire.rs` rejection test for the Responses/Anthropic provider (adjust the assertion helper if it used Gemini).
- **Done when:** test passes; `seed` listed as rejected only for Responses and Anthropic in docs.
- **Risk / notes:** None.

### 02-8 [DECIDE] [P1] `parallel_tool_calls`, `store`, `prompt_cache_key` into Chat providers
- **Where:** `adapters/src/client_wire.rs:250-262` (`OPENAI_CHAT_COMPLETIONS` arm has only `service_tier` Carried and `output_config` Mapped).
- **Now:** A Responses-dialect client sending these three to a Chat upstream gets a 400 naming them, although the Chat Completions API accepts all three with identical names and shapes.
- **Options:** (A) mark Carried for Chat; (B) keep rejecting.
- **Recommendation: (A).** Same name, same shape; `OPENAI_RESPONSES` already carries them.
- **How:** In the `OPENAI_CHAT_COMPLETIONS` arm add `"parallel_tool_calls" | "store" | "prompt_cache_key" => Carried`. Confirm the Chat encoder injects carried fields (`req.extra = carried` at `openai_chat.rs:775`).
- **Test:** `openai_chat.rs`: `responses_client_extras_reach_chat_upstream` — `ClientWireRequest::new(OpenAiResponses, json!({"model":.., "input":[], "parallel_tool_calls": false, "store": true, "prompt_cache_key": "k"}))` -> encoded body has all three verbatim. Update `client_wire.rs` `carried_fields_survive_the_policy` to include the Chat provider.
- **Done when:** the test passes and the three names no longer appear in a rejection for Chat in `contract_replay.rs`.
- **Risk / notes:** OpenAI-compatible third-party Chat upstreams may ignore or reject `store`/`prompt_cache_key`; that is the client's explicit request, and the upstream's error surfaces to it.

### 02-9 [DECIDE] [P2] `output_config` into Anthropic providers + CI coverage of same-format passthrough
- **Where:** `adapters/src/client_wire.rs:242-249` (`ANTHROPIC_MESSAGES` arm: only `service_tier | top_k` Carried, `parallel_tool_calls` Mapped); `adapters/src/anthropic.rs` cross-format `for (name, value) in carried` (~:757).
- **Now:** A non-Anthropic client sending `output_config` to an Anthropic upstream gets a 400 naming it; Anthropic accepts `output_config` natively. Same-format Anthropic passthrough forwards it, but no CI test names it (only the manual probe).
- **Options:** (A) Carried; (B) keep rejecting.
- **Recommendation: (A) Carried for Anthropic.** Native field, identical shape.
- **How:** add `"output_config"` to the Anthropic `Carried` arm. Note `check_output_config_subkeys` only inspects providers where the disposition is `Mapped`, so no subkey filtering applies for Anthropic.
- **Test:** `anthropic.rs`: `same_format_passthrough_keeps_output_config` (Anthropic-dialect body with `"output_config": {"effort":"low","format":{"type":"json_schema","schema":{}}}` -> encoded body contains it unchanged) and `cross_format_output_config_is_carried_to_anthropic` (Chat-dialect client with the same key -> present in the body, not rejected).
- **Done when:** both tests pass.
- **Risk / notes:** If a Chat client sends `reasoning_effort` and `output_config.effort` together, both reach Anthropic; acceptable (client's explicit input).

### 02-10 [DECIDE] [P2] Server tools cross-format
- **Where:** `adapters/src/client_wire.rs` `check_cross_format_tools`; Anthropic `encode_tools_for_target` (`adapters/src/anthropic.rs:767`).
- **Now:** Server tools (Anthropic `web_search_*`, Responses `web_search`, Gemini `googleSearch`) are carried same-format and rejected by name cross-format; no provider-to-provider mapping.
- **Options:** (A) implement per-pair server-tool mappings; (B) keep the rejection and narrow the plan wording.
- **Recommendation: (B).** The tools differ in result shape (citations, `server_tool_use`/`web_search_tool_result` blocks, grounding metadata), so a mapping would change observable responses; a named 400 (or lossy drop) is the honest outcome. No code change.
- **How:** Doc only (02-21): replace "server tools ... wherever the provider supports them" with "server tools reach same-format upstreams verbatim and are rejected by name cross-format".
- **Test:** none new; `contract_replay.rs:493-573` and `client_wire.rs:744` already cover both sides.
- **Done when:** plan text states the narrowing.
- **Risk / notes:** None.

### 02-11 [DECIDE] [P1] Key the disposition table per (client format, provider protocol)
- **Where:** `adapters/src/client_wire.rs:238` `field_disposition(provider, field)`; callers `apply_field_policy` (`:292`) and `check_output_config_subkeys` (`:489`). The function is `pub` but unused outside this file (grep confirmed).
- **Now:** The table is keyed on provider protocol only; the client dialect enters only via `extra_field_names` (`:189`). Plan step 3: "one table per format pair".
- **Options:** (A) re-key to the pair; (B) keep and fix the wording.
- **Recommendation: (A).** It is the mechanism that lets 02-6 to 02-9 carry a field for one pair (e.g. Chat -> Gemini `seed`) while rejecting it for another, and it makes the table testable cell by cell. Cheap: the function is crate-internal in practice.
- **How:** Change the signature to `field_disposition(client: ClientWireFormat, provider: &ProtocolName, field: &str)`. Restructure as `match (client, provider.as_str())` with the current per-provider arms as the fallback for each client format, so behaviour is identical before 02-6..02-9. Pass `client.format` from both call sites. Document the table in the function's doc comment as a grid (rows = provider, columns = client format only for cells that differ).
- **Test:** `client_wire.rs`: a table-driven test `disposition_is_per_format_pair` asserting at least: (OpenAiResponses, OPENAI_CHAT, `parallel_tool_calls`) = Carried; (OpenAiChatCompletions, GEMINI, `seed`) = Mapped; (OpenAiResponses, GEMINI, `seed`) = Rejected (if 02-7 chosen); (OpenAiChatCompletions, ANTHROPIC, `response_format`) = Rejected. Existing tests must pass unchanged.
- **Done when:** `grep -n "field_disposition(" adapters/src/client_wire.rs` shows the three-argument form everywhere.
- **Risk / notes:** Do 02-11 first or together with 02-6..02-9 to avoid editing the same match arms twice. Do not add a re-export.

### 02-12 [FIX] [P1] One definition of the never-forwarded header set; validation must reject `browser-*`
- **Where:** `core/src/provider_config.rs:~2397-2431` `validate_forwarded_inbound_headers` (checks `NEVER_FORWARDED_HEADERS`, `sec-`, `access-control-`); `engine/src/headers.rs:8-17` (`NEVER_FORWARDED_PREFIXES = ["sec-","access-control-","browser-"]`, `header_is_never_forwarded`).
- **Now:** A `browser-*` entry in `forward_inbound_headers` passes config validation and is then silently dropped at runtime. The prefix list is defined twice and has already diverged. Plan step 7: the never-forwarded set is "enforced again at config validation".
- **Wanted:** Same set at both layers.
- **How:** In `provider_config.rs` next to `NEVER_FORWARDED_HEADERS` add `pub const NEVER_FORWARDED_HEADER_PREFIXES: &[&str] = &["sec-", "access-control-", "browser-"];` and `pub fn is_never_forwarded_header(lower_name: &str) -> bool`. Use it in `validate_forwarded_inbound_headers` (replace the two `starts_with` calls) and in `engine/src/headers.rs` (delete the private const and fn, call `llm_proxy_core::provider_config::is_never_forwarded_header`). Do not re-export from the crate root.
- **Test:** `provider_config.rs`: extend `forwarded_inbound_allowlist_rejects_browser_and_origin_headers` (`:~3957`) list with `"browser-use"` and `"Sec-CH-UA"` (mixed case) asserting `ForbiddenForwardedHeader`. `engine/src/headers.rs`: a test that iterates a sample drawn from `NEVER_FORWARDED_HEADERS` plus one name per prefix and asserts `select_forwarded_headers` drops each (guards the shared helper).
- **Done when:** `grep -n "NEVER_FORWARDED_PREFIXES" engine/src/headers.rs` returns nothing; the tests pass.
- **Risk / notes:** Existing configs with a `browser-*` allowlist entry will now fail validation at startup (they were no-ops before); mention in the changelog line if the repo keeps one.

### 02-13 [FIX] [P1] Endpoint-family forwarder must use `select_forwarded_headers`
- **Where:** `crates/llm-proxy-server/src/routes/families.rs:365-369` in `forward()` (loop over `target.forward_inbound_headers`, `outbound.headers.get(name)`, inserts raw into `extra_headers`); helper `engine/src/headers.rs` `select_forwarded_headers`; HeaderMap -> map conversion `routes/core_pipeline.rs:224` `inbound_headers_for_engine`.
- **Now:** Family calls (embeddings, rerank, audio, images, files, batches) forward configured names directly: no never-forwarded check, no CR/LF value check. Protection rests solely on config validation (and 02-12 shows validation lagged the runtime set).
- **Wanted:** One runtime filter for every forwarding path.
- **How:** Replace the loop with `let inbound = inbound_headers_for_engine(outbound.headers); for (name, value) in llm_proxy_engine::headers::select_forwarded_headers(&inbound, &target.forward_inbound_headers) { extra_headers.insert(name, value); }` (`inbound_headers_for_engine` is `pub(crate)` in the same crate; `target.forward_inbound_headers` is `Arc<Vec<String>>`, deref to `&[String]`). Keep the `ProxyRequest` construction as is.
- **Test:** `crates/llm-proxy-server/tests/endpoint_family_handlers.rs`: add `family_forwarding_honours_allowlist_and_never_forwarded_set`. Build the provider via the existing `provider_config` helper but set one adapter's `forward_inbound_headers: Some(vec!["x-custom-trace".into(), "browser-use".into(), "origin".into()])` directly on the struct (bypassing validation, as other tests do); send an embeddings request with those three headers; at the fake upstream assert `x-custom-trace` arrived and `browser-use`/`origin` did not. Fails on current code (the latter two arrive). Gate with the same cfg/feature as the rest of that file.
- **Done when:** `grep -n "forward_inbound_headers" crates/llm-proxy-server/src/routes/families.rs` shows only the call into `select_forwarded_headers`; test passes.
- **Risk / notes:** Header names reach the upstream lowercased (as in the core pipeline); HTTP is case-insensitive.

### 02-14 [FIX] [P1] Capture Gemini `thoughtSignature` in streaming responses
- **Where:** `adapters/src/gemini.rs` `GeminiStreamDecoder::decode_frame` (`:44-125`, calls `push_function_call` for each `functionCall` part, `:175`) ignores `part.thought_signature`; `protocol/src/core.rs:~806` `CoreEvent::ToolCallStart { index, id, name }` has no signature slot; non-streaming `decode_response` already captures it (`gemini.rs:600-605`).
- **Now (new finding, not in the report):** Plan step 6 and Done say signatures are "captured on `functionCall` parts and re-emitted" and "ride the Anthropic client wire on `tool_use.signature`". That holds only for non-streaming. A streamed Gemini tool call loses the signature, so the next turn replays the call without it (Gemini thinking models then reject or degrade the replay).
- **Wanted:** Step 6 "Preserve Gemini thought signatures ... across turns", for both modes.
- **How:**
  1. Add `signature: Option<String>` to `CoreEvent::ToolCallStart` with `#[serde(default, skip_serializing_if = "Option::is_none")]` and update its manual `Debug` impl (redact like `CoreContent::ToolUse`, `core.rs:246`). Fix all constructors/matchers (about 40 sites; find with `grep -rn "ToolCallStart" crates apps`): other decoders pass `signature: None`; client encoders ignore it unless noted below.
  2. `push_function_call` takes `signature: Option<&str>` from `part.thought_signature` and sets it on `ToolCallStart`.
  3. Client stream encoder for Anthropic dialect (`protocol/src/client/anthropic.rs`, the `ToolCallStart` handling that builds the `content_block_start` `tool_use` block): include `"signature": <sig>` on the block when present, mirroring the non-stream `tool_use.signature` encode (`:311-317`, `:421-427`). Other client encoders (Chat, Responses, Gemini, Ollama) ignore it.
  4. Engine: confirm the stream assembly in `engine/src/lifecycle.rs` does not rebuild `ToolCallStart` without the field (grep returned no hits there, so no change expected).
- **Test:** `gemini.rs`: `stream_function_call_captures_thought_signature` — feed a frame whose part is `{"functionCall":{"name":"get_weather","args":{"city":"SF"}},"thoughtSignature":"sig-1"}`; assert the emitted `ToolCallStart.signature == Some("sig-1")`. `protocol/src/client/anthropic.rs`: `stream_tool_call_start_with_signature_emits_signature_on_block` asserting the encoded `content_block_start` JSON has `content_block.signature == "sig-1"`, and `..._without_signature_omits_the_key`. Optional end-to-end in `contract_replay.rs`: Gemini fake upstream streaming a signed call to an Anthropic-dialect client, then replaying the returned `tool_use` (with `signature`) and asserting the second upstream request's `functionCall` part carries `thoughtSignature` (extends the test at `:826`).
- **Done when:** the tests pass; a streamed signed call round-trips.
- **Risk / notes:** Touches a core type (`CoreEvent` has `deny_unknown_fields`; the new field is `default`, so existing JSON fixtures such as `tests/fixtures/openai_chat/streaming-tool/core-events.json` still deserialize). Largest item in this spec; do it after the P0 items.

### 02-15 [TEST] [P1] Gemini signature capture from the non-streaming response, from a wire body
- **Where:** `adapters/src/gemini.rs:600-605` (`signature: part.thought_signature.clone()` in `decode_response`); `contract_replay.rs:826-916` builds the replay `CoreRequest` in code, so response-side capture is untested.
- **Now:** The first half of "preserved across turns" (response -> core) has no assertion.
- **Wanted:** Step 6.
- **How:** Tests only.
- **Test:** `gemini.rs`: `decode_response_captures_thought_signature_on_function_call` — JSON response with a candidate part `{"functionCall":{"name":"get_weather","args":{"city":"SF"}},"thoughtSignature":"sig-1"}`; `decode_response`; assert `content` contains `CoreContent::ToolUse { signature: Some("sig-1"), .. }`. Then a full-loop test in `contract_replay.rs` next to `:826`: run the Gemini fake upstream returning that body for an Anthropic-dialect client, take the client's `tool_use` block (assert `signature` present), post it back in the next turn as a `tool_use` + `tool_result`, and assert the captured second upstream request has `thoughtSignature` on both the `functionCall` and `functionResponse` parts.
- **Done when:** both tests pass; replacing `part.thought_signature.clone()` with `None` fails them.
- **Risk / notes:** Pair with 02-14 (same helper fixtures).

### 02-16 [TEST] [P1] Cross-format image legs: Gemini `inlineData`/`fileData`, OpenAI `image_url`, Anthropic
- **Where:** `adapters/src/gemini.rs:299-327` `neutral_image_to_gemini_part`; `adapters/src/openai_chat.rs:338-360` `neutral_image_to_openai_part`; `adapters/src/anthropic.rs:~984` image encode. Only the decode (`protocol/src/client/openai_chat.rs:944`) and one Anthropic leg (`contract_replay.rs:727-825`) are asserted.
- **Now:** The "image_url parts carried" claim is untested for two of three targets.
- **Wanted:** Step 4 `image_url`.
- **How:** Tests only.
- **Test:** `gemini.rs`: `base64_image_encodes_as_inline_data` (`CoreContent::Image { source: json!({"type":"base64","media_type":"image/png","data":"AAAA"}) }` in a user message -> `parts[0].inlineData == {"mimeType":"image/png","data":"AAAA"}`); `url_image_encodes_as_file_data` (source `{"type":"url","url":"https://x/y.png"}` -> `fileData.fileUri == "https://x/y.png"`, `mimeType == "image/png"` default). `openai_chat.rs`: `base64_image_encodes_as_data_url_image_part` (-> content part `{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}`) and `url_image_encodes_as_image_url_part`. Add the unreadable-source case: `{"type":"file",...}` is rejected by name via `check_cross_format_content` (already implemented; assert the error contains `type=image`).
- **Done when:** four tests pass; swapping `inline_data`/`file_data` constructors fails them.
- **Risk / notes:** Check serialized key spellings against `zen.rs:276-296` (`inlineData`, `fileData`, `mimeType`, `fileUri`).

### 02-17 [TEST] [P2] `top_k` cross-format encode (Gemini and Anthropic)
- **Where:** `adapters/src/gemini.rs:503-510` (core `top_k`, falling back to raw Chat body `top_k`); `adapters/src/anthropic.rs:722-724`; table `client_wire.rs:245`/`:268`.
- **Now:** Only same-format `top_k` is asserted (`contract_replay.rs:378, 1025`).
- **Wanted:** Step 4 `top_k`.
- **How:** Tests only.
- **Test:** `gemini.rs`: `top_k_from_core_maps_to_generation_config` (`core.sampling.top_k = Some(5)` -> serialized `generation_config.topK == 5`) and `top_k_from_chat_body_extra_maps_to_generation_config` (Chat-dialect `ClientWireRequest` body `{"top_k": 7}` -> `topK == 7`, and the request is not rejected). `anthropic.rs`: `top_k_from_core_reaches_anthropic_body` (`body["top_k"] == 5`).
- **Done when:** three tests pass.
- **Risk / notes:** None.

### 02-18 [TEST] [P1] Gemini `thinkingConfig` reaches the wire
- **Where:** `adapters/src/gemini.rs:494-499`; `protocol/src/zen.rs:415-430` (`thinkingConfig`, `thinkingBudget` renames). Only the helper is tested (`reasoning.rs:389` `gemini_budget_clamps_per_model`). The existing `encode_reasoning_effort_omitted` (`gemini.rs:1616`) asserts `temperature == None` and does not check thinking.
- **Now:** A rename or wiring regression of `thinkingConfig` would pass CI.
- **Wanted:** Step 5 "Gemini's `thinkingBudget`".
- **How:** Tests only; also rename the misleading `encode_reasoning_effort_omitted`.
- **Test:** `gemini.rs`: `reasoning_effort_becomes_clamped_thinking_budget_on_the_wire` — `core.sampling.reasoning_effort = Some("high")`, model `gemini-2.5-pro`; parse `proxy_req.body` as `serde_json::Value`; assert `generation_config.thinkingConfig.thinkingBudget` equals the value `reasoning::gemini_budget` yields (compute via the helper's documented table, e.g. clamp of the `high` budget into `128..=32768`) and is an integer. `thinking_budget_from_anthropic_thinking_object`: `sampling.thinking = {"type":"enabled","budget_tokens": 100000}` -> `thinkingBudget == 32768`. `no_reasoning_means_no_thinking_config`: key absent. Rename `encode_reasoning_effort_omitted` to match what it asserts or fold its assertion into the new test.
- **Done when:** tests pass; renaming the serde attr to `thinking_budget` fails them.
- **Risk / notes:** Pick the `generation_config` vs `generationConfig` key from an actual serialized body, not from memory (`GeminiRequest` has no rename today).

### 02-19 [TEST] [P1] OpenAI `reasoning_content` is replayed outbound
- **Where:** `adapters/src/openai_chat.rs:366-380` `encode_assistant_content` (Thinking -> `reasoning_content`, empty text preserved) and the assistant-message builder that consumes `reasoning_content`; same-format raw at `:461`. Codec-only tests exist in `protocol/src/client/openai_chat.rs:1004, 1198, 1540`; the `thinking_request` adapter fixture has user-side thinking only.
- **Now:** No adapter-level test with an assistant `CoreContent::Thinking`.
- **Wanted:** Step 6 "OpenAI `reasoning_content` across turns", including "explicitly empty ... round-trips".
- **How:** Tests only.
- **Test:** `openai_chat.rs`: `assistant_thinking_is_replayed_as_reasoning_content` — messages `[user "q", assistant [Thinking{text:"plan", signature:None}, Text "a"], user "next"]`; assert the assistant `ChatMessage.reasoning_content == Some("plan")` and `content == "a"`. `empty_thinking_block_is_replayed_as_empty_reasoning_content` — `Thinking{text:""}` -> `reasoning_content == Some("")` (the key is present in the serialized JSON: assert `body["messages"][1]["reasoning_content"] == ""`). `multiple_thinking_blocks_concatenate`.
- **Done when:** tests pass; dropping the `Thinking` arm fails them.
- **Risk / notes:** None.

### 02-20 [FIX] [P2] Warn-only drops that remain in Responses/Gemini/Chat encoders: classify and enforce
- **Where:** `adapters/src/responses.rs:~613-625` (stop sequences "not forwarded", `user_id` "not forwarded"); `adapters/src/gemini.rs:~535` (`user_id`); non-text blocks inside `tool_result` content dropped with a warning: `responses.rs` (`dropping non-text content block in ToolResult encoding`), `openai_chat.rs:905` `collect_text` ("dropping non-text content block"), `gemini.rs:340` `encode_tool_result_payload` ("dropping non-text content in tool result"); cache-marker drops ("cache_control is not supported, dropping cache marker") in all three encoders.
- **Now:** Done-when 2 is absolute ("carries each field or fails naming it"); these are remaining silent-ish drops.
- **Wanted:** Each either carried, rejected by name, or explicitly exempt.
- **How (decisions made):**
  1. `stop` sequences to Responses: the API has no equivalent, so reject by name: `InvalidRequest("stop sequences have no openai_responses equivalent; remove them or enable the route's lossy mode")`; warn-and-drop under `target.lossy`.
  2. `user_id`: carry to Responses as `user` (accepted field); Gemini has no field and `metadata.user_id` is attribution only (Claude Code sends it on every request), so EXEMPT: keep the debug-level log, change the level from `warn` to `debug`, and document it as non-semantic in 02-21. Do not reject.
  3. Cache markers: EXEMPT (hints with no effect on output) on non-Anthropic targets; downgrade the warn to `debug` and keep.
  4. Non-text content inside `tool_result` (images etc.): extend `uncarried_block_label`/`check_cross_format_content` (`client_wire.rs:~416-470`) to also walk `CoreContent::ToolResult.content` and report `messages[m].content[b].content[k] type=image` for any non-`Text` inner block, so these are rejected by name (dropped under lossy) instead of vanishing in `collect_text`/the Responses/Gemini loops. Same-format passthrough is unaffected.
- **Test:** `responses.rs`: `stop_sequences_are_rejected_by_name` (+ lossy twin `Ok`), `user_id_is_carried_as_user`. `client_wire.rs`: `image_inside_tool_result_is_rejected_by_name` (core with a `ToolResult` whose content has an `Image`; error contains `content[` and `type=image`) and `text_only_tool_result_passes`. One adapter-level test (Chat) proving the 400 surfaces through `encode_request`.
- **Done when:** the tests pass; `grep -n "dropping non-text content" adapters/src/*.rs` shows only lossy-guarded paths.
- **Risk / notes:** Behaviour change: Anthropic->Chat conversations whose tool results contain images now 400 instead of silently losing the image; `lossy` restores the old behaviour. Item 4 is the most invasive; if scope is tight, ship 1-3 and list 4 as a follow-up with the 02-21 wording "non-text tool_result content is dropped with a warning".

### 02-21 [DOC] [P2] Correct stale plan text
- **Where:** `docs/completed/02-request-fidelity.md`.
- **Now / wanted (edit each):**
  1. Done, step 1: "Codex traffic needs the Responses client route (plan 08) and is not replayable yet." is false; Codex replay exists (`contract_replay.rs:1311, 1413, 1489`, fixtures under `captured/codex/` and `codex/`). Replace with a sentence naming the Codex fixtures.
  2. Paths: `adapter/reasoning.rs`, `adapter/client_wire.rs` -> `crates/llm-proxy-adapters/src/reasoning.rs`, `crates/llm-proxy-adapters/src/client_wire.rs`; `ClientWireRequest in crates/llm-proxy-provider` -> `crates/llm-proxy-adapters/src/client_wire.rs`; To-do 7 `crates/llm-proxy-provider/src/transport.rs` -> `crates/llm-proxy-adapters/src/request.rs` (`ProxyRequest`) and `crates/llm-proxy-transport/src/client.rs`.
  3. To-do 3 "per-route `lossy`" -> "per-adapter `lossy`" (`ProviderAdapterConfig.lossy`, `provider_config.rs:~1442`), matching the Done text.
  4. Done step 2: "only the model edited" is not literal. List the real edits: Anthropic: reasoning knobs re-derived, `tool_use.signature` stripped; Chat: `thinking` removed, `reasoning_effort` re-derived, `stream_options.include_usage` forced; Gemini: `thinkingBudget` clamped (after 02-5); all adapters: operator `prompt_cache` hygiene when configured.
  5. Done step 3: update the disposition-table sentence to "keyed per client format and provider protocol" (after 02-11) and list the carried/mapped/rejected cells as decided in 02-6..02-10 (server tools: rejected by name cross-format).
  6. Done step 5: "same-format passthrough re-derives the fields through the same filter" -> true for Anthropic, Chat and Gemini (after 02-5); Responses same-format forwards `reasoning` untouched because the effort vocabulary is the provider's own.
  7. Done step 6: add that streaming Gemini calls capture signatures (after 02-14).
  8. Done step 7: "Clean client-supplied request ids before logging" is met by never adopting them; say so (already in Done; fix To-do wording only if desired). Add the is_error marker (02-1), `tool_choice` -> `toolConfig` (02-2), thinking-does-not-fit error (02-4), user_id/cache-marker exemptions (02-20) as one-line facts each. No design narratives (AGENTS.md).
  9. Last Done bullet ("Verified with the audit probes"): the probe prints, it does not assert; reword to "CI equivalents: `contract_replay.rs` cases for ...", citing test names.
- **Done when:** `grep -n "not replayable yet\|llm-proxy-provider\|adapter/" docs/completed/02-request-fidelity.md` returns nothing.
- **Risk / notes:** Do this last so it matches whichever options were taken for the DECIDE items.

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: RV-2, RV-3, RV-4, RV-5, RV-6, RV-9.
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings
- "G13: client request ids are not sanitized before logging" — not a defect: inbound `x-request-id` is never read or adopted (`routes/mod.rs` `inject_request_id` generates the id; `x-request-id` is in `NEVER_FORWARDED_HEADERS`; `contract_replay.rs:346` asserts the response id is not the client's). Only the plan wording is loose (02-21 item 8).
- "Row 33: probe prints `DROPPED` but asserts nothing" — the CI replay (`contract_replay.rs:632`, `:1095`) is the equivalent pass/fail check; no new script needed. Wording only (02-21 item 9).
- "Row 2: unscrubbed UUID `x-claude-code-session-id` in `captured/claude-code/headers.json:5`" — a session id, not a credential; the fixtures README documents curation. No action.
- "Responses same-format forwards `reasoning` raw (G8 second bullet)" — not a violation: `reasoning.effort` uses the provider's own vocabulary and the field is not in the plan's list; the upstream validates it. Noted in 02-21 item 6.
- "G17: remaining `deny_unknown_fields`" — all hits are outside the plan's request path: `core.rs` normalized types (decoded only in tests), `protocol/src/anthropic.rs` `ToolResult`/`ApiError` (no decode path), serialize-only output types in `client/openai_chat.rs`, `token_count.rs`, `error_response.rs`, admin API bodies in `llm-proxy-admin/src/keys.rs`, and `llm-proxy-bridge/src/lib.rs:178` `ExecuteOptions` (an SDK per-call options struct, not a wire request; leaving strict keeps typos in SDK calls loud). No action.
- "G16: same-format 'only the model edited' is not literal" — the code is intentional (reasoning re-derivation, signature stripping, forced `include_usage`, prompt hygiene); only the doc claim changes (02-21 item 4).
- "Row 28 `ProxyRequest` path `crates/llm-proxy-provider/src/transport.rs` missing" — the code is fine (`adapters/src/request.rs:25`); doc path fix only (02-21 item 2).
- "Row 11 `lossy` is per-adapter, not per-route" — code and Done text agree; only To-do 3 wording is stale (02-21 item 3).
