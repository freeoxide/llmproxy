# 01. Bug fixes

- **Status:** In progress
- **Depends on:** Nothing
- **Audit rows:** 1, 2, 3, 4, 5, 6, 7, 10, 11, 13
- **Code-audit bugs:** BUG-1, BUG-2, BUG-3, BUG-4, BUG-5, BUG-6, BUG-7, BUG-8,
  BUG-9, BUG-10, BUG-11, BUG-12, BUG-15, BUG-16, BUG-17, BUG-21, BUG-22, BUG-26
- **Backlog:** 3, 5, 7, 28
- **Conflicts:** None

Make what already exists correct and truthful before anything is built on it.

## Done

- **Server exit (row 1, BUG-1):** the shutdown deadline now starts at the shutdown
  signal, not at startup.
- **Prompt deletion (row 2, BUG-2):** `ttl` is carried on cache markers, and
  content the proxy can't parse gets a 400 instead of disappearing.
- **Redirect leak (row 3, BUG-8):** upstream redirects are followed only within
  the same origin.

## To do

1. **Client key sent upstream (BUG-9).** With passthrough and client keys both on,
   the proxy's client key goes to the provider. Never forward a token that
   authenticated the caller. Refuse this combination at config validation until a
   separate upstream-key header exists. See `extract_inbound_auth` in
   `crates/llm-proxy-server/src/routes/core_pipeline.rs`.
2. **Anthropic stream usage and stop reason (row 4, BUG-3).** The documented
   `message_delta` usage has only `output_tokens`, but `Usage` in
   `crates/llm-proxy-protocol/src/anthropic.rs` requires `input_tokens`, so the
   event is skipped. Usage reads 0/0 and `max_tokens` becomes `end_turn`. Read
   `message_start` usage too. The fixture
   `crates/llm-proxy-provider/tests/fixtures/anthropic/stream_usage/core-events.json`
   locks in the wrong result and must change.
3. **Usage on chat streams (row 5, BUG-7, backlog 7).** Always request
   `include_usage` from providers that accept it, and remove the usage chunk for
   clients that didn't ask for it. Use `UsageProvenance` for estimates rather than
   a new flag. The `Accept-Encoding` part of backlog 7 isn't needed, since the
   proxy doesn't send that header.
4. **Client-visible usage (row 6, BUG-6).** When usage arrives after the finish
   event, clients see zeros while the ledger is correct. Fix the encoders in
   `crates/llm-proxy-protocol/src/client/openai_chat.rs` and
   `crates/llm-proxy-protocol/src/client/anthropic.rs`.
5. **Cut-off streams (row 7, BUG-4, backlog 28).** Keep valid stream framing, as
   `docs/protocol.md` and `crates/llm-proxy-server/tests/core_pipeline.rs` require,
   but mark the failure for both the client and the ledger. Decode chat
   `{"error": ...}` frames instead of skipping them.
6. **Cache markers (row 10, row 11, BUG-5).** `encode_content_blocks` in
   `crates/llm-proxy-provider/src/adapter/anthropic.rs` drops message-level
   markers, and `CoreContent::ToolResult` has no field for one. Carry both,
   including `ttl`. Add `ttl` to the OpenAI-format `CacheControl` as well.
7. **Usage mapping (row 13, BUG-11, BUG-12, backlog 3).** In
   `build_usage_from_openai`, count DeepSeek-style miss tokens as plain input
   rather than cache creation. Parse `prompt_tokens_details.cached_tokens` and
   Anthropic's 5m and 1h splits. Label absent usage as missing, not as
   provider-reported zeros.
8. **Ledger accuracy (BUG-10, BUG-21, BUG-26, backlog 5).** Keep `model_upstream`
   on streaming rows. Write a settled row when a client disconnects before the
   first event. Count and log events lost to a failed batch or a full channel.
   Disconnect propagation itself already works.
9. **Upstream codecs (BUG-15, BUG-16, BUG-17, BUG-22).** Fix the Responses and
   Gemini tool-call shapes and Gemini's streaming URL, encode `tool_choice: "auto"`
   as a string, and stop sending `"role": ""` on deltas. These came from reading
   code and API docs, so start each with a failing test against the documented
   shape.

## Done when

- [`run.sh`](../research/audit-2026-11-05/evidence/probes/run.sh) shows correct
  results for rows 4 to 7, 10, 11 and 13.
- Each fix has a regression test that fails without it.
