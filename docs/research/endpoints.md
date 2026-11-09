# Extended Endpoint Families

Each section is dated; corrections made by the 2026-11-08 research-audit
pass are marked inline.

Additional endpoints should reuse routing, transport, auth, observability, and
cost infrastructure, but each needs its own normalized request, response, and
event contract. They should not be forced through the chat core.

## Embeddings

*Notes captured October 2026.*

- Dimensions are capabilities. Providers may rename `dimensions`, constrain
  allowed sizes, or ignore it; reject or warn when the returned vector size can
  differ from the request.
- Preserve input ordering and rewrite response indices when server-side
  batching splits a request.
- Accept provider-specific response envelopes only in the adapter, then expose
  one stable vector representation.
- Record token or billed-unit usage with provenance.
- Remove auth/region fields from model payloads before dispatch.

Representative transforms include `input` to `inputs`, `dimensions` to
`output_dimension`, provider knobs under `extra_body`, tensor request/response
bridges, model-prefix removal, and separate text/image embedding paths.

## Rerank

*Notes captured October 2026.*

A normalized rerank core needs a query, ordered documents, top-N selection,
scores, optional returned documents, and usage. Provider adapters may:

- rename `documents` to `texts` or object-based passages;
- translate `top_n` to `top_k`;
- use a different host family, as Bedrock does with agent-runtime;
- repeat the query per document;
- reconstruct document echoes and normalize scores;
- unwrap provider-specific error envelopes.

Rerank is not an embeddings sub-route even when providers share credentials.

## Realtime and WebSocket protocols

*Notes captured October 2026.*

Realtime is a bidirectional event protocol, not HTTP streaming with a different
content type. Track session configuration, conversation items, audio buffers,
tool calls, response phases, usage, and terminal/error events.

- GA and beta event vocabularies may require explicit normalization.
- Some providers require a bootstrap/client-secret call followed by a separate
  WebSocket URL.
- Preserve existing URL query parameters when injecting the model or API
  version.
- Unknown events should be logged and handled by policy, not blindly forwarded.
- Standalone usage events may need buffering until the next response.
- Audio sample rates, encodings, voice settings, and turn detection are
  provider capabilities.

HTTP-only Responses providers must remain on HTTP even if another provider
offers a native WebSocket surface.

## Transcription and text-to-speech

*Notes captured October 2026.*

Audio endpoints need a distinct binary/multipart boundary:

- Let the HTTP client generate multipart boundaries; do not retain a JSON
  `Content-Type` header.
- Normalize language, timestamp, response-format, voice, speed, sample-rate,
  and encoding fields by capability.
- Some services accept raw audio bytes, some multipart files, and others gRPC.
- Preserve segments, language, duration, and provider metadata without forcing
  them into plain transcript text.
- Cost may depend on duration, characters, or audio tokens rather than chat
  tokens.

Representative bridges include Deepgram query parameters, Riva gRPC,
OpenAI/Hosted-vLLM transcription paths, Azure Speech endpoint normalization,
Polly/ElevenLabs provider contracts, and long-running document/audio jobs.

## Image and video generation

*Notes captured October 2026.*

Model family can select a different endpoint and request schema. Normalize:

```text
prompt and negative prompt
size or aspect ratio
quality and output format
seed and model-specific controls
input image, mask, or multiple-image policy
synchronous result versus operation/job identity
usage and cost metadata
```

Do not silently discard extra input images or map unsupported sizes. Image edit,
variation, and generation are separate capabilities even when one provider
uses a shared route.

Video generation is usually long-running. Preserve operation identity,
polling state, cancellation, generated-file retrieval, duration, resolution,
and provider/model affinity. Encode proxy-managed IDs only through a versioned,
reversible scheme.

## Files, containers, and vector stores

*Notes captured October 2026; correction 2026-11-08.*

- Preserve query strings while appending resource IDs to configured base URLs.
- Treat resumable and multipart upload flows as explicit state machines.
- Keep file purpose, MIME type, ownership, expiry, and provider identity.
- Validate metadata schemas and limits before dispatch.
- Route encoded file, batch, container, and vector-store IDs back to their
  owning provider without exposing credentials.
- File search may be native, mapped to a provider tool, or emulated through a
  two-step search/model flow; expose which path ran.
- Container/code-interpreter creation can incur cost before model inference.

Anthropic Files/Skills, Gemini resumable uploads, OpenAI containers, and managed
vector stores use different beta headers and object lifecycles. They should not
share one untyped pass-through handler. Correction 2026-11-08 per research
audit: Anthropic Files no longer needs a beta header — only
`anthropic-version` (https://platform.claude.com/docs/en/api/files) — so
attribute beta-header handling per family rather than assuming Files carries
one.

## OCR, batch, and fine-tuning

*Notes captured October 2026.*

OCR can be synchronous, an upload-plus-job flow, or a bridge through a vision
chat model. Normalize page selection, input source, polling, page output,
annotations, and provider metadata.

Batch and fine-tuning APIs require durable job state. Normalize provider status
vocabularies, retain request counts and errors, and route job/file IDs with
provider affinity. Streaming JSONL results should be transformed incrementally
rather than loaded into memory as one response.

## Search bridges

*Notes captured October 2026.*

Search is structured retrieval, not assistant text. A core search result should
preserve:

```text
query and filters
result URL/title/snippet/content
rank or score
citations and annotations
provider metadata
search-query counts and billing provenance
```

Adapters may collapse query lists, translate domain/country/time filters, use
GET query strings or POST task arrays, require feature headers, and flatten
nested result envelopes. Keep those transforms provider-local.

When chat providers return citations or search results beside message content,
preserve them as structured metadata. Inferring annotations from `[1]` markers
is a lossy fallback and should be labeled as inferred.

Search-only interception and agentic search turn one model request into a
multi-stage workflow. They must inherit auth, budgets, user identity,
cancellation, and observability from the parent request.

## Endpoint implementation checklist

*Notes captured October 2026.*

- Dedicated core request/response/event types.
- Capability validation before dispatch.
- Correct JSON, binary, multipart, gRPC, SSE, or WebSocket transport.
- Provider-specific URL and authentication handling.
- Durable identity for jobs and uploaded resources.
- Usage/cost provenance appropriate to the modality.
- Cancellation and cleanup for uploads, streams, polling, and generated files.
- Fixtures for each supported provider envelope and failure mode.
