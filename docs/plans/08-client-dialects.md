# 08. Client dialects

- **Status:** Not started
- **Depends on:** 03
- **Audit rows:** None
- **Code-audit bugs:** None
- **Backlog:** 9, 19, 36
- **Conflicts:** None

Accept more client formats. Each one is a codec in `llm-proxy-protocol` plus a route
through the same engine, so it gets limits, routing and accounting without new
pipeline code.

## To do

1. **OpenAI Responses (backlog 9).** Serve `/providers/{provider}/v1/responses` with
   streaming event synthesis, exact usage fields and Codex-compatible tool-call
   ordering. Comes first because Codex-class clients can't be pointed elsewhere.
2. **Gemini native (backlog 19).** Serve `generateContent` and `streamGenerateContent`
   under `/providers/{provider}/v1beta/models/{model}`.
3. **Ollama native (backlog 36).** Serve `/api/chat`, `/api/generate` and `/api/tags`
   as a provider-scoped route, reusing the same admission path.

## Done when

- Captured Codex traffic replays through the contract tests from plan 02.
- Each dialect passes the shared streaming, tool-call, usage and error tests.
- A dialect can be used from the SDK without the server crate.
