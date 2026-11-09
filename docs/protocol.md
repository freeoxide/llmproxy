# Protocol Architecture

The proxy translates every supported wire protocol through one normalized chat
core. It does not implement direct protocol pairs.

```text
client wire format -> CoreRequest -> provider wire format
provider response/events -> CoreResponse/CoreEvent -> client wire format
```

This keeps protocol translation separate from provider selection, credentials,
catalogs, and transport.

## Current protocol support

| Protocol family | Client input/output | Upstream provider |
|---|---:|---:|
| OpenAI Chat Completions | yes | yes |
| Anthropic Messages | yes | yes |
| OpenAI Responses | yes | yes |
| Gemini GenerateContent | yes | yes |
| Ollama (`/api/chat`, `/api/generate`, `/api/tags`) | yes | no |

The client routes are:

```text
POST /providers/{provider}/v1/chat/completions
POST /providers/{provider}/v1/messages
POST /providers/{provider}/v1/messages/count_tokens
POST /providers/{provider}/v1/responses
POST /providers/{provider}/v1beta/models/{model}:generateContent
POST /providers/{provider}/v1beta/models/{model}:streamGenerateContent?alt=sse
POST /providers/{provider}/api/chat
POST /providers/{provider}/api/generate
GET  /providers/{provider}/v1/models
GET  /providers/{provider}/api/tags
```

The Responses and Gemini-native routes resolve through the provider's
`routes.responses` and `routes.generate_content` adapter entries; the Ollama
routes resolve through `routes.chat_completions`. Ollama is a client dialect
only — there is no Ollama upstream adapter; the same admission, routing and
accounting pipeline serves every dialect.

## Boundaries

### Client adapters

Client adapters decode a client request into `CoreRequest` and encode
`CoreResponse` or `CoreEvent` values into the client's JSON or SSE shape.

They own client validation and client-facing protocol errors. They do not know
which provider will run the request.

### Router and server

The server selects the provider named in the URL, resolves the configured route
and model alias, invokes the selected provider adapter, and manages HTTP/SSE,
timeouts, cancellation, metrics, and events.

It does not translate provider fields itself.

### Provider adapters

Provider adapters encode `CoreRequest` into one upstream protocol and decode
provider JSON or SSE into `CoreResponse` or `CoreEvent` values.

Each adapter knows only its provider protocol and the core. It must not import
or call a client adapter or another provider adapter.

## Normalized request

`CoreRequest` models client intent rather than wire nesting. It contains:

- `ModelRef`, preserving both the client-requested model and an optional
  resolved upstream model;
- separated system instructions and conversation messages;
- block-based content;
- tool definitions and optional tool choice;
- sampling and reasoning options;
- streaming intent and caller metadata;
- bounded, opaque provider hints for behavior that has no stable core field.

Client adapters normalize equivalent wire shapes. For example, OpenAI function
arguments nested under a tool call and Anthropic `tool_use` input both become a
core `ToolUse` block.

The current chat core supports these content kinds:

```text
Text
Image
Document
Audio
Video
ToolUse
ToolResult
Thinking
RedactedThinking
Refusal
Opaque
```

Media source values and provider hints may remain opaque when their stable
meaning is not portable. They must be redacted in diagnostics and interpreted
only by adapters that understand them.

`Opaque` carries a client block the core does not model (such as a replayed
Anthropic `server_tool_use`) as its raw wire JSON. Only same-format passthrough
can forward it; a cross-format request carrying one is rejected by name.

## Normalized response

`CoreResponse` contains:

- an optional upstream response ID;
- the requested and resolved model identity;
- ordered content blocks;
- a canonical stop reason and optional stop sequence;
- usage with explicit provenance;
- bounded provider metadata;
- optional fixed-point cost computed from configured pricing.

Known stop reasons are end turn, maximum tokens, tool use, stop sequence,
refusal, and error. Unknown provider values remain explicit instead of being
silently mapped to a successful stop.

Provider metadata is for useful values that have no canonical field. It is not
a license to pass arbitrary fields from one protocol into another.

## Streaming events

Streaming is normalized as a state machine, not concatenated into a fake
non-streaming response.

```text
MessageStart
ContentStart
TextDelta
ThinkingDelta
ToolCallStart
ToolCallDelta
ToolCallStop
UsageDelta
MessageStop
Error
Ping
```

Provider decoders may keep local state for partial SSE frames, content indices,
tool-call argument fragments, reasoning blocks, usage, and terminal events.
Client encoders keep the state needed to produce a coherent client stream.

Important invariants:

- tool calls use stable content indices;
- partial JSON arguments remain string deltas until the call is complete;
- usage-only and finish-only events are not discarded as empty;
- provider message IDs are preserved when available;
- usage is not labeled provider-reported unless it came from the provider;
- an upstream disconnect is finalized according to the client protocol;
- an error after visible output is emitted in-band because the HTTP status is
  already committed;
- no event is duplicated when the first provider event is buffered to discover
  an upstream ID.

V1 streams incrementally carry text, thinking, and tool calls. Other content
kinds are buffered for non-streaming output or rejected when the target client
cannot represent them.

## Lossy translation

Protocols do not have identical capabilities. When a value cannot be mapped:

1. add a core field if the concept is broadly portable;
2. preserve it as bounded metadata when it is useful but provider-specific;
3. drop it with an observable warning when omission is safe;
4. reject the request or response when omission would change required
   behavior.

Never solve a mismatch by special-casing Provider A inside Client Protocol B's
adapter. The provider adapter or a capability policy at the provider edge owns
that behavior.

Examples of capability-sensitive translation include tool schema restrictions,
tool choice, cache control, reasoning options, structured output, multimodal
content, stream terminators, and usage fields.

## Adding support

### New client protocol

1. Add its wire types and client adapter.
2. Decode requests into the existing endpoint core.
3. Encode core responses and events into the client protocol.
4. Add the client route and protocol-aware error mapping.
5. Add request, response, and streaming fixtures.

Provider adapters should not change.

### New upstream protocol

1. Add wire types and a provider adapter.
2. Encode `CoreRequest` into the provider request.
3. Decode provider responses and streams into core values.
4. Register the protocol in provider configuration.
5. Add provider fixtures and transport tests.

Client adapters should not change.

### New endpoint family

Do not stretch the chat core into a universal DTO. Embeddings, rerank, images,
audio, realtime, files, and search need endpoint-specific cores while sharing
routing, auth, transport, observability, and cost infrastructure.

## Fixture requirements

Each adapter should cover, where applicable:

- plain text and system instructions;
- tool definitions, calls, results, and tool choice;
- reasoning or thinking;
- stop-reason and usage mapping;
- cache control and provider hints;
- streaming text, tools, usage, ping, malformed events, and errors;
- unsupported or lossy fields;
- provider-specific malformed responses.

Snapshot normalized requests, responses, and event sequences. Translation bugs
often produce valid JSON with the wrong meaning, so compilation alone is not a
sufficient protocol test.
