# Contract fixtures (plan 02, step 1)

Two fixture families, both replayed against a fake upstream in CI
(`tests/contract_replay.rs`):

1. **`captured/` — live client traffic**, captured 2026-11-05. The real
   clients ran against a loopback capture upstream that recorded the exact
   request bytes and headers and replied with canned responses (no upstream
   API keys exist in this environment; the step's own wording is "replay it
   against a fake upstream"). Credentials were fake placeholders at capture
   time and stay fake in the fixtures.
2. Synthesized fixtures in the per-client directories below, written from the
   official API/SDK documentation before the live clients were available.
   They cover field sets the short capture scripts did not send.

## Live captures (`captured/`)

| Fixture | Client (version at capture) | Notes |
|---|---|---|
| `captured/claude-code/session-request.json` + `headers.json` | Claude Code CLI 2.1.251 (`claude -p` with a Bash-tool turn; `ANTHROPIC_BASE_URL` pointed at the capture upstream) | Streaming session: user (5 text blocks incl. `<system-reminder>`s) → assistant `tool_use` → user `tool_result` with `cache_control`. Real wire details kept: `model: claude-sonnet-4-5` (undated alias), `max_tokens: 32000`, `thinking {budget_tokens, type, display: "omitted"}`, top-level `context_management`, 5-beta `anthropic-beta` list, `x-api-key` auth (no `authorization`), `x-app`, `x-stainless-*` telemetry. Curation: 27k-char system prompt and two long `<system-reminder>` texts truncated with in-band markers; tool list cut from 25 schemas to `Bash` + `Read` (kept verbatim); `device_id`/`session_id` inside the JSON-encoded `metadata.user_id` replaced with placeholders (the string-encoded-JSON shape is real); a token counter in the tool_result reminder stabilized. Everything else is byte-as-captured. |
| `captured/anthropic-sdk/tool-replay-request.json`, `stream-request.json`, + `headers.json` | `@anthropic-ai/sdk` (JS) 1.5.0 (`messages.create`, `baseURL` pointed at the capture upstream) | Tool conversation replay (text+image user turn, `tool_use`, `tool_result` with `is_error`), `system` with `cache_control {type, ttl}`, `tool_choice.disable_parallel_tool_use`, `top_k`, `stop_sequences`; second capture is the streaming shape. Real header set: `authorization: Bearer`, `anthropic-version: 2026-10-21`, `x-stainless-*`. Bodies stored verbatim (no curation needed). |
| `captured/openai-sdk/chat-request.json`, `stream-request.json`, + `headers.json` | `openai` (JS SDK) 7.51.0 (`chat.completions.create`, `baseURL` pointed at the capture upstream) | Full knob set incl. `response_format`, `parallel_tool_calls: false`, `seed`, `logit_bias`, penalties, `service_tier`, `store`, `stop`, `user`, `max_completion_tokens`, multimodal `image_url` with `detail`, string-encoded `tool_calls.arguments`; second capture is `stream` + `stream_options.include_usage`. Bodies stored verbatim. |

## Synthesized fixtures

| Fixture | Client | Shape provenance |
|---|---|---|
| `claude-code/session-request.json` | Claude Code (Anthropic Messages dialect) | Messages API reference <https://docs.anthropic.com/en/api/messages> (model, max_tokens, system array with `cache_control` incl. `ttl`, messages with text/tool_use/tool_result blocks, tools with `input_schema`, tool_choice with `disable_parallel_tool_use`, temperature, top_p, top_k, stop_sequences, metadata.user_id, thinking `{type, budget_tokens}`, stream); Claude Code system-prompt caching pattern per <https://code.claude.com/docs> (system array of text blocks, one carrying `cache_control`). |
| `anthropic-sdk/tool-replay-request.json` | Anthropic SDK (Python/Typecript) | Messages API request schema as above; assistant tool_use replay + user tool_result (with `is_error`) per the tool-use guide <https://docs.anthropic.com/en/docs/agents-and-tools/tool-use>. |
| `anthropic-sdk/media-request.json` | Anthropic SDK (Messages dialect) | Image blocks with `url` and Files API `file` sources per the vision guide <https://platform.claude.com/docs/en/build-with-claude/vision>; `document` blocks with `url` and `base64` (`application/pdf`, with `cache_control`) sources per the PDF support guide <https://platform.claude.com/docs/en/build-with-claude/pdf-support>. The URLs, file id and PDF bytes are placeholders. |
| `anthropic-sdk/web-search-replay-request.json` | Anthropic SDK (Messages dialect), server tools | Turn two of a web search conversation per the web search tool guide <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>: the `web_search_20250305` tool definition, and the assistant turn replayed as received (`server_tool_use`, `web_search_tool_result` with `encrypted_content`, cited text with `web_search_result_location` citations carrying `encrypted_index`), which the guide requires be sent back unmodified. The ids and encrypted values are placeholders. |
| `openai-sdk/chat-request.json` | OpenAI SDK (Chat Completions) | Chat Completions API reference <https://platform.openai.com/docs/api-reference/chat> — the full documented knob set: `response_format`, `parallel_tool_calls`, `seed`, `n`, `logprobs`, `logit_bias`, `frequency_penalty`, `presence_penalty`, `service_tier`, `prompt_cache_key`, `store`, `reasoning_effort`, `stream_options`, `stop`, `user`, plus multimodal `image_url` content parts per <https://platform.openai.com/docs/guides/images-vision>. |
| `openai-sdk/stream-request.json` | OpenAI SDK, streaming | Same reference; `stream: true` with `stream_options.include_usage` per the streaming guide. |

Codex CLI traffic is **not** captured or replayable here: Codex speaks the
OpenAI Responses API, which the proxy does not yet expose as a client route
(see plan 08), and the Codex CLI is not installed in this environment. It is
listed as a partial step in the plan-02 report.

## Replay rule

Every fixture — synthesized and captured — replays through the real router
(`build_router`) against a loopback fake upstream. The request reaching the
fake upstream must equal the fixture body exactly, apart from the listed
edits:

- model aliasing (`model` may be rewritten),
- reasoning translation (the `thinking` knobs `type`/`budget_tokens` and
  `reasoning_effort` are translated and clamped, never forwarded raw, per
  BUG-20; auxiliary `thinking` subfields such as Claude Code's `display` ride
  along),
- `stream_options.include_usage` is forced on for streaming chat requests so
  the ledger records real usage,
- headers: only the per-provider allowlist is forwarded; client auth,
  `Origin`, SDK telemetry (`x-stainless-*`) and browser headers never are.
  The captured `headers.json` sets are applied to the replayed requests so
  the allowlist is exercised against what the real clients send.
