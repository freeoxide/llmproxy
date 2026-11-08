# Core Protocol and Lifecycle

This document collects the cross-provider rules that belong in the normalized
core, streaming pipeline, routing layer, or operational lifecycle. Each
section is dated; corrections and additions made by the 2026-11-08
research-audit pass are marked inline.

## Normalization boundaries

*Notes captured October 2026.*

Use one normalized core per endpoint family. Chat, embeddings, rerank, image
generation, audio, and files share infrastructure but not request or response
invariants. The translation path remains:

```text
client protocol -> endpoint core -> provider adapter
provider response/events -> endpoint core -> client protocol
```

Adapters should not call each other or depend on another provider's wire
types. Provider-only fields belong in explicit hints or opaque metadata, and
must not leak into strict providers after translation.

Translation is sometimes lossy. Keep that visible in logs or metadata rather
than silently pretending that images, reasoning blocks, citations, or terminal
states survived unchanged.

## Capability-driven sanitization

*Notes captured October 2026.*

OpenAI-compatible does not mean field-compatible. Providers commonly reject
otherwise valid fields such as `tool_choice`, `cache_control`,
`reasoning_effort`, `dimensions`, `temperature`, or structured-output schema
keys.

Prefer capability data over provider-name conditionals:

```text
supported parameters
stream terminator style
reasoning and structured-output support
tool schema restrictions
content modalities
usage and cost source
authentication and header policy
```

Differentiate behavior-preserving drops from semantic loss. Removing
`tool_choice = "auto"` for a provider where auto is already the default may be
safe. Removing a forced tool choice is not. Reject or warn when a requested
behavior cannot be honored.

Apply nested drop rules before provider transformation, then validate the
provider-shaped request. Do not forward client auth headers or arbitrary
provider fields without an allowlist.

## Streaming is a state machine

*Notes captured October 2026.*

A stream must track more than the latest chunk:

```text
message and content-block state
tool calls by stable index
reasoning versus visible text
latest cumulative usage
terminal status and finish reason
whether visible output has escaped
upstream message or response identity
```

Important rules:

- A finish reason may arrive before usage, and usage may arrive in its own
  final chunk. Do not finalize on the first finish marker alone.
- `completed`, `failed`, `incomplete`, and transport termination are distinct
  terminal states.
- Empty content chunks may still carry usage, a finish reason, metadata, or a
  protocol event. Filtering is protocol-specific.
- OpenAI `[DONE]`, Anthropic `message_stop`, Responses terminal events, and
  Gemini stream closure are different contracts.
- Responses-style streams are event streams. Preserve event ordering rather
  than treating every event as an OpenAI chat delta.
- When indices are missing, allocate stable fallback slots. Never let later
  chunks renumber an active tool call or content block.
- Preserve the original request context for logging and hooks even after a
  provider-shaped body has been built.

If an error occurs before output, return a normal HTTP error when possible. If
output has already escaped, emit a protocol-shaped in-band error or clean
terminal event and stop. Retrying after visible output can duplicate text or
corrupt tool-call state, so it should be disabled unless a protocol has an
explicit safe-resume mechanism.

Cached stream replay, polling, and reconstructed final responses are separate
execution modes. Ensure callbacks, usage, and cost are recorded once. A
materialized final response does not imply success; failed and incomplete
streams may still need a response object for accounting.

## Tool calls and structured output

*Notes captured October 2026; current-behaviour addition 2026-11-08.*

Tool calls require stateful assembly. Names and IDs often arrive before JSON
arguments, argument fragments may span arbitrary boundaries, and parallel
calls may use missing or repeated indices. Buffer by stable index and preserve
raw argument deltas until completion.

At protocol boundaries:

- Sanitize tool names to the target provider's character and length rules,
  resolve collisions, and keep a reverse map for responses.
- Repair truncated JSON only through an explicit, observable recovery path.
- Coerce or filter JSON Schema deliberately. Providers differ on `$ref`,
  `$defs`, `anyOf`, nullable types, defaults, titles, `required`, and whether
  schemas must be objects.
- Treat structured output as a provider capability. It may be a native schema,
  JSON mode, a synthetic forced tool call, or a prompt-level instruction —
  and the forced-tool-call route is model-gated at Anthropic: newer models
  reject that structured-output path with a 400 (added 2026-11-08 per
  research audit), so it must be a per-model capability, not a blanket
  fallback.
