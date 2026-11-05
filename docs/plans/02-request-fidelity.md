# 02. Request fidelity

- **Status:** Not started
- **Depends on:** 01
- **Audit rows:** 8, 9, 12
- **Code-audit bugs:** BUG-18, BUG-19, BUG-20
- **Backlog:** 10, 13, 16
- **Conflicts:** 10

Real clients must work through the proxy unchanged. Today it drops 11 of 17
tested OpenAI fields with a 200 and rejects common Anthropic fields with a 400.

## To do

1. **Contract tests first.** Capture traffic from Claude Code, Codex, the OpenAI
   SDK and the Anthropic SDK. Remove secrets, store it as fixtures and replay it
   against a fake upstream in CI.
2. **Same-format passthrough (conflict 10).** When client and provider use the same format, keep
   the raw body and apply only the edits the proxy needs, such as the model name.
   Unknown fields survive. Remove `deny_unknown_fields` from the request path in
   `MessageRequest`, `Tool` and `SystemContentBlock`. Read usage from the response
   without rewriting it.
3. **Cross-format field policy.** Keep one table per format pair in which each
   field is carried, mapped or rejected, and reject unknown fields by naming them.
   A per-route `lossy` opt-in turns a rejection into a drop with a logged warning.
4. **Missing fields.** Carry `response_format`, `parallel_tool_calls`, `seed`,
   `image_url`, `stop_sequences`, `top_k`, `output_config` and server tools wherever
   the provider supports them.
5. **Reasoning translation (BUG-20, backlog 13).** Convert between `reasoning_effort`,
   `thinking.budget_tokens` and Gemini's `thinkingBudget` with per-model limits, at
   the provider edge rather than in `llm-proxy-protocol`. Stop forwarding these
   fields unfiltered.
6. **Tool-state replay (backlog 16).** Preserve Gemini thought signatures and
   OpenAI `reasoning_content` across turns.
7. **Header allowlist (row 12, backlog 10).** Add a per-provider list of inbound
   headers to forward, with `anthropic-beta` and session headers on by default for
   Anthropic. Client auth, `Origin` and browser headers are never forwarded. Clean
   client-supplied request ids before logging them. `ProxyRequest` in
   `crates/llm-proxy-provider/src/transport.rs` has no inbound headers today.

## Done when

- Every captured client conversation replays with the same request reaching the
  upstream, apart from the listed edits.
- A cross-format request either carries each field or fails with an error naming it.
- A request can enable an Anthropic beta feature through the proxy.
