# Lint backlog

Remaining `cargo clippy --workspace --all-targets --all-features` warnings after
the lint-infrastructure adoption described in `SETUP-RUST-LINTS.agent.md`.

Baseline when the Tier B config landed: 961 warnings. Now: 20.

These are deliberately **not** suppressed. `too_many_lines` and
`too_many_arguments` stay enabled at their real thresholds (100 lines, 6
arguments) so the gate keeps telling the truth about new code; the entries below
are the existing debt. Do not add `#[allow]`/`#[expect]` to clear them, and do
not raise the thresholds — split the functions.

## `clippy::too_many_lines` (threshold 100)

| Lines | Location | Function |
|---|---|---|
| 410 | `crates/llm-proxy-server/src/routes/core_pipeline.rs:1510` | `build_sse_output_stream` |
| 214 | `crates/llm-proxy-adapters/tests/fixture_tests.rs:962` | `run_stream_decode_fixture` |
| 211 | `crates/llm-proxy-adapters/src/openai_chat.rs:414` | `OpenAiChatAdapter::encode_request` |
| 200 | `crates/llm-proxy-server/src/routes/core_pipeline.rs:694` | — |
| 196 | `crates/llm-proxy-adapters/tests/fixture_tests.rs:708` | `run_decode_response_fixture` |
| 190 | `crates/llm-proxy-protocol/src/client/openai_chat.rs:94` | `decode_request` |
| 186 | `crates/llm-proxy-server/src/routes/core_pipeline.rs:997` | — |
| 171 | `crates/llm-proxy-protocol/src/client/openai_chat.rs:628` | `StreamEncoder::encode_event` |
| 152 | `crates/llm-proxy-adapters/src/responses.rs:79` | `ResponsesStreamDecoder::decode_frame` |
| 145 | `crates/llm-proxy-server/src/routes/token_count.rs:102` | — |
| 132 | `crates/llm-proxy-protocol/src/client/anthropic.rs:788` | `StreamEncoder::encode_event` |
| 129 | `crates/llm-proxy-adapters/src/responses.rs:537` | `ResponsesAdapter::encode_request` |
| 124 | `crates/llm-proxy-adapters/src/gemini.rs:363` | `GeminiAdapter::encode_request` |
| 113 | `crates/llm-proxy-protocol/src/client/openai_chat.rs:370` | `encode_response` |
| 105 | `crates/llm-proxy-adapters/tests/fixture_tests.rs:565` | `run_encode_request_fixture` |
| 105 | `crates/llm-proxy-server/tests/chat_completions.rs:958` | — |
| 101 | `crates/llm-proxy-server/tests/chat_completions.rs:2012` | — |

## `clippy::too_many_arguments` (threshold 6) — all 7/6

- `crates/llm-proxy-server/src/routes/core_pipeline.rs:694`
- `crates/llm-proxy-server/src/routes/core_pipeline.rs:997`
- `crates/llm-proxy-server/src/routes/token_count.rs:102`

All three also appear above. Grouping the request-scoped values that always
travel together into a small struct clears both lints at once. Do not merge
unrelated arguments into a tuple just to lower the count.

## How to approach these

Most are large `match` dispatchers over protocol event or content variants. The
decomposition that has worked so far is one named private helper per arm, or per
coherent group of arms, taking exactly the state that arm needs. Established
examples from the completed passes:

- `crates/llm-proxy-adapters/src/anthropic.rs` — `encode_system`,
  `encode_messages`, `encode_tools`, `encode_tool_choice`,
  `push_content_block_delta`, `tool_field_or_sentinel`
- `crates/llm-proxy-protocol/src/client/anthropic.rs` — `decode_tool_result_blocks`,
  `encode_content_start_block`, `encode_error_event`
- `crates/llm-proxy-core/src/provider_config.rs` — `validate_provider_config` split
  into one helper per validation rule

Two cautions:

- **Extract, do not rewrite.** These are protocol encoders and decoders. A
  change in event ordering, field precedence, or warn-and-skip behaviour is a
  real bug that the type system will not catch. The suite is the safety net —
  1,203 tests must still pass with identical counts.
- **`build_sse_output_stream` (410) is the delicate one.** It is an async SSE
  pipeline; `emit_finalization_events`, `emit_encoder_finish`, and
  `finalize_sse_framer` have already been pulled out of it. Reaching 100 may not
  be possible without restructuring how the stream terminates and how client
  disconnect propagates. Stopping at the safe point and leaving it listed here is
  a better outcome than a subtly broken stream.

If a function genuinely cannot get under 100 without producing worse code — a
flat `match` whose every arm is three lines, where extraction only adds
indirection — say so in the PR and leave it listed here. That is a real answer;
a suppression is not.