- Keep internal response-format tools separate from user tools so they do not
  leak into client-visible tool calls.
- Validate provider-specific `tool_choice` support instead of forwarding the
  OpenAI shape blindly.
- Preserve tool-result ordering and provider-required role alternation.

MCP, agent, and auto-execute tools are orchestration, not ordinary tool
translation. They can turn one client request into discovery, model, tool, and
follow-up phases. Such flows must inherit auth, budgets, region, user identity,
and cancellation from the parent request.

## Reasoning and content

*Notes captured October 2026.*

Reasoning can be stateful across turns. Some providers require signed thinking
blocks to be replayed, others reject invalid or incomplete blocks, and local
models may delimit reasoning with text markers such as `<think>`. Keep
reasoning separate from visible text in the core and preserve opaque signatures
when the provider requires them.

Multimodal routing should be based on content kind and MIME type, not only the
provider name. Normalize MIME aliases and strip parameters before choosing URL,
base64, inline-data, file-ID, image, document, or audio representations.

Provider metadata may contain response IDs, container IDs, encrypted content,
citations, cache details, or routing affinity. Preserve it in a bounded opaque
map when the core has no stable field, but do not pass it automatically to a
different provider.

## Usage and cost

*Notes captured October 2026; correction 2026-11-08.*

Never present estimated or synthetic usage as provider-reported truth. Track
provenance explicitly:

```text
provider-reported
locally estimated
translated then provider-counted
synthetic zero
unknown
```

Usage objects may contain null counts, separate cached-token fields, reasoning
tokens, or usage-only stream events. Normalize malformed values without losing
their source. The latest cumulative usage should replace earlier snapshots;
true deltas should be added only when the provider contract says they are
deltas.

Some providers require opt-in usage flags such as
`stream_options.include_usage`. OpenRouter's `usage.include` is deprecated
and has no effect — do not build on it (corrected 2026-11-08 per research
audit). Token-count endpoints may
also need their own schema bridge and provider headers. Local tokenization is a
fallback, not a native-provider count.

Cost needs fixed-point arithmetic plus pricing provenance and version. Record
currency, input/output/cache/reasoning components, and whether the value came
from provider metadata or local pricing. Unknown pricing should remain unknown.
Do not calculate the same request twice through a synthetic response and a
generic completion callback.

Rotated credentials, OAuth subscriptions, and learned quotas are routing state,
not merely strings. If added, track cooldowns, exhausted versus transient
limits, sticky sessions, and the credential that incurred the charge without
exposing secrets.

## Routing and affinity

*Notes captured October 2026.*

Provider route identity should be explicit and canonical. Model aliases select
an upstream model inside that provider; they should not silently select a
different provider.

Follow-up requests may need affinity to a provider, deployment, container, or
previous response. Prefer explicit metadata. If identity is encoded into an ID,
make the format versioned, authenticated where necessary, and reversible.

Fallback and retry must share one policy chain. Revalidate fallback targets
against client/provider allowlists and remove internal test or routing flags
before dispatch. Parse `Retry-After` and provider-specific quota-reset signals
into one internal representation.

Duplicate-request suppression is not caching. If implemented, scope it to
simultaneous identical submissions, use a short bounded window, and avoid
canceling legitimate repeated prompts.

## Cancellation and shutdown

*Notes captured October 2026.*

Client disconnect must cancel upstream streaming and release response bodies,
sockets, stream wrappers, and credential state. Cancellation should cover the
first-frame wait as well as the steady-state loop.

Background side effects need durability classes:

- stream cleanup is immediate and cancellation-aware;
- operational logging may be bounded and best-effort;
- usage and billing records require retry or persistence;
- mutable state should distinguish in-memory truth from durable writes.

Shutdown should move through explicit running, draining, flushing, and stopped
states. Bound every queue, retry loop, and flush deadline. A failed disk write
must remain pending rather than being reported as durable.

## Core implementation checklist

*Notes captured October 2026.*

- Endpoint-specific request, response, and event types.
- Stable content and tool-call indices.
- Explicit stop reasons, terminal states, usage provenance, and provider
  metadata.
- Stateful client encoders and provider decoders.
- Capability-based request sanitization.
- No retry after visible output without a safe-resume contract.
- Cancellation from client socket to upstream body.
- Golden fixtures for malformed, partial, usage-only, tool, reasoning, and
  terminal-event cases.
