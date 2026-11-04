# Niche repository survey — third research wave (November 2026)

Method: a third research wave, run in November 2026 after the two October
waves recorded in
[`competitive-landscape-compendium.md`](competitive-landscape-compendium.md).
Niche hunts across the Rust/Go/TypeScript/Python long tail — agent-facing
local proxies, semantic and prompt caches, MCP gateways, guardrail/DLP
toolkits, queue dispatchers and load balancers for local inference,
subscription-to-API adapters, research routers, local-inference front doors,
and curated meta-lists — produced a deduplicated find list of **121 candidate
repositories**; the **23 below were researched in full** (identity, language,
form factor, maintenance status, capabilities with primary-source evidence
URLs, and the absences the research verified). All findings and URLs come
from the wave-3 research runs; nothing was built or run.

Sections are modular: each is self-contained and liftable on its own. Gap
numbers reference the register in the compendium (#1–#20); capabilities that
fall outside that register are tagged `new — <name>` and summarized as
candidate gap rows at the end of this intro; `context` marks informative but
unranked material (comparison axes the compendium deliberately leaves
unranked, already-scoped future work from `docs/research/endpoints.md`, or
parity with what llm-proxy already ships).

Candidate new gaps this wave surfaces (each backed by at least one niche
repo): **timeout hierarchies** (phase-shaped TTFB/idle budgets),
**request-payload cost transforms** (compaction, tool-output compression,
history hygiene), **shadow-eval quality guards** for such transforms,
**OAuth credential lifecycle** for non-cloud upstreams, **upstream
cancellation propagation** on client disconnect, **engine-state-aware
routing**, **admission queueing** with priority/fair-share, **caller
SSO/RBAC**, **externally synced price catalogs**, and **hedged request
races**.

Tagging policy: #6 tags rows that serve the OpenAI Responses API on the
client side (the register's definition); multi-protocol or endpoint breadth
without a Responses endpoint is `context`, as is breadth already scoped in
`docs/research/endpoints.md`. #19 (web admin UI) tags browser-served
consoles only — terminal TUIs and HTTP admin APIs are tagged `context` even
when they are rich admin surfaces.

Maintenance statistics (stars, commits, release dates, activity verdicts)
are the wave-3 runs' reads of each repository's GitHub page, releases page,
or API (and clone refs where pinned), captured November 2026; each section
closes its Maintenance paragraph with a **Sources** line linking them.

## Sections

Switchyard (NVIDIA-NeMo) · xLB (EeroEternal) · ostk-cache (os-tack) ·
claude-code-proxy (raine) · mistral.rs · SMG · Antirion · Plano · AISIX ·
mcp-gateway (MikkoParkkola) · ThinkWatch · preflight (ghuntley) · tokdiet ·
CLIProxyAPI · sub2api · Claude Code Router · Otari · done-hub · yolorouter ·
completion-to-response · GPTCache · PromptCache (messkan) · ollamaMQ

## NVIDIA-NeMo/Switchyard

**Identity.** Apache-2.0 model-routing toolkit from NVIDIA NeMo, successor to
NVIDIA-AI-Blueprints/llm-router. Rust Cargo workspace (edition 2024,
`rust-version` 1.96.1): embeddable routing library `libsy` (beta), optional
HTTP LLM client with protocol translation (alpha), a runner for external
runtimes (NeMo Relay), and a standalone OpenAI/Anthropic/Responses-compatible
server explicitly labeled demo-stability ("not a supported production path");
Python, C-ABI, and Node/TypeScript bindings plus a PyPI package.

**Maintenance.** Very active — last commit 2026-11-03, ~100 commits since
2026-10-01, last push 2026-11-04; v0.3.0 stable (2026-09-22), v0.4.0a1
pre-release, v0.4.0 targeted late November 2026 (roadmap issue #1026); 3,483
stars / 354 forks. Source claims verified by the research run against a
shallow clone of main @ f60a7a2.

**Sources.** Stats and activity: https://github.com/NVIDIA-NeMo/Switchyard
(repo page; commit and release history); capability code citations verified
against the clone at
https://github.com/NVIDIA-NeMo/Switchyard/tree/f60a7a2291ea1f8068f769fb920314c3280fb168;
v0.4.0 roadmap: https://github.com/NVIDIA-NeMo/Switchyard/issues/1026.

| Capability | Evidence | Gap |
|---|---|---|
| Ordered per-candidate fallback: each candidate exhausts its retry budget (Retry-After honored, backoff 250 ms→2 s) before a rewritten fallback request advances | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/libsy-llm-client/src/run.rs | #1 |
| Per-backend failure cooldown: `failure_cooldown_ms` (default 5000) parks an atomic unavailable-until timestamp after transient failures; terminal cooldown surfaces as 503 | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/reference/toml_schema.md | #1 |
| Rate-limit-aware fallback policy: `fallback_on_rate_limit` decides whether an exhausted 429 falls through or returns verbatim; forwarded credentials retry without poisoning shared cooldown | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/libsy-llm-client/src/run.rs | #1 |
| Whole-attempt-chain deadline: one `timeout_ms` covering every attempt, retry delay, and complete stream reads | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/libsy-llm-client/src/backend.rs | new — timeout hierarchy |
| Native triple-protocol server: `/v1/chat/completions`, `/v1/messages`, `/v1/responses` plus `/v1/decision`, count_tokens variants, and `/v1/responses/compact` | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/switchyard-server/src/lib.rs | #6 |
| Agent-aware routing suite: composite judge + between-turn stage router, LLM classifier with message-hash fallback, plan-execute, advisor gates, prefill router | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/routing_algorithms/overview.md | #13 |
| Prometheus `/metrics` plus OTel spans under GenAI semantic conventions (cache-read/creation and reasoning tokens per call) | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/switchyard-server/src/metrics.rs | #11, #12 |
| Append-only JSONL routing log: durable per-request routing records (chosen model, fallbacks, usage, cost) queryable via `/v1/stats` | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/switchyard-server/src/routing_log.rs | #2 |
| Credential redaction middleware scrubbing provider keys from response headers and decompressed bodies | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/switchyard-server/src/redaction.rs | #7 |
| Caller-credential forwarding (use your Claude login upstream) incl. a Claude Code plugin | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/examples/claude/README.md | context (parity: llm-proxy ships opt-in passthrough BYOK — `crates/llm-proxy-core/src/provider_config.rs:378-385`, `crates/llm-proxy-server/src/routes/core_pipeline.rs:727-730`) |
| Responses continuation state and `responses_reasoning` replay policy; context compaction operation | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/docs/reference/toml_schema.md | #6, new — payload cost transforms |
| Theoretical KV-cache hit-rate tracking (prefix-fingerprint match rate as a routing-quality signal) | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/crates/switchyard-server/src/stats/cache_eligibility.rs | new — engine-state-aware routing |
| Unreleased main: speculative parallel model/decision calls with loser cancellation, host aborts, serializable routing state | https://github.com/NVIDIA-NeMo/Switchyard/blob/main/CHANGELOG.md | context (announced) |

**Absent (verified).** No client-facing auth/key store, no per-client rate
limiting or budgets, no response caching (eval-only cache estimation), no
content guardrails, no MCP, no queryable durable store beyond JSONL, no
middleware suite, no per-provider egress forward proxy; the standalone server
is demo-stability.

## xLB (EeroEternal)

**Identity.** Single-crate Rust (edition 2021, tokio + httparse, ~11 source
files) streaming-first thin-L7/L4 load balancer positioned as "the HAProxy
role in the AI stack" in front of gateways like LiteLLM, New API, AISIX; it
forwards bytes, not tokens, pooling backend connections across long-lived
streaming POSTs. One static binary driven by TOML; musl/darwin tarballs and
GHCR/Docker Hub images. MIT.

**Maintenance.** Experimentally young and currently dormant — created
2026-10-14, 38 commits and 9 releases (v0.0.1–v0.0.9) compressed into
Oct 14–19 2026, then silent; last push 2026-10-19 (~2.5 weeks before
2026-11-04); 173 stars, single maintainer, 0 open issues.

**Sources.** Stats and activity: https://github.com/EeroEternal/xlb (repo
page) and https://github.com/EeroEternal/xlb/releases (v0.0.1–v0.0.9,
2026-10-14/19).

| Capability | Evidence | Gap |
|---|---|---|
| Phase-shaped timeout hierarchy: `connect` (5 s), optional `first_byte` TTFB budget, `idle` per-direction in-stream silence, `total` ceiling, `fin` half-close grace; cross-field validation | https://github.com/EeroEternal/xlb/blob/main/docs/architecture.md | new — timeout hierarchy |
| Wall-clock retry budget anchored at request arrival, jointly bounding connect retries, stale-socket failovers, and pre-first-byte reroutes; exhaustion rejects with a reason metric | https://github.com/EeroEternal/xlb/blob/main/src/config.rs | #1 |
| Pre-execution-refusal vs in-doubt error split: non-idempotent POST/PUT/PATCH reroute only on unambiguous pre-execution statuses; `respect_idempotency_key` restores full reroute | https://github.com/EeroEternal/xlb/blob/main/docs/changelog.md | #1 |
| Pre-first-byte HTTP status rerouting with buffered request-body replay (≤64 MiB); hard invariant: never retried after the first response byte | https://github.com/EeroEternal/xlb/blob/main/README.md | #1 |
| Thin-L7 HTTP/1.1 framing with cross-client POST connection pooling: LIFO idle pools, non-blocking liveness probes, keep-alive rewrite, L4 fallback for h2/Upgrade/binary | https://github.com/EeroEternal/xlb/blob/main/docs/architecture.md | context |
| Health checking with hysteresis thresholds, authenticated probes (CR/LF-validated headers), pool purge on unhealthy, passive dial-failure ejection with cooldown | https://github.com/EeroEternal/xlb/blob/main/docs/gateways.md | #1 |
| `xlb_ttft_delta_ms` — histogram of proxy-added first-byte latency — plus 16 further Prometheus metrics on a dedicated listener | https://github.com/EeroEternal/xlb/blob/main/docs/observability.md | #12, #17 |
| Per-backend concurrency caps with RAII permits and capacity-aware leastconn/roundrobin preference; 503 `queue_full` on slot-acquire timeout | https://github.com/EeroEternal/xlb/blob/main/config.toml | #3, new — admission queueing (queue_full backpressure) |
| Client-abort propagation: client EOF/RST raced against backend reads so disconnect immediately closes the upstream socket; TCP keepalive on both legs | https://github.com/EeroEternal/xlb/blob/main/docs/changelog.md | new — upstream cancellation |
| Sanitized multi-attempt trace correlation: secret-redacting trace-id derivation, logged with attempt counters on every reroute | https://github.com/EeroEternal/xlb/blob/main/src/proxy/http1.rs | context |
| Integration suite (~87 KB) pinning no-retry-after-first-byte, phase timeouts, idempotency split, retry ceiling, pooled-death failover — a ready-made spec for retry-contract tests | https://github.com/EeroEternal/xlb/blob/main/tests/integration.rs | context |

**Absent (verified, mostly stated non-goals).** No client auth, no
quotas/budgets/cost tracking, no admin API or hot reload, no LLM protocol
awareness (bodies opaque), no per-client rate limiting, no dedup/caching/
guardrails/token counting/MCP; HTTP/2 and WebSocket clients get raw L4
fallback only.

## ostk-cache (os-tack)

**Identity.** Rust local proxy (single `ostk-cache` server binary plus
`ostk-cache-hooks` installer and `ostk-cache-stats` reporter over a
tokio-free core crate) from ostk.ai that sits between agent harnesses
(Claude Code, Codex, Cursor) and Anthropic/OpenAI, rewriting request bodies
so stable context sits at byte-stable boundaries that maximize provider
prompt-cache hits; SSE/multipart forwarded verbatim. Dual MIT/Apache-2.0.

**Maintenance.** Effectively dormant — 104 commits all by one author, last
commit 2026-06-12 (the v0.4.0 bump), no activity in the ~5 months to
2026-11-04; 10 stars. Not buildable from the public repo alone: it depends
on three private crates from sibling repo os-tack/haystack.

**Sources.** Stats and activity: https://github.com/os-tack/ostk-cache
(repo page; 104 commits by one author) and
https://github.com/os-tack/ostk-cache/releases (v0.1.0 2026-05-06 through
v0.4.0 2026-06-12).

| Capability | Evidence | Gap |
|---|---|---|
| Prompt-cache hit-rate engineering by request mutation: single 1 h `cache_control` collapse, HUD line, proxy-owned breakpoint placement | https://github.com/os-tack/ostk-cache/blob/main/README.md | #14 |
| Volatile billing-token stripper: schema-free recursive removal of Claude Code's per-turn `cch=<hex>;` correlator that busts ~1 MB cached prefixes | https://github.com/os-tack/ostk-cache/blob/main/src/billing_strip.rs | new — payload cost transforms |
| History hygiene: `<system-reminder>` stripping from past turns with current-turn protection and tool_use/tool_result pairing preserved | https://github.com/os-tack/ostk-cache/blob/main/src/system_reminder_strip.rs | new — payload cost transforms |
| WARM/DEAD cache-write policy state machine with TTL-tier forecasting (median of trailing 5 inter-request gaps; 5 m/1 h write premiums) | https://github.com/os-tack/ostk-cache/blob/main/src/write_policy.rs | #14 |
| Byte-stable synthetic history projection (`rebuild`) with WARM freeze — re-serving the prior synthetic verbatim because re-render churn was 83% of cache-write spend | https://github.com/os-tack/ostk-cache/blob/main/src/rebuild.rs | new — payload cost transforms |
| Cross-provider cache-policy seam: Anthropic explicit breakpoints vs GPT `prompt_cache_retention`/`prompt_cache_key`, with normalized UsageSnapshot reconciliation | https://github.com/os-tack/ostk-cache/blob/main/src/provider_policy.rs | #14 |
| Streaming SSE usage rewriting: monotonic-max `input_tokens` overlay plus fabricated Anthropic-shaped errors to trigger client-side compaction | https://github.com/os-tack/ostk-cache/blob/main/src/usage_truth.rs | new — payload cost transforms |
| Progressive soft-cap body reduction pipeline (tool_result ejection → pair pruning → unused tool defs → structured 413 with reduction payload) | https://github.com/os-tack/ostk-cache/blob/main/README.md | new — payload cost transforms |
| Per-turn JSONL ledger with byte columns, stats CLI, and a two-window A/B passthrough protocol with a 5% noise threshold | https://github.com/os-tack/ostk-cache/blob/main/docs/PASSTHROUGH.md | context |
| Content-addressed file-handle inlining (~96-byte `file_id` refs) with a fail-open contract — every failure mode passes the original body | https://github.com/os-tack/ostk-cache/blob/main/src/rewrite_middleware.rs | new — payload cost transforms |
| Host lifecycle-hooks ingestion (SessionStart/PreToolUse/… posted to a `/hook/event` endpoint, rotated JSONL) | https://github.com/os-tack/ostk-cache/blob/main/README.md | context |
| Cross-session harness transcript tailing (Claude Code/Codex session logs → activity summary + tool_use_id seek index) | https://github.com/os-tack/ostk-cache/blob/main/src/transcript_tail.rs | context |
| Opt-in full HTTP capture with redaction, sharded rotation, bounded-channel writer, and self-loop guard | https://github.com/os-tack/ostk-cache/blob/main/src/http_capture.rs | context |

**Absent (verified).** No auth/multi-tenancy (loopback trust, forwards the
caller's API key), no retries/failover/aliasing (one upstream per process),
no rate limits or spend caps, no local response caching, no MCP beyond a
README mention, no metrics/admin API (local files only), kernel-daemon modes
Unix-only; not verifiable by build (private deps).

## claude-code-proxy (raine)

**Identity.** Single-crate Rust CLI (`src/` layout) running a local
Anthropic-compatible HTTP proxy that lets Claude Code drive
subscription-backed upstreams — Codex/ChatGPT, Kimi, Grok, Cursor Agent,
OpenCode Go — with `serve/monitor/demo/models` subcommands plus per-provider
`auth login|device|status|logout`. MIT; distributed via Homebrew, install
script, prebuilt GitHub Releases (not crates.io).

**Maintenance.** Active — latest commit 2026-10-31, v0.1.48 released
2026-11-01; 687 commits, 694 stars, ten releases Sep–Nov 2026.

**Sources.** Stats and activity: https://github.com/raine/claude-code-proxy
(repo page), https://github.com/raine/claude-code-proxy/releases (v0.1.48,
2026-11-01), and the run-cited release commit
https://github.com/raine/claude-code-proxy/commit/49cf152.

| Capability | Evidence | Gap |
|---|---|---|
| Request-path retries: 429/500/502/503/504 with 2 s→30 s exponential backoff honoring Retry-After; Codex empty-completion retry; subscription-limit fail-fast | https://raw.githubusercontent.com/raine/claude-code-proxy/main/src/retry.rs | #1 |
| Full OAuth credential lifecycle for subscription upstreams: browser PKCE, device-code, browser-polling flows; per-request token refresh; macOS Keychain or permissioned `auth.json` | https://claude-code-proxy.raine.dev/reference/command-reference/ | new — OAuth credential lifecycle |
| Model-ID-driven routing on a single listener: registered `gpt-*` IDs with `-fast`/`-ultrafast` service-tier suffixes, `cursor:` prefixes, Anthropic-style aliases via configurable `aliasProvider` | https://claude-code-proxy.raine.dev/using/models-and-routing/ | #13 |
| Embedded monitor TUI (attachable over SSH, auto-reconnect, 250 ms polling) plus loopback-only `/monitor` snapshot endpoint: sessions, lifecycles, tokens, throughput | https://claude-code-proxy.raine.dev/using/monitor-tui/ | context (terminal TUI; #19 covers browser-served consoles) |
| Subscription-quota usage and reasoning-token surfacing in usage reports (spend against the subscription, not USD prices) | https://raw.githubusercontent.com/raine/claude-code-proxy/main/CHANGELOG.md | context |
| Opt-in traffic capture grouped by Claude Code session, failed-response error captures, redacting rotating JSONL logs | https://claude-code-proxy.raine.dev/reference/files-and-storage/ | context |
| Multi-protocol inbound (`/v1/messages`, opt-in `/v1/chat/completions` and `/v1/responses`) plus five upstream translators incl. WebSocket transport with HTTP fallback | https://claude-code-proxy.raine.dev/reference/http-api/ | #6 |
| In-memory session-state continuity keyed by `x-claude-code-session-id` (Cursor conversation IDs, optional Codex `previous_response_id`/compaction) | https://claude-code-proxy.raine.dev/how-it-works/ | #14 |
| Per-provider offline token estimators for count_tokens (o200k_base plus image/reasoning/framing estimates) | https://claude-code-proxy.raine.dev/how-it-works/ | context (parity-plus: llm-proxy has offline tiktoken — `crates/llm-proxy-core/src/token/tiktoken.rs`; per-provider estimators are the refinement) |
| Opt-in image generation/editing endpoints and a changelog-era transcription route (current status unverified) | https://claude-code-proxy.raine.dev/reference/http-api/ | context (scoped future work in endpoints.md) |
| Homebrew/installer/Nix distribution; 64 MiB bodies; graceful SIGTERM | https://raw.githubusercontent.com/raine/claude-code-proxy/main/README.md | context |

**Absent (verified).** No client-auth validation (accepted but never
checked; non-loopback exposure warned against), no rate limiting, dedup,
budgets, or quotas, no admin/mutation API ("no administrative mutation API,
credential API, or remote shutdown route"), no persistent usage ledger
(monitor history in-memory, resets on restart), no multi-tenant keys, no
MCP, no first-party Anthropic/Gemini API upstreams, no per-provider egress
forward proxy (global env proxies only).

## mistral.rs (EricLBuehler)

**Identity.** Rust LLM inference engine on the Candle ML framework (~15-crate
workspace): `mistralrs serve` CLI/server binary, embeddable
`mistralrs-server-core` axum router crate, Python SDK, Docker images; MIT.
Included here as the serving-side cousin — the closest architectural peer on
the engine side of the dual-protocol problem llm-proxy solves at the proxy
layer.

**Maintenance.** Very active — last commit 2026-11-03, v0.9.5 (2026-10-19)
plus 27 commits on master; ~7.8k stars.

**Sources.** Stats and activity: https://github.com/EricLBuehler/mistral.rs
(repo page) and https://github.com/EricLBuehler/mistral.rs/releases (v0.9.5,
2026-10-19).

| Capability | Evidence | Gap |
|---|---|---|
| Side-by-side OpenAI `/v1` and Anthropic `/v1/messages` + count_tokens in one server; the internal hub is the OpenAI ChatCompletionRequest itself | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-server-core/src/anthropic.rs | #6 (serves `/v1/responses`), context (llm-proxy solves the same dual-protocol problem with a neutral IR — `docs/protocol.md:7-9`) |
| Full canonical Anthropic SSE event machine plus custom `agentic_tool_call_progress`/`agentic_tool_approval_required`/`file_produced` events | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-server-core/src/anthropic.rs | context |
| Anthropic error-envelope fidelity: typed statuses incl. 409/529, exact "prompt is too long" wording for context overflow, generic messages hiding internal error detail | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/serve/anthropic-messages-api.md | context |
| Agent permission system (auto/ask/deny) with streamed approval events, `/v1/agent/approvals/{id}` resolution, `remember_for_session`, request-may-only-tighten ratchet | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/agents/permissions-and-approvals.mdx | context (llm-proxy executes no tools) |
| Per-request context limits (smallest of model/paged-KV/`--max-model-len`) with per-API typed overflow errors, reported on `/v1/models` | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/agents/permissions-and-approvals.mdx | context |
| KV prefix caching (incl. multimodal) default-on for Metal plus in-situ re-quantization endpoint `/re_isq` | https://github.com/EricLBuehler/mistral.rs#what-makes-it-fast | #14 (engine-side) |
| Model-name routing with default/aliases and runtime load/unload/reload — a request to an unloaded model waits out an automatic reload | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/serve/multiple-models.mdx | context |
| MCP client: Process/HTTP/WebSocket transports, per-server bearer tokens, `tool_prefix` namespacing, timeouts, concurrency caps | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-mcp/README.md | #8 |
| MCP server mode: second listener serving JSON-RPC 2.0 at `POST /mcp`, exposing the served model as a single `chat` tool | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/agents/expose-as-mcp.md | #8 |
| Prometheus `/metrics` with vLLM-derived TTFT/inter-token/queue histograms and client-disconnect outcome detection via an observed body wrapper | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-server-core/src/metrics.rs | #12, #17 |
| Request-ID echo plus Anthropic-convention `request-id` header; JSON access logs | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-cli/src/args/server.rs | context |
| Endpoint breadth: Responses with background jobs + cancel, embeddings, audio speech/transcriptions/translations, image gen, files/containers, `/v1/skills`, LoRA mgmt, system doctor | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-server-core/src/route_registry.rs | context (llm-proxy's endpoints.md future list) |
| Stateful server-side sessions incl. live Python interpreter state, export/import, LRU cap, no-ID prefix heuristic | https://github.com/EricLBuehler/mistral.rs/blob/master/docs/src/content/docs/guides/agents/persist-sessions.mdx | context |
| Embeddable axum router builder (`MistralRsServerRouterBuilder`) + self-hosted OpenAPI/Swagger docs | https://github.com/EricLBuehler/mistral.rs/blob/master/mistralrs-server-core/src/lib.rs | context |

**Absent (verified).** No auth of any kind (docs: "use a reverse proxy"), no
rate limiting/quotas/cost/usage tracking, no gateway functions (no upstream
abstraction, retries, or failover), no response cache (KV only); Anthropic
fidelity gaps: thinking signatures always empty, `cache_control`
breakpoints/TTLs unimplemented (`cache_creation_input_tokens` always 0),
nested tool_result blocks and `is_error` not translated; sessions
in-memory only.

## SMG — Shepherd Model Gateway (smg-project)

**Identity.** Engine-agnostic LLM gateway in native Rust (Python
bindings/launcher), shipped as a server binary + `smg launch` CLI,
installable via Docker/Helm/pip/cargo; Apache-2.0, LightSeek-backed. Form
factor: self-hosted gateway fronting inference fleets (vLLM, TRT-LLM,
SGLang, Gemini and more).

**Maintenance.** Exceptionally active — commits dated 2026-11-04 (the
research day, ~34 that day), v1.14.0 shipped 2026-11-03, 22 releases through
2026; 699 stars, 4,479 commits. Research was docs-based (lightseek.org/smg
v1.14.0); no local checkout exists under `ref/`.

**Sources.** Stats and activity: https://github.com/smg-project/smg (repo
page) and the run's 15 opened pages under https://lightseek.org/smg/
(v1.14.0 docs).

| Capability | Evidence | Gap |
|---|---|---|
| KV-cache-aware routing as the default policy: four index modes (engine KV events, token-ID radix, text radix, TTL'd hash), match threshold, spill gate, per-model overrides | https://lightseek.org/smg/v1.14.0/concepts/routing/cache-aware | new — engine-state-aware routing |
| Engine-state-aware overload protection: queue-depth and KV-usage vetoes from `/v1/loads`, `/metrics`, gRPC `GetLoads`, KV-event records, or ZMQ, with optional 503 shedding + Retry-After | https://lightseek.org/smg/v1.14.0/concepts/reliability/overload-protection | new — engine-state-aware routing |
| Two-level tokenization cache: L0 exact-match (256 MB budget, approximate-LRU) + L1 prefix-at-special-token-boundaries (50 MB) so multi-turn chats tokenize only the suffix | https://lightseek.org/smg/v1.14.0/concepts/performance/tokenizer-caching | context |
| WASM plugin middleware (OnRequest/OnResponse) hot-deployed via `POST/GET/DELETE /wasm` without restart, sandboxed with fixed limits | https://lightseek.org/smg/v1.14.0/concepts/extensibility/wasm-plugins | context (extensibility surveyed as context in waves 1–2) |
| Priority admission scheduling: four classes via `x-smg-priority`, reserved floors, strict-priority drain, starvation promotion, preemption before first byte (a streamed byte is never preempted) | https://lightseek.org/smg/v1.14.0/concepts/reliability/priority-scheduling | new — admission queueing |
| Per-tenant token/request budgets with reserve-then-settle semantics (own estimate reserved, signed delta settled; failures never leak reservations; 429 with Retry-After) | https://lightseek.org/smg/v1.14.0/concepts/reliability/tenant-rate-limiting | #2, #3 |
| Gateway-wide admission control: concurrency permits over the whole stream, bounded FIFO queue, optional token-bucket refill, 429/503 with Retry-After and error-code headers | https://lightseek.org/smg/v1.14.0/concepts/reliability/rate-limiting | #3, new — admission queueing |
| Request-path retries: fixed retryable set, 50 ms→×1.5→30 s with ±20% jitter, bodies buffered ≤1 MiB, worker re-selection skipping failed workers, never retry a started stream | https://lightseek.org/smg/v1.14.0/concepts/reliability/retries | #1 |
| Per-worker three-state circuit breakers (429 deliberately uncounted, ≤1 failure charged per request, health-check promotion to half-open) | https://lightseek.org/smg/v1.14.0/concepts/reliability/circuit-breakers | #1 |
| Split control/data-plane auth: OIDC/JWT SSO with JWKS auto-discovery, SHA-256 hashed keys with constant-time compare, admin/user RBAC roles, tenant keys, control-plane audit log | https://lightseek.org/smg/v1.14.0/concepts/security/authentication | new — caller SSO/RBAC, #9 |
| Gateway-side MCP client: stdio + streamable HTTP (pooled dynamic servers), tool aliasing/dedup, ordered allow/deny approval policies, internal-server hiding, tool-loop caps | https://lightseek.org/smg/v1.14.0/concepts/extensibility/mcp | #8 |
| Very wide API surface: `/v1/responses` (MCP tool loops, stored conversations), `/v1/messages`, Gemini `/v1/interactions`, embeddings, rerank, classify, transcriptions, tokenize, SGLang `/generate`, `/v1/decisions`, realtime WS/WebRTC, vendor dialect profiles | https://lightseek.org/smg/v1.14.0/reference/api/openai | #6 + endpoints future work |
| 150+ Prometheus metrics across six layers (TTFT/TPOT histograms, breaker/KV-event health, capped cardinality) plus OTel trace export | https://lightseek.org/smg/v1.14.0/reference/metrics | #11, #12 |
| Async admin API with job model: worker CRUD (202 + pollable jobs), runtime tokenizer CRUD, `POST /flush_cache`, `GET /inflight`, heap profiles | https://lightseek.org/smg/v1.14.0/reference/api/admin | context (HTTP admin API; #19 covers browser-served consoles) |
| Sticky sessions/routing keys (header-derived, 4 h idle pin) plus HA mesh with SWIM gossip and CRDT state incl. radix-tree delta gossip | https://lightseek.org/smg/v1.14.0/reference/configuration | #14, context (multi-instance) |

**Absent (verified).** No USD cost tracking, pricing tables, or spend budgets
(anywhere in the config reference); no durable per-request usage ledger
(chat history storage aside — llm-proxy's SQLite ledger is ahead here —
`crates/llm-proxy-storage/src/sqlite.rs:105-131`);
per-instance tenant limits (~N× across replicas); no per-client-IP limiting;
no request dedup; WASM plugins cannot influence routing, are skipped on
failure, and bypass OnResponse on streams; no shipped Grafana dashboards.

## Antirion (eliasbiondo)

**Identity.** Apache-2.0 LLM/AI gateway written as a 27-crate Rust workspace
shipping one binary — or a distroless Docker image — with the Next.js admin
console, Admin API, embedded store, and CLI (serve/validate/schema/
healthcheck) built in; data plane :8080, admin :8081. Positioned (and
benchmarked) against Bifrost and LiteLLM.

**Maintenance.** Active but 18 days old at research time — repo created
2026-10-17, v0.1.0/v0.1.1 released 2026-10-18 with cosign/SLSA/SBOM, last
push 2026-10-31, 534 commits; one human maintainer (commits co-authored
with Claude); 662 stars. Pre-1.0, bus factor one.

**Sources.** Stats and activity: https://github.com/eliasbiondo/antirion
(repo page; created 2026-10-17) and
https://github.com/eliasbiondo/antirion/releases (v0.1.0/v0.1.1,
2026-10-18).

| Capability | Evidence | Gap |
|---|---|---|
| Route trees with fallback chains, weighted Thompson-sampling splits, least-cost/latency/busy and classifier pickers; session/prefix affinity; route simulation | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-resilience/src/engine/hedge.rs | #1, #13 |
| Hedged request races: extra attempts every `delay` up to `max_extra` while none has answered; losers cancelled and settled with health verdicts | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-resilience/src/engine/hedge.rs | new — hedged requests |
| Reservation-based limits engine (GCRA/sliding window) with per-attempt holds settled before the last response byte, degraded store mode with queued-settlement replay, lease-expiry release for crashed nodes | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-limits/src/engine.rs | #2, #3 |
| Budgets with forecasts, overage grants/approvals, event feed, per-period refused/downgraded counts — run entirely off the request path | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-runtime/src/budgets/mod.rs | #2 |
| Guardrails: PII/secrets/regex/moderation/LLM-judge with format-preserving redaction, block/redact/flag/divert actions, zero-data-retention mode | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-guardrails/src/lib.rs | #7 |
| Exact + semantic response cache with single-flight, epochs, memory slab and Valkey/Lua backends, console-manageable rules | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-cache/src/semantic.rs | #10 |
| Enterprise identity in-tree: virtual keys, JWT, BYOK, full SAML 2.0 (XML dsig, c14n), OIDC SSO, SCIM, TOTP/WebAuthn, RBAC roles, audit log with change approval | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-identity/src/saml/mod.rs | new — caller SSO/RBAC, #9 |
| MCP gateway (both protocol eras, encrypted session tokens, tool approval, OpenAPI-as-tools) plus an A2A proxy | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-mcp/src/lib.rs | #8 |
| WASI 0.2 + webhook plugin hooks, sha256-pinned, per-call stores capped at 64 MiB / 5 ms, no files/env/network | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-ext/src/wasm/mod.rs | context |
| OTLP metrics/traces with GenAI semantic names plus an alerting engine with SLOs and Slack/PagerDuty/email/webhook sinks | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-obs/src/metrics/otlp.rs | #11, #12 |
| Embedded admin console + OpenAPI 3.1 Admin API (ETag/If-Match, Idempotency-Key, `?wait=`, config generations with diff/rollback, `[REDACTED]` secrets) | https://github.com/eliasbiondo/antirion/blob/console/README.md | #19 |
| Reproducible open-loop benchmark harness vs Bifrost/LiteLLM with published raw per-cell results and validity flags | https://github.com/eliasbiondo/antirion/blob/main/docs/benchmarks.md | context |
| Connect-time SSRF validation on egress (URL rules + per-resolved-address rules, trust classes) | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-egress/src/ssrf.rs | context |
| In-repo fuzzing (13 cargo-fuzz targets), cargo-deny, reproducible builds, JSON-Schema config (`antirion schema`) | https://github.com/eliasbiondo/antirion/tree/main/fuzz/fuzz_targets | context |
| Thread-per-core data plane with least-loaded accept lanes and jemalloc; zero-config first start; cluster mode on PostgreSQL + Valkey; Helm chart; signed releases | https://github.com/eliasbiondo/antirion/blob/main/crates/antirion-server/src/threads.rs | context |

**Absent / caveats (verified).** Linux amd64/arm64 glibc-only binaries; no
Python/JS SDKs; no audio/image/video/files/batch/fine-tuning endpoints; no
durable request replay beyond retry budgets; ~3 weeks of production
exposure; benchmark cells largely flagged invalid (EC2 network floor) —
cross-gateway numbers need care.

## Plano (katanemo)

**Identity.** Katanemo's (a DigitalOcean company) open-source AI-native
proxy server and out-of-process data plane for agentic apps, built on Envoy;
the data plane is Rust — proxy-wasm cdylib plugins (`llm_gateway`,
`prompt_gateway`, `hermesllm`) inside Envoy plus the Rust "brightstaff"
model-serving process — with a TypeScript `planoai` CLI. Apache-2.0; run
via `planoai up config.yaml` or a single Docker image; declarative YAML.

**Maintenance.** Active — last commit and release 0.4.39 both 2026-10-24,
11 releases since mid-July 2026, active issues/PRs; 7.1k stars. Docs at
docs.planoai.dev; planoai.dev self-reports HuggingFace routing 115+ LLMs
across 1M+ interactions through it (vendor claim, https://planoai.dev/).

**Sources.** Stats and activity: https://github.com/katanemo/plano (repo
page) and https://github.com/katanemo/plano/releases (0.4.39, 2026-10-24).

| Capability | Evidence | Gap |
|---|---|---|
| NL-intent agent orchestration listener: a small orchestrator model infers intent per turn and routes to plain-HTTP `/v1/chat/completions` agents | https://docs.planoai.dev/guides/orchestration.html | context (agent-level routing) |
| Three-mode LLM routing with automatic fallback: exact provider/model, semantic aliases, or routing preferences (4B router), first model primary and rest fallbacks on 429/5xx | https://docs.planoai.dev/guides/llm_router.html | #1, #13 |
| Ordered guardrail filter chains: MCP- or HTTP-filter services that inspect/mutate/short-circuit (4xx = policy violation before the agent/LLM) | https://docs.planoai.dev/concepts/filter_chain.html | #7 |
| Token-budget rate limits keyed on arbitrary header selectors (or `*` wildcard) via the `governor` crate | https://docs.planoai.dev/resources/configuration_reference.html | #3 |
| Cheapest/fastest selection policy from live cost + latency catalogs (models.dev / custom / Prometheus query, refreshed) | https://docs.planoai.dev/resources/configuration_reference.html | #13, new — synced price catalogs |
| Model-affinity session pinning (`X-Model-Affinity`) with in-memory or Redis cache for multi-replica deployments | https://docs.planoai.dev/guides/llm_router.html | #14 |
| Agentic Signals: model-free behavioral quality detectors (3 layers, 7 categories) emitted as OTel attributes — loop detection incremental O(1) | https://docs.planoai.dev/concepts/signals.html | context |
| OpenTelemetry tracing with sampling, custom span attributes, exporters (incl. PostHog), traceparent propagation, `planoai trace` inspector | https://docs.planoai.dev/resources/configuration_reference.html | #11 |
| Prometheus metrics (:19901/stats, brightstaff :9092 RED + LLM TTFT/per-token metrics) plus a shipped self-contained Prometheus+Grafana compose with a 19-panel dashboard | https://docs.planoai.dev/guides/observability/monitoring.html | #12, #17 |
| Envoy-format access logs split into ingress / LLM / internal streams | https://docs.planoai.dev/guides/observability/access_logging.html | context |
| Ingress breadth: `/v1/chat/completions`, `/v1/responses`, `/v1/messages` cross-provider with streaming in both SDKs | https://docs.planoai.dev/concepts/llm_providers/client_libraries.html | #6 |
| Ten first-class providers + `provider/*` wildcard auto-configuration of every offered model, per-provider passthrough auth and custom headers | https://docs.planoai.dev/concepts/llm_providers/llm_providers.html | context |
| Persistent conversation state in Postgres backing `/v1/responses` continuation + `overrides.optimize_context_window` history trimming | https://docs.planoai.dev/resources/configuration_reference.html | new — payload cost transforms, context |
| MCP-native filter ecosystem and typed declarative `prompt_targets` (local nearest-neighbor intent matching via `acap`) | https://github.com/katanemo/plano/blob/main/crates/llm_gateway/Cargo.toml | #8 |
| planoai CLI lifecycle + single Docker image + K8s deployment guidance (rollout-restart on config change) | https://docs.planoai.dev/resources/deployment.html | context |

**Absent (verified).** No client auth/keys/admin API (any api_key accepted
locally), no usage ledger or billing-grade query API, no response caching or
dedup, no token-count endpoint or embeddings/rerank/audio/image families,
no hot config reload, no documented egress forward proxy or listener TLS;
`crates/prompt_gateway`/`hermesllm` contents unverified.

## AISIX (api7)

**Identity.** Rust-native Apache-2.0 AI gateway from API7 (the original
Apache APISIX creators) — an 18-crate Cargo workspace built as the OSS data
plane of commercial AISIX Enterprise/Cloud; single `aisix` static binary
proxying OpenAI- or Anthropic-format clients to OpenAI, Anthropic,
Gemini/Vertex, Bedrock, Azure OpenAI and any OpenAI-compatible upstream;
declarative `resources.yaml` (JSON-Schema checked, SIGHUP/etcd hot reload),
read-only admin listener, Prometheus listener, Docker image, small CLI.

**Maintenance.** Very active — created 2026-04-17, last push 2026-11-04, 987
commits; GA v1.0.0 2026-09-04, ten releases in eight weeks (v1.8.0,
2026-10-29); 212 stars / 48 forks (young, single corporate backer).

**Sources.** Stats and activity: https://github.com/api7/aisix (repo page:
created 2026-04-17, 987 commits) and https://github.com/api7/aisix/releases
(v1.0.0 2026-09-04 through v1.8.0 2026-10-29).

| Capability | Evidence | Gap |
|---|---|---|
| Six multi-target routing strategies (failover default, smooth-weighted RR, consistent hash, least-cost/latency/busy) with priority tiers, retry knobs, `retry_on_429`, `fallback_on_statuses`, per-model cooldowns and background health ejection | https://raw.githubusercontent.com/api7/aisix/main/README.md | #1, #13 |
| Multi-granularity rate limits: per-key/per-model rps/rpm/rph/rpd + tpm/tpd + in-flight concurrency, 429 pre-call with Retry-After and X-RateLimit-* headers; per-process or Redis (single/cluster/sentinel) shared counters, fail-open on Redis loss | https://raw.githubusercontent.com/api7/aisix/main/config.example.yaml | #3 |
| Caller API keys stored as SHA-256 hashes with RFC 3339 expiry, disable/enable, glob model allowlists, two-key rotation path, `x-aisix-api-key` header alternative | https://raw.githubusercontent.com/api7/aisix/main/README.md | #9 |
| OIDC/JWT caller authentication: JWKS (RS/PS/ES/EdDSA) or HMAC, aud/nbf/exp validation, required scopes, bound claims, priority-ordered claim→key mapping, coalesced JWKS refresh with stale-while-refresh | https://raw.githubusercontent.com/api7/aisix/main/README.md | new — caller SSO/RBAC |
| Layered guardrails at input/output/both: keyword/regex/semantic/PII built-ins plus Presidio, Lakera, OpenAI Moderation, Bedrock, Azure, Alibaba, custom scripts; monitor mode | https://raw.githubusercontent.com/api7/aisix/main/README.md | #7 |
| Response cache: exact-match of non-streaming chat completions on a normalized key (temperature compared at 0.001) with memory/Redis backends, TTL, applies_to selectors, purge generations, Cache-Control bypass; semantic layer on Redis 8+ search; automatic Anthropic prompt-cache breakpoints | https://raw.githubusercontent.com/api7/aisix/main/config.example.yaml | #10, #14 |
| MCP gateway: aggregated `ANY /mcp` and `/mcp/{server}` with gateway-held credentials, OpenAPI→MCP exposure, stateless 2026-07-28 revision, MCP conformance suite as merge-blocking CI | https://raw.githubusercontent.com/api7/aisix/main/README.md | #8 |
| A2A agent gateway at `/a2a/{agent}` (JSON-RPC + SSE, per-agent protocol pin, card URL rewriting, shared key governance) | https://raw.githubusercontent.com/api7/aisix/main/README.md | context |
| Broad endpoint families (chat, completions, responses, messages + count_tokens, embeddings, rerank, systemone/decisions, images, async video, audio, files/batches/fine-tuning, realtime WS, `/passthrough/:provider/*`) and five native adapter families (Bedrock SigV4, Vertex OAuth2, Azure) | https://raw.githubusercontent.com/api7/aisix/main/README.md | #6, #4 + endpoints future work |
| Observability: dedicated `:9090/metrics` with request- vs attempt-level counters (successful_fallbacks, TTFT/e2e histograms, auth decisions), per-attempt usage events with cache_status and error classes, OTLP GenAI/Datadog/SLS/S3 exporters with bounded queue, pprof and heap dumps | https://raw.githubusercontent.com/api7/aisix/main/README.md | #11, #12, #17 |
| Ops hardening: thread-per-core with SO_REUSEPORT, multi-listener TLS + PROXY protocol v1/v2, real_ip trusted CIDRs, outbound SSRF protection, upstream pool tuning incl. mTLS, SIGHUP/etcd hot reload, graceful drain, <10 MiB idle memory | https://github.com/api7/aisix/releases | #16 + context |
| Ensemble models (panel + judge) and semantic (embedding) routing incl. `jev` classifier mode | https://raw.githubusercontent.com/api7/aisix/main/README.md | #13 |
| Lock-free hot-path config reads via ArcSwap snapshots with reclamation of replaced snapshots | https://raw.githubusercontent.com/api7/aisix/main/crates/aisix-core/src/snapshot.rs | context |
| Budgets and spend caps — explicitly Enterprise/Cloud-only ("not part of the AISIX OSS resource model") | https://github.com/api7/aisix/releases | #2 (gated) |
| `aisix validate` offline JSON-Schema validation, read-only admin listener with OpenAPI 3 + Scalar UI, 183-scenario e2e suite, bench directory, MCP conformance harness | https://raw.githubusercontent.com/api7/aisix/main/README.md | context |

**Absent (verified).** OSS is data-plane only — budgets/RBAC/audit/encrypted
provider keys/control-plane dashboard are Enterprise/Cloud; no local usage
store or usage-query API in OSS (usage leaves via exporters only — llm-proxy's
SQLite ledger + usage API is a differentiator —
`crates/llm-proxy-storage/src/sqlite.rs:105-131`,
`crates/llm-proxy-api/src/routes.rs:53-55`); exact cache covers only
non-streaming chat completions; token windows only per-minute/per-day, fixed
windows, Redis-outage fail-open; legacy HTTP+SSE MCP unsupported; MCP serving
stateless (no session state); A2A guardrails input-only; roadmap-not-shipped:
prompt templates, scheduled key rotation, Vault/KMS, SAML, alerting, SDKs.

## mcp-gateway (MikkoParkkola)

**Identity.** Self-hosted MCP gateway/proxy in Rust shipped as a single
~12–13 MB binary (plus crates.io, Homebrew, npm, Docker/Helm), sitting
between an AI client and many backend MCP servers/REST APIs behind a
9–17-tool "meta-tool" surface with on-demand discovery (or a 2-tool Code
Mode). **PolyForm Noncommercial 1.0.0** — strictly an ideas reference for
llm-proxy, never code reuse (MIT versions were deprecated/yanked).

**Maintenance.** Very active — last commit 2026-11-02, ~3,191 commits,
releases every 1–4 weeks (stable v3.5.1 2026-09-04; v4.0.0-beta.3
2026-10-06); 85 stars, single author.

**Sources.** Stats and activity: https://github.com/MikkoParkkola/mcp-gateway
(repo page; ~3,191 commits) and
https://github.com/MikkoParkkola/mcp-gateway/releases (v3.5.1 2026-09-04,
v4.0.0-beta.3 2026-10-06).

| Capability | Evidence | Gap |
|---|---|---|
| Meta-tool MCP gateway surface with on-demand tool discovery (gateway_search_tools/gateway_invoke), aggregating stdio/Streamable-HTTP/SSE/WebSocket backends and REST APIs; A2A adapter | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/README.md | #8 |
| Fail-closed tool-poisoning and prompt-injection validator scanning every backend tool description (HIGH patterns fail closed; response-injection scanner; firewall JSON-RPC −32002) | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/security-controls.toml | #7, #8 |
| SHA-256 capability pinning with rug-pull quarantine: 110+ built-in capability YAMLs, pinned files failing closed on mismatch, changed tool definitions trigger unload-and-quarantine | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/README.md | #8 |
| End-user identity propagation: gateway-signed ES256 assertions, client-token passthrough, and RFC 8693 token exchange, all fail-closed; per-user OAuth isolation; RFC 8707 resource indicators | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/CHANGELOG.md | new — caller SSO/RBAC (identity axis) |
| Per-slot circuit breakers (per backend and per-user session-pool slot) plus a tunable business-logic error-budget kill switch revivable only via a meta-tool; 429 excluded from budget accounting | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/ARCHITECTURE.md | #1 |
| Per-client rate limiting with burst bucket (token bucket per caller key, not per-IP) | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/gateway.example.yaml | #3 |
| Cost governance with blocking tiers: currency, daily/per-tool/per-key budgets, 50/80/100% alert tiers where 100% blocks, `costs.json` persistence with anti-tamper hardening, cost-report meta-tool | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/gateway.example.yaml | #2 |
| SHA-256-keyed response cache plus three-state idempotency cache (in-flight → completed, SHA-256 over server:tool + canonical args, TTLs, failure eviction) and prompt_cache_key preservation | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/ARCHITECTURE.md | #10 |
| ~8 ms hot reload of backends and config without dropping connections (notify + meta-tools, transition tracker merging in-memory state) | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/README.md | #16 |
| Key server: SHA-256 hashed expiring API keys with OIDC, policies, admin token; single-use dashboard bootstrap link; agent auth | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/gateway.example.yaml | #9 |
| Hash-chain transparency audit log of tool invocations, tested Prometheus alert rules, stats meta-tool | https://github.com/MikkoParkkola/mcp-gateway/releases | #12 (metrics.rs counters and shipped Prometheus alert rules; the hash-chain audit trail itself is comparison context) |
| SSRF guard with resolves-then-pins DNS (rebinding defense), special-use IP blocking, cleartext-credential refusal at config load, Origin/Host cross-site validation, mTLS, HMAC+nonce message signing | https://raw.githubusercontent.com/MikkoParkkola/mcp-gateway/main/security-controls.toml | context |
| Full deployment stack: Docker (incl. `-full` with Node/uv/git/ssh), Helm, K8s manifests, Prometheus; setup wizard importing Claude/Cursor/Zed configs, `doctor`, web UI/dashboard | https://github.com/MikkoParkkola/mcp-gateway/tree/main/deploy | #19 |

**Absent (verified).** It is an MCP tool gateway, not an LLM API proxy — no
chat/messages/models client endpoints, no wire translation, no SSE token
translation, no embeddings/audio/image/batch, no offline token counting; no
SQL usage ledger (JSON + meta-tools only), no egress forward proxy observed,
no per-model aliasing/failover; "retry forwarding not yet implemented" for
the stateless 2026-07-28 protocol per its own changelog.

## ThinkWatch (ThinkWatchProject)

**Identity.** Rust Cargo workspace (server, gateway, mcp-gateway, auth,
common, test-support) with a React 19 console — a self-hosted dual-port
"bastion host for AI access": AI+MCP gateway on :3000, management
console/admin API on :3001, deployed via Docker Compose or Helm. Source
available under **BSL 1.1** (free up to 10M billable tokens + 10k MCP calls
per month; GPL-2.0+ changeover announced but not in effect).

**Maintenance.** Very active — repo created 2026-04-02, 913 commits, last
push 2026-11-04, v3.11.0 released 2026-11-03 after ~12 releases in the
prior month; 814 stars.

**Sources.** Stats and activity:
https://github.com/ThinkWatchProject/ThinkWatch (repo page; 913 commits,
created 2026-04-02) and
https://github.com/ThinkWatchProject/ThinkWatch/releases (v3.11.0,
2026-11-03); the run used the GitHub API and the releases page.

| Capability | Evidence | Gap |
|---|---|---|
| OIDC/SSO sign-in (Zitadel, Okta, Entra, Keycloak, Auth0) with JWT access + rotating refresh tokens, replay detection, IP-bound sessions, Argon2id passwords | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | new — caller SSO/RBAC |
| RBAC with five built-in roles (admin/operator/user/viewer/service) + custom roles over models, tools, and admin pages, incl. MCP tool-level access control | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | new — caller SSO/RBAC |
| `tw-` virtual keys stored as SHA-256 hashes, scoped to AI and/or MCP gateway, with rotation grace periods, inactivity timeouts, per-key limits | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | #9 |
| MCP gateway where tool calls run as the real user: per-user OAuth/token connections so upstream audit logs show who acted; unconnected callers get the authorization URL; AES-256-GCM tokens at rest | https://github.com/ThinkWatchProject/ThinkWatch/tree/main/crates/mcp-gateway/src | #8 |
| PII/secrets redaction with placeholder restore in streams: 131 rules in 20 groups, provider keys recognized by value, per-value exceptions | https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v3.11.0 | #7 |
| Dangerous-command and content-filter guards on requests and streamed answers (held-back tail matching, refuse/delete/replace/record actions) | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#behaviour-worth-knowing | #7 |
| Sliding-window rate limits (requests or tokens, 1 min–1 week) and spend budgets (day/week/month) on users/keys/roles with 50/80/95/100% alerts, 429 + Retry-After, fail-open default with fail-closed switch, client-facing `GET /v1/usage` | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#behaviour-worth-knowing | #2, #3 |
| Cost accounting with chargeback CSVs, month-end forecast, cost centers, per-model quota weights | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | #2 |
| ClickHouse audit trail (six tables, 90/30-day TTLs) of every model request and tool call with optional pre-redacted bodies, SIEM forwarding (Syslog/Kafka/signed webhooks), S3 offload >256 KiB | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/.env.example | context |
| Weight/latency/health routing with a `tw-breaker` circuit breaker and per-route sliding-minute RPM/TPM caps pushing traffic to the next route | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | #1, #13 |
| Transient upstream retry window (default 120 s) before any content reaches the client, with reasoning-effort auto-adaptation on rejection | https://github.com/ThinkWatchProject/ThinkWatch/releases | #1 |
| Provider-encrypted conversation pinning: conversations whose history a provider encrypted stay pinned 30 days with a clear SealedHistoryRefused error otherwise | https://github.com/ThinkWatchProject/ThinkWatch/releases | #14 |
| Four client dialects on one port (OpenAI Chat, Responses, Anthropic Messages, Gemini) translated to OpenAI/Anthropic/Google/Azure/Bedrock (SigV4 + event-stream decoding) upstreams | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#highlights | #6, #4 |
| Bearer-gated `/metrics` endpoint, unmounted when the token is unset | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/.env.example | #12 |
| Docker Compose + Helm deployment, distroless images, CPU/memory-adaptive execution, runbooks | https://github.com/ThinkWatchProject/ThinkWatch/blob/main/README.md#deployment | context |

**Absent / caveats (verified).** BSL 1.1, not OSI open source; README claims
TOT but the security doc does not document it (treat MFA as unverified); no
in-process TLS termination; chat-only protocol breadth (no embeddings,
rerank, transcription/TTS, image/video, batch, realtime — the same families
llm-proxy defers); no SAML (OIDC only); advertised LLM response caching
unverified (cache.rs purpose not read); heavy footprint (PostgreSQL + Redis
+ ClickHouse + optional S3).

## preflight (ghuntley)

**Identity.** Rust single-crate (edition 2024, axum 0.8 + tokio + reqwest +
rusqlite, ~3.6k lines) local HTTP inspection proxy that scans
OpenAI-compatible LLM requests and attachments for secrets before they
reach the model, replacing findings with stable `[REDACTED:rule]`
placeholders; axum server (127.0.0.1:8081) + CLI, a Bubblewrap-sandboxed
attachment worker, a Gitleaks rules updater, Nix flake + hardened NixOS
service module. By Geoffrey Huntley; the outbound DLP stage in front of his
`ghuntley/underclass` proxy. MIT.

**Maintenance.** Effectively dormant human development — the entire
implementation landed in 5 commits on 2026-09-20 (repo creation day), no
release/tag/CHANGELOG, no human commits since (two unmerged dependabot PRs);
the weekly rules workflow still runs green (latest success 2026-11-02)
without needing commits; 123 stars.

**Sources.** Stats and activity: https://github.com/ghuntley/preflight
(repo page; five commits on 2026-09-20), the run's clone of main @ eb13fc0,
and workflow runs at
https://github.com/ghuntley/preflight/actions/workflows/rules.yml (green
through 2026-11-02).

| Capability | Evidence | Gap |
|---|---|---|
| Request-path DLP secret scanning with the full Gitleaks rule DB (pinned v8.30.1) compiled in, gated by an explicit 18-rule profile; keyword prefilter → regex → entropy → SHA-256 allowlist carnet | https://github.com/ghuntley/preflight/blob/main/src/scanner.rs | #7 |
| Stable-placeholder redaction with redact / no-go (409) / advisory policy modes; un-rewritable findings fail closed; error bodies carry opaque finding UUIDs, never the secret | https://github.com/ghuntley/preflight/blob/main/src/main.rs | #7 |
| Local OCR and attachment inspection (JPEG/PNG/APNG/PDF, barcodes, metadata) via Tesseract/Poppler/QPDF/ExifTool/ZBar behind typed argv adapters; rebuilt artifacts re-inspected before approval | https://github.com/ghuntley/preflight/blob/main/src/attachments.rs | context (OCR is scoped future work in endpoints.md) |
| Bubblewrap-sandboxed, networkless worker processes (--unshare-all, tmpfs, 120 s, 16 MiB stdout cap); unknown formats and PDF active content fail closed | https://github.com/ghuntley/preflight/blob/main/docs/adr/0002-rust-workers-and-content-cache.md | context |
| Content-addressed verdict/artifact cache (memory + WAL SQLite with secure_delete) binding digest + credential scope + profile fingerprint, single-flight, generation-based purge | https://github.com/ghuntley/preflight/blob/main/src/cache.rs | context |
| Live cache administration (`status|purge [--scope]`) over a mode-0600 Unix control socket | https://github.com/ghuntley/preflight/blob/main/src/main.rs | context |
| SSRF-hardened remote materialization: https-only, no redirects/proxy, DNS resolved once and pinned, all IPs must be global-unicast, byte budgets, then replaced with inline data: URLs | https://github.com/ghuntley/preflight/blob/main/src/resolver.rs | context |
| Concurrency admission control: 16-permit semaphore returning 503 `inspection_capacity`; 2 attachment/4 scan worker slots | https://github.com/ghuntley/preflight/blob/main/src/main.rs | #3, new — admission queueing (backpressure form) |
| Prometheus `/metrics` with lock-free counters and a request-to-headers histogram; aggregate labels only | https://github.com/ghuntley/preflight/blob/main/src/metrics.rs | #12 |
| Secret-free structured JSON logging contract + cross-proxy x-request-id correlation (ingress-minted UUIDv4 propagated upstream) | https://github.com/ghuntley/preflight/blob/main/src/correlation.rs | context |
| SIGHUP atomic config reload with immutable runtime snapshots (failed reload keeps the previous runtime; admitted requests keep their snapshot) | https://github.com/ghuntley/preflight/blob/main/src/main.rs | #16 |
| Split credentials: optional client key, upstream key substitution after hop-by-hop stripping, separate file-API key | https://github.com/ghuntley/preflight/blob/main/src/main.rs | context |
| NixOS hardened service module (DynamicUser, ProtectSystem=strict, MemoryMax…) + QEMU VM test; weekly fully-gated automated Gitleaks rules updates committed with provenance | https://github.com/ghuntley/preflight/blob/main/nixos/module.nix | context |

**Absent (verified).** OpenAI-shaped passthrough only (no `/v1/messages`, no
translation), hard-wired to a single "underclass" upstream; no
retries/failover/multi-provider routing (explicitly delegated); no
response-side inspection (upstream bodies stream through uninspected); no
rate limiting per client/IP, quotas, budgets, cost tracking, or usage
history; single shared bearer key compared non-constant-time (no key ids,
rotation, or hashing); no MCP; Linux-only, Nix-centric; runtime behavior
against a live provider not verified.

## tokdiet (agiwhitelist)

**Identity.** TypeScript/Node 20 CLI (`npx tokdiet start`, alias `td`)
running a local, loopback-only streaming reverse proxy on :7787 with a live
dashboard on :7878 between AI coding agents (Claude Code, Cursor, Codex,
opencode, Aider) and model APIs (Anthropic, OpenAI, Gemini, MiniMax); it
meters tokens/USD and compacts bloated agent context by recoverably paging
out cold tool results. Also a Claude Code marketplace plugin (metering hook
only). Born as "Context Governor" (legacy CTXGOV_* names survive).

**Maintenance.** Dormant — 12 commits in a 3-day AI-assisted burst
(2026-06-16/18), one npm release (0.1.0, 2026-06-16), no releases/tags since,
one unanswered issue (a pricing bug, Oct 2026); 32 stars. Treat as a
proof-of-concept snapshot.

**Sources.** Stats and activity: https://github.com/agiwhitelist/tokdiet
(repo page; 12 commits, 2026-06-16/18) and
https://www.npmjs.com/package/tokdiet (0.1.0, 2026-06-16).

| Capability | Evidence | Gap |
|---|---|---|
| Fail-open mutating streaming proxy core: every stage individually guarded so any internal error degrades to byte-transparent passthrough ("the proxy never breaks the user's workflow") | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/src/proxy.ts | new — payload cost transforms, context |
| Context "virtual memory" compaction: hot/warm/cold tiers, loss-free dedup, recoverable elision (240-char previews + 12 salient lines, originals persisted to SQLite), optional mid-summarize; published −71% input tokens | https://github.com/agiwhitelist/tokdiet/tree/main/src/compactor | new — payload cost transforms |
| Shadow-eval quality guard: ~5% of compacted requests re-sent uncompacted and scored, 50-sample rolling average vs a 2% max-degradation budget, tripping overall or per-strategy safe-mode; priors persisted across restarts | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/src/quality/guard.ts | new — shadow-eval quality guard |
| Published 66-task A/B quality-parity benchmark harness: paired governed/ungoverned runs against a real model, objective grader + LLM judge, majority voting, per-category regression list | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/bench/run.mjs | context |
| Page-fault detection (model echoing paged-out `cg-` ids or complaining) with one-shot blob re-injection | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/src/pagefault.ts | new — payload cost transforms |
| Hierarchical USD budget gate: session → day (UTC) → repo-month precedence, warn/compact/block actions (block = 429) | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/src/budget.ts | #2 |
| Cost metering from an out-of-band SSE usage tee with a user-editable, hot-reloaded pricing.json incl. cache read/write rates; keys forwarded, never persisted | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/ARCHITECTURE.md | new — synced price catalogs |
| Anthropic prompt-cache-aware and thinking-safe mutation guards: never rewrite at/before a cache_control breakpoint (would invalidate ~0.1×-priced prefixes); never mutate signed/thinking blocks | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/README.md | #14 (safety constraint for any transform) |
| Live SSE dashboard (five screens: session, savings, quality, tool/repo, strategy leaderboard) over better-sqlite3 | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/ARCHITECTURE.md | #19 |
| Claude Code plugin: PreToolUse/PostToolUse metering hooks appending JSONL records; `/tokdiet` command; never blocks a tool | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/hooks/tool-meter.mjs | context |
| Four provider adapters with per-provider upstream override env vars; unknown JSON fields pass through untouched | https://raw.githubusercontent.com/agiwhitelist/tokdiet/main/README.md | context |

**Absent (verified).** No auth (127.0.0.1 bind is the whole security model),
no retries/fallback/load balancing (one fixed upstream per provider), no
rate limiting, no response caching (preserves upstream prompt-cache prefixes
only), no MCP, no Prometheus/request-id/configurable logging, no cursor-paged
usage API; page-fault recovery impossible for streaming; embedding judge
unimplemented; unknown models silently cost $0.00 (open pricing bug);
dormant.

## CLIProxyAPI (router-for-me)

**Identity.** Go 1.26 single-binary proxy server (plus embeddable Go SDK and
an optional desktop client) that turns coding-CLI subscriptions — ChatGPT
Codex, Claude Code, Gemini CLI, Antigravity, AI Studio, Kimi, Grok Build,
Meta Muse Code, Devin — into OpenAI/Gemini/Claude/Codex/Grok-compatible
local APIs with multi-account OAuth credential pools. MIT; 57.5k stars /
8.8k forks / 4,709 commits; large third-party ecosystem.

**Maintenance.** Extremely active — last commit and v8.0.54 on 2026-11-04
(research day); near-daily releases. Core value prop (impersonating official
CLI clients against subscription backends) is ToS-gray: treated here as a
pattern reference, not an integration target.

**Sources.** Stats and activity:
https://api.github.com/repos/router-for-me/CLIProxyAPI (stars/forks/commits,
read 2026-11-04), https://github.com/router-for-me/CLIProxyAPI (the run's
shallow clone at main HEAD 78a9cc9), and
https://github.com/router-for-me/CLIProxyAPI/releases (v8.0.54,
2026-11-04).

| Capability | Evidence | Gap |
|---|---|---|
| Per-provider OAuth login flows: browser PKCE, device-code, service-account import, SIWC binding, headless Management-API auth URLs | https://github.com/router-for-me/CLIProxyAPI/blob/main/cmd/server/main.go | new — OAuth credential lifecycle |
| Multi-account credential pools with three rotation strategies (round-robin with identity-based rotation, smooth weighted, fill-first to stagger rolling-window caps), priority tiers, prefix pinning | https://github.com/router-for-me/CLIProxyAPI/blob/main/sdk/cliproxy/auth/selector.go | #5 |
| Credential-level retry rounds (default 3) with per-round credential caps, cooldown backoff levels, model-scoped cooling, and custom error classification (stop / stop-and-cooldown / continue) | https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml | #1, #5 |
| Session-affinity sticky routing for prompt/KV-cache reuse: session headers, prompt_cache_key, Responses conversation IDs, or first-message hash; subagents inherit the parent's credential | https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml | #14 |
| Passive upstream quota observation from response headers (Retry-After watermarks, limit-reached flags) kept separate from cooldown state; live quota fetch/reset API | https://github.com/router-for-me/CLIProxyAPI/blob/main/sdk/cliproxy/auth/quota_signals.go | #5 |
| Very wide client + upstream surface: chat/completions, completions, responses (HTTP + WebSocket + compact), messages + count_tokens, images, video, TTS, `/v1/live` realtime incl. WebRTC, Gemini native, Codex aliases | https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/api/server_routes.go | #6 + endpoints future work |
| v8 Management API: GET/PUT/PATCH/DELETE over the whole YAML tree with comment-preserving PATCH, auth-file upload/download/refresh, ~80 runtime toggle routes, auto-downloaded web panel | https://github.com/router-for-me/CLIProxyAPI/blob/main/docs/management-api-v8.md | #19, #16 |
| Cluster mode with a distributed credential-lease control plane ("Home"): JWT/mTLS bootstrap, heartbeats, fences, reclaim grace so nodes share one pool without double-spending | https://github.com/router-for-me/CLIProxyAPI/blob/main/docker-compose.cluster.yml | context (multi-instance) |
| Pluggable credential token stores: file, Postgres, Git repo, S3/MinIO registered through one interface | https://github.com/router-for-me/CLIProxyAPI/blob/main/cmd/server/main.go | context |
| Payload rewrite rules engine: set-if-missing / always-overwrite / strip by JSON path with rich matchers (model, protocol, headers, payload conditions) | https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml | context (request policy layer) |
| Stream-bootstrap buffering: hold handshake/heartbeat frames (48-frame/1 MiB budget) so HTTP-200-embedded rejections can still fail over before headers commit | https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml | #1 (pre-commit fallback refinement) |
| Embeddable Go SDK + c-shared plugin system with a plugin store; TLS listener, zstd/gzip response compression, mDNS LAN discovery, pprof, TUI, example-key safe-mode | https://github.com/router-for-me/CLIProxyAPI/blob/main/docs/sdk-usage.md | context |
| Client fingerprint cloaking / CLI impersonation (pinned CLI wire fingerprints, uTLS, CCH signing, zero-width obfuscation) | https://github.com/router-for-me/CLIProxyAPI/blob/main/config.example.yaml | context (ToS-gray; documented as pattern only) |

**Absent (verified).** No persistent usage/cost ledger (built-in statistics
removed in v6.10.0, delegated to third-party tools; ~60 s in-memory queue
only), no built-in budgets/spend caps/pricing, downstream auth is a flat
plaintext api-keys list (no hashes, no per-key limits — llm-proxy's
argon2id + per-key attribution are stronger — `config.toml.example:42-54`),
no response caching, no
content guardrails, no MCP server, no embeddings/rerank/batch/files/
fine-tuning.

## sub2api (Wei-Shaw)

**Identity.** Go 1.27 + Gin + Ent + Vue 3 multi-tenant
"subscription-to-API" gateway platform (PostgreSQL 15+, Redis 7+) that turns
pooled AI product subscriptions (Claude, Codex, Gemini, Grok, Antigravity,
CN providers) into distributable `sk-` keys with per-user billing, budgets,
payments (Alipay/WeChat/Stripe/EasyPay) and an account-pool scheduler.
LGPL-3.0; 45.4k stars. Its README carries a ToS-violation warning and it
ships provider-adversarial machinery — a **design reference for
account-pool mechanics, not a compliant integration target**.

**Maintenance.** Extremely active — last push 2026-11-04, ~7,850 commits,
releases every 2–5 days (v0.2.19, 2026-10-28).

**Sources.** Stats and activity:
https://api.github.com/repos/Wei-Shaw/sub2api (stars/forks/commits, read
2026-11-04) and https://github.com/Wei-Shaw/sub2api/releases (v0.2.19,
2026-10-28).

| Capability | Evidence | Gap |
|---|---|---|
| Account-pool schedulability state machine: five independent time-based isolation states (overload, rate-limit reset, temp-unschedulable ± model scope, expiry, session windows) persisted as columns | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/account.go | #5 |
| Sticky-session scheduling with prompt-cache affinity: session key from client session IDs, else SHA-256 of `cache_control:{ephemeral}` content, else client digest; Redis binding with 1 h TTL; sticky wait queues; account switches reclassify cache billing | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/gateway_service.go | #14 |
| In-request failover loop keyed to 401/403/429: per-account retry counters, same-account retry delays/deadlines, switch budgets, mid-stream switching after SSE comments only; client disconnects classified 499 and stop failover | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/handler/failover_loop.go | #1, new — upstream cancellation (499 classification) |
| Status-code→account-health taxonomy: 401 OAuth cache invalidation + recoverable cooldown, 402 permanent stop, 429 5 h/7 d quota windows parsed from headers, 529 overload cooldown | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/ratelimit_service.go | #5 |
| Background OAuth token refresh with distributed lock and a stale-snapshot rollback guard; per-platform refreshers | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/token_refresh_service.go | new — OAuth credential lifecycle |
| Append-only token-level cost ledger: 5 m/1 h cache variants, per-kind USD costs, total vs actual (post-multiplier) with rate-multiplier snapshots, first-token latency, requested-vs-upstream model mismatch flag, billing circuit breaker | https://github.com/Wei-Shaw/sub2api/blob/main/backend/ent/schema/usage_log.go | #2 |
| Layered budgets: per-key 5 h/1 d/7 d USD windows, user subscription quotas, and Redis in-flight balance reservation (estimate = input + output price × max_tokens, TTL-renewed, fail-open on Redis loss) | https://github.com/Wei-Shaw/sub2api/blob/main/backend/migrations/064_add_api_key_rate_limits.sql | #2 |
| Per-user-per-group rate multipliers (billing markup), RPM overrides, and a scheduler profit gate with veto cap and anti-livelock handling | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/user_group_rate.go | context (reseller economics) |
| Remote pricing catalog (Wei-Shaw/model-price-repo) with SHA-256 verification, local fallback, override patch merge, 10-minute hash rechecks, hot reload without restart | https://github.com/Wei-Shaw/sub2api/blob/main/deploy/config.example.yaml | new — synced price catalogs |
| Redis concurrency slots per account and per user (sorted-set with expiry sweeps, stale cleanup) feeding multi-layer load-aware selection; SSE pings while queued | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/concurrency_service.go | #3 |
| Broad multi-protocol surface: messages(+count_tokens), responses(+subpaths), chat/completions, embeddings, models, usage/billing, `/v1/live` realtime, images (sync/async/tasks), videos, tts/stt/custom-voices, x_search, Gemini cachedContents CRUD | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/server/routes/gateway.go | #6 + endpoints future work |
| Management MCP server: stateless Streamable-HTTP endpoint exposing four immutable tool sets (user/admin × read/read-write) with bounded envelopes and per-tool annotations | https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/managementmcp/server.go | #8 |
| Synthetic channel monitors (probe/quota modes, AES-256-GCM probe keys, jittered intervals), async prompt-audit guard, dashboard aggregation caches | https://github.com/Wei-Shaw/sub2api/blob/main/backend/ent/schema/channel_monitor.go | context |
| Hardened admin tooling: scoped admin keys with expiry/IP allowlists, passkeys/TOTP/step-up, session fingerprint binding, OIDC connect, Idempotency-Key on admin writes, systemd/compose/Caddy deployment | https://github.com/Wei-Shaw/sub2api/blob/main/deploy/README.md | context |

**Absent (verified).** No local response caching or request dedup
(idempotency middleware covers admin APIs only); not embeddable (Go
monolith, mandatory PostgreSQL + Redis, no single-binary embedded storage);
plugin surface limited to local OAuth transport plugins; no rerank or OCR;
provider-adversarial features (TLS fingerprint profiles, CLI mimicry,
per-account proxy rotation) make it non-compliant as an integration target;
deep behavior documented mostly in source comments (Chinese docs).

## Claude Code Router (musistudio)

**Identity.** TypeScript/Node.js local agent-gateway in three form factors:
Electron desktop app (macOS/Windows/Linux, v3.1.3), npm CLI (`ccr`, gateway
:3456, web UI :3458), Docker Compose — a "control plane" for coding agents
(Claude Code, Codex, Grok CLI, Kimi, OpenCode, Kilo, Pi, ZCode, WorkBuddy)
with routing rules, credential pools, MCP aggregation and usage analytics.
MIT; 38k stars; started 2025-02-25, the day after Claude Code's release.

**Maintenance.** Active — last commit 2026-10-10, v3.1.3 (2026-10-09), ~30
releases on a bug-fix-driven cadence; 950 open issues / 237 PRs show backlog
pressure.

**Sources.** Stats and activity:
https://github.com/musistudio/claude-code-router (repo page: ~38k stars, 989
commits, issue/PR counts) and
https://github.com/musistudio/claude-code-router/releases (v3.1.3,
2026-10-09).

| Capability | Evidence | Gap |
|---|---|---|
| Ordered conditional routing rules with request rewriting: dot-path conditions with 9 operators, set/delete/append rewrites, built-in Claude Code detection and subagent auto-routing | https://ccrdesk.top/routing/ | #13 (policy routing) |
| Sandboxed Node.js route scripts (Worker with frozen input + fetch/fs/env API) with size/time/rewrite caps and circuit breaking after 3 failures in 60 s | https://github.com/musistudio/claude-code-router/tree/main/packages/core/src/routing | context (extensibility) |
| Retry policy + ordered model-chain fallback: off / retry-N (408/409/429/5xx + network) / model-chain on any 4xx-5xx; Retry-After honored, 1 s→30 s backoff, `x-ccr-fallback-model` header | https://github.com/musistudio/claude-code-router/releases | #1 |
| Upstream credential pools with per-key priority, weight and quota; rotation on refresh-401 (Kimi/Grok); macOS keychain lookup; optional balance/quota connectors | https://ccrdesk.top/guides/provider/ | #5 |
| Client access keys with expiration (never/7/30/90/custom), instant revoke on delete, per-key local quotas across requests/tokens/images × min/hour/day; constant-time compare | https://ccrdesk.top/configuration/api-keys/ | #9, #3 |
| Request logs with resolved route, credential, latency, tokens, cost, full bodies, and agent-execution-chain reconstruction | https://ccrdesk.top/guides/observability/ | context |
| Usage dashboard (time ranges, cache ratio, success rate, heatmap, model distribution) plus exportable PNG share cards | https://ccrdesk.top/configuration/overview/ | #19 |
| Five client-side protocols with any-to-any translation (OpenAI Chat/Responses, Anthropic Messages, Gemini generateContent/interactions) + auto endpoint protocol/model detection and presets | https://github.com/musistudio/claude-code-router/blob/main/packages/core/src/routing/protocol-endpoints.ts | #6 |
| ToolHub MCP aggregation with lazy tool resolution: `tool_hub.resolve` (retrieval model picks 1–20 tools) + `tool_hub.invoke`; stdio/streamable-http/SSE transports; built-in browser automation; session id forwarded as prompt_cache_key | https://ccrdesk.top/toolhub/ | #8, #14 |
| Fusion: composed virtual models bundling base + capability models (vision/search/image/video/MCP tools) as ordinary routing entries | https://ccrdesk.top/fusion/ | context |
| Desktop MITM proxy mode (CA-cert interception) for clients that ignore base-URL env vars; /health; admin token; model catalog CDN refresh | https://ccrdesk.top/configuration/server/ | context |
| AgentClaw chat-relay access to agents via Weixin/Slack/Discord/Telegram/LINE/Feishu/DingTalk | https://github.com/musistudio/claude-code-router/blob/main/README.md | context |
| Provider presets, deeplink/manifest import, connectivity check that sends real requests before saving | https://ccrdesk.top/guides/provider/ | context |

**Absent (verified).** No native TLS termination (reverse proxy advised);
request logs retained same-day only (no configurable retention — llm-proxy's
max_age/max_rows pruning is an advantage — `config.toml.example:56-68`);
no response/prompt caching of its
own; no guardrails/content filtering; cost estimation-only; no embeddings,
rerank, batch, transcription/TTS, or realtime endpoints; hot-reload of
routing rules not documented.

## Otari (mozilla-ai)

**Identity.** Mozilla.ai's Apache-2.0, Python 3.13+ OpenAI-compatible LLM
gateway ("a gateway you own and run"): FastAPI-style async server in a
hexagonal `src/gateway` package over PostgreSQL/SQLite (SQLAlchemy/alembic),
a React dashboard, and a distributable `otari` CLI; Docker/Compose with
Render and Railway templates; three runtime modes (standalone, hosted
control plane at otari.ai, hybrid data plane). Built on Mozilla's any-llm
provider abstraction.

**Maintenance.** Extremely active — repo created 2026-04-02, last push and
v0.37.0 both 2026-11-04, 13+ minor releases in the preceding three weeks;
835 stars. Still pre-1.0 with active schema churn (~90 alembic migrations).

**Sources.** Stats and activity: https://github.com/mozilla-ai/otari (repo
page; 835 commits, created 2026-04-02) and
https://github.com/mozilla-ai/otari/releases (v0.37.0, 2026-11-04).

| Capability | Evidence | Gap |
|---|---|---|
| Pre-dispatch budget holds: a request's worst case (prompt estimate + entire max_tokens output) held BEFORE dispatch in one conditional update; reconciled to measured cost after; crashed holds returned after a TTL | https://github.com/mozilla-ai/otari/blob/main/docs/budgets.md | #2 |
| Scoped budgets: org/workspace/membership/key ceilings on USD, token and request axes plus rpm/tpm per user, rolling or UTC calendar resets, inert until a spend ceiling applies them | https://github.com/mozilla-ai/otari/blob/main/docs/budgets.md | #2, #3 |
| Structured budget-refusal semantics: 403 with `Otari-Error-Code: budget_exceeded`, an `Otari-Budget-Scope` header naming the refusing budget, Retry-After until window end, and a body stating both amounts | https://github.com/mozilla-ai/otari/blob/main/docs/budgets.md | #2 |
| Workspace-scoped virtual keys: per-key model allowlists (provider wildcards), tool allowlists, own budget ceilings stacking with the owner's, expiry, budget exemption, service-key flag; role-split issuance; regeneration kills the old secret immediately | https://github.com/mozilla-ai/otari/blob/main/docs/api-keys.md | #9 |
| Service keys with per-end-user spend, budgets and per-minute limits from a curated list; pseudonymous per-end-user provider identifiers recorded on usage rows | https://github.com/mozilla-ai/otari/blob/main/docs/end-users.md | new — caller SSO/RBAC (end-user identity axis) |
| Workload-identity federation: backends authenticate as a key with a platform OIDC identity token instead of the key secret | https://github.com/mozilla-ai/otari/blob/main/docs/api-keys.md | new — caller SSO/RBAC |
| Routing policies: on_failure failover (incl. 400/401/429 and empty choices), budget-conditional selection on `budget_used_pct`/`budget_remaining_usd`, weighted router, priority router skipping rate-full models; per-request override headers, error codes, `otari routing explain` CLI | https://github.com/mozilla-ai/otari/blob/main/docs/routing.md | #1, #13 |
| Automatic provider selection with sticky prompt-cache sessions: up to 32 offerings ranked by estimated cost, health, region, latency; colon spelling pins one provider; session bindings reuse provider+credential | https://github.com/mozilla-ai/otari/blob/main/docs/models.md | #14 |
| Multi-dimensional rate-limit rules: per key/user/end-user/deployment/model/budget with rpm, tpm and max_concurrent (15-min slots); `tpm_admission` holds prompt+max_tokens; report mode; rate_limit_store memory or Redis; live edits propagating within 30 s | https://github.com/mozilla-ai/otari/blob/main/docs/configuration.md | #3 |
| Managed MCP tool loop: inline or workspace-stored servers, gateway executes tool calls and re-calls the model (terminal last round without tools), encrypted tokens, SSRF-checked URLs, caller-orchestrated mode | https://github.com/mozilla-ai/otari/blob/main/docs/mcp.md | #8 |
| Layered inference guardrails callers cannot remove: caller opt-in, org-mandated, and operator-mandated on routing policies (block with fail-closed on unavailability) | https://github.com/mozilla-ai/otari/blob/main/docs/guardrails.md | #7 |
| Built-in tools: sandboxed code execution (sandbox_url or hosted e2b), web search and fetch with provider-native interception (auto|otari|provider) and max_uses caps | https://github.com/mozilla-ai/otari/blob/main/docs/tools.md | context |
| Agent traces with a context-window map (8 otari.context.* attributes per span), OTLP ingestion, opt-in sealed content capture, Prometheus `/metrics` | https://github.com/mozilla-ai/otari/blob/main/docs/traces.md | #11, #12 |
| Idempotency-Key replay of stored (encrypted) responses and `streaming_first_chunk_hold_ms` holding the status line for the provider's first chunk so pre-output failures return proper errors and can fall over | https://github.com/mozilla-ai/otari/blob/main/docs/configuration.md | #1 (safe-retry refinement) |
| Agent Guardrails: standalone `otari hook` binary wiring repo-level gates (paths, commands, verifiers, rubrics) into Claude Code/Codex hooks | https://github.com/mozilla-ai/otari/blob/main/docs/agent-guardrails.md | context |

**Absent (verified).** No response caching (exact or semantic; prompt-cache
tokens tracked for accounting only), no per-IP rate limiting (everything
keyed to key/user/budget/model/deployment), no unauthenticated loopback mode
(bootstrap key minted on first boot; no `mode=none` equivalent —
`config.toml.example:42-54`), no
per-provider egress forward proxy documented, not embeddable and not Rust,
no cargo-dist-style single binary (Docker/uv/brew instead).

## done-hub (deanxv)

**Identity.** Single-binary Go (Gin) LLM relay gateway with an embedded
React web UI, forked from MartialBE/one-hub (one-api lineage) with drop-in
DB/Docker compatibility; its distinctive work is agent-CLI OAuth channel
types (Claude Code, Codex, Gemini CLI, Antigravity, Vertex AI Express),
cross-protocol native routing, and a long trail of quota-ledger correctness
fixes. Apache-2.0.

**Maintenance.** Active — last push 2026-10-27, latest release v1.23.22
(2026-09-29), ~40 tagged releases Sep 2025–Sep 2026; 472 commits, 815 stars.

**Sources.** Stats and activity: https://github.com/deanxv/done-hub (repo
page; last push 2026-10-27) and https://github.com/deanxv/done-hub/releases
(v1.23.22, 2026-09-29).

| Capability | Evidence | Gap |
|---|---|---|
| Multi-bucket cache-token usage ledger: 5 m/1 h cache-write buckets, cache-read, OpenAI cache_write_tokens (deliberately decoupled from Anthropic 1.25×), audio/image/text × input/output, reasoning tokens — each billable at its own ratio | https://github.com/deanxv/done-hub/blob/main/types/common.go | #2 (billing fidelity) |
| Documented quota-ledger correctness fixes incl. the cached_tokens `omitempty` bug (zero values dropped, strict clients retried in a loop); fix ships zero-value serialization tests | https://github.com/deanxv/done-hub/blob/main/README.md#L127-L133 | context (direct lessons for the usage ledger) |
| Flat Anthropic-style cache fields parsed in OpenAI-compatible usage as fallback billing (all cache-write billed at the conservative 5 m ratio) | https://github.com/deanxv/done-hub/commit/bbb2293e34 | context |
| Pre-consume + streaming realtime quota enforcement via a Redis INCRBY Lua script with TTL that can abort mid-stream at the quota; graceful skip when quota ≫ estimate | https://github.com/deanxv/done-hub/blob/main/relay/relay_util/quota.go | #2 |
| Batched DB quota writer with ordered graceful-shutdown flush (StopBatchUpdater before FlushAllBatches or the ticker swaps the map mid-write); int32→bigint quota columns | https://github.com/deanxv/done-hub/blob/main/model/utils.go | context |
| Agent-CLI OAuth channel types: Claude Code console OAuth, Codex OAuth with version-gated UA and per-model instruction files, Gemini CLI published credentials, Antigravity, Vertex Express | https://github.com/deanxv/done-hub/blob/main/providers/codex/base.go | new — OAuth credential lifecycle |
| Cross-channel retry bounded by live channel count with wall-clock retry timeout collapsing to 503, honest retry_skip/retry_timeout logs, and Retry-After-derived cooldowns taking precedence | https://github.com/deanxv/done-hub/blob/main/relay/main.go | #1 |
| Group-based channel distribution with token-pinned groups, backup groups and per-group billing ratios | https://github.com/deanxv/done-hub/blob/main/middleware/distributor.go | #13, #2 |
| API tokens with per-key quota, expiry, unlimited flag, batched used-quota metering | https://github.com/deanxv/done-hub/blob/main/model/token.go | #9, #2 |
| Multi-bucket per-user + per-IP rate limiting with CIDR whitelist and a CriticalRateLimit that whitelisting deliberately does NOT exempt (brute-force protection) | https://github.com/deanxv/done-hub/blob/main/middleware/rate-limit.go | #3 |
| Prometheus metrics with per-channel/model dimensions; upstream request-id correlation; panic counter | https://github.com/deanxv/done-hub/blob/main/metrics/main.go | #12 |
| Built-in MCP server (SSE at /mcp/message + Streamable HTTP) exposing gateway tools to agents | https://github.com/deanxv/done-hub/blob/main/mcp/server.go | #8 |
| Pluggable content-safety checker framework and MaxPromptTokens input guard (400 before any upstream call) | https://github.com/deanxv/done-hub/blob/main/safty/safty.go | #7 |
| Broad endpoint families across 45+ adapters (chat, completions, embeddings, rerank, realtime, transcriptions, speech, translations, moderations, images, Veo video, search, Midjourney tasks); long-context tiered pricing applied incrementally on streams; bulk price import from models.dev | https://github.com/deanxv/done-hub/blob/main/relay/relay_util/quota.go | context (endpoints.md-scoped families; no Responses surface cited), new — synced price catalogs |

**Absent / caveats (verified).** No request dedup window (llm-proxy has
one — `crates/llm-proxy-server/src/routes/core_pipeline.rs:182-185`); logs
full request/response bodies by default (llm-proxy's digests-only
posture is stronger privacy — `crates/llm-proxy-storage/src/sqlite.rs:130-131`); no offline token counting (billing trusts
upstream-reported usage — the source of its cache-token bugs); realtime
quota enforcement silently degrades to post-hoc billing when Redis is
disabled; full multi-user web-app attack surface (payments, invite/rebate,
OAuth trust gating) llm-proxy deliberately avoids; the exact GLM float-token
fix commit could not be located (README line is the only citable evidence).

## yolorouter

**Identity.** Self-hosted, OpenAI-compatible LLM gateway in Go (1.25.7+)
shipped as a single server binary with an embedded multi-user web admin
console (serve/stop/update subcommands); SQLite by default, optional
PostgreSQL. Apache-2.0. Launched July 2026 as a failover gateway for AI
coding CLIs; went multi-protocol with cost-optimization transforms in v0.2.0.

**Maintenance.** Active — 606 commits, last commit 2026-10-28 (verified in
the research run's shallow clone, HEAD 7ac9750); twenty releases at roughly
weekly cadence, v0.1.0 (2026-07-21) → v0.2.9 (2026-10-27); 254 stars.

**Sources.** Stats and activity: https://github.com/yolorouter/yolorouter
(repo page: 606 commits) and the run's shallow clone at
https://github.com/yolorouter/yolorouter/tree/7ac9750 (HEAD verified
2026-10-28); releases at https://github.com/yolorouter/yolorouter/releases.

| Capability | Evidence | Gap |
|---|---|---|
| Upstream key pool with rate-limit benching: plain 429 benches a key for its Retry-After clamped 1 s–10 min; benched keys demoted (soonest-to-recover first), never excluded — an all-cooling pool still dispatches | https://github.com/yolorouter/yolorouter/blob/main/internal/gateway/keypool.go | #5 |
| Background key auto-recovery loop (30 s heartbeat) retesting system-kicked keys and releasing benches without admin action | https://github.com/yolorouter/yolorouter/blob/main/internal/keyrecovery/loop.go | #5 |
| Multi-provider failover with a generation-guarded three-state circuit breaker (soft half-weight faults, time-based probe admission, generation token so pre-transition results cannot mis-book state); key rotation and failover complete before the first byte | https://github.com/yolorouter/yolorouter/blob/main/internal/gateway/circuit/circuit.go | #1 |
| Balanced scheduling mode with per-key prompt-cache affinity: caller keys spread evenly and stuck to one provider so upstream prompt caches stay warm | https://github.com/yolorouter/yolorouter#scheduling-modes | #14 |
| Request-path tool-output compression: anchored-regex detection of go test/pytest/vitest/npm/pnpm logs, git diffs, grep results through a most-specific-first compressor chain, live-zone protected, replace-only-if-shorter, fail-open | https://github.com/yolorouter/yolorouter/blob/main/internal/compress/compress.go | new — payload cost transforms |
| Custom system-prompt injection ("house rules") implemented per egress protocol shape, deterministic to preserve prompt caches, with a published 150-pair on/off benchmark (median +12.6% output reduction, negatives reported honestly) | https://github.com/yolorouter/yolorouter/blob/main/internal/capability/systemprompt/inject.go | new — payload cost transforms |
| Per-caller key governance: budgets (micros), RPM/TPM/concurrency, model allowlists, expiry, instant revocation; media jobs reserve priced bounds against the budget while in flight | https://github.com/yolorouter/yolorouter/blob/main/internal/service/apikey/api_key_service.go | #2, #3, #9 |
| Four chat wire protocols any-to-any (OpenAI Chat/Responses, Anthropic Messages, Gemini) plus images, a pollable video job dialect, and speech; native media providers | https://github.com/yolorouter/yolorouter#protocols | #6 + endpoints future work |
| Seven-phase configurable relay timeouts (connect, TLS, header, first-byte, body-idle, attempt, request) replacing a single wall clock so long-reasoning models are not cut off; ordering validated at startup | https://github.com/yolorouter/yolorouter/blob/main/internal/config/config.go | new — timeout hierarchy |
| Community price catalog (github.com/yolorouter/price-catalog) with one-click model import, background verification of imported mappings (write-nothing probes, vision detection), unpriced candidates never auto-enabled | https://github.com/yolorouter/yolorouter/blob/main/CHANGELOG.md | new — synced price catalogs |
| Request logs with full per-attempt routing chains, final upstream URL, compress skip reasons, and per-modality billing snapshots; console shows full client and upstream bodies | https://github.com/yolorouter/yolorouter/blob/main/internal/model/request_log.go | context, #17 |
| W3C traceparent propagation into the audit log (strict version-00 parsing, never invents an id) | https://github.com/yolorouter/yolorouter/blob/main/internal/gateway/traceparent.go | context |
| Multi-user console with OAuth2/OIDC SSO, per-account scoping, AES-256 upstream keys at rest | https://github.com/yolorouter/yolorouter#why-yolorouter | new — caller SSO/RBAC, #15, #19 (embedded web console) |
| Single-binary ops: embedded console, self-update, systemd/launchd/Windows installers, global or per-provider http/https/socks5/socks5h egress proxy (~30 s hot reload), vision-fallback capability | https://github.com/yolorouter/yolorouter#quick-start | context |

**Absent (verified).** No MCP support (grep over internal/, cmd/, frontend
found none); no gateway-side response or semantic caching (cache tokens only
metered and priced); no inbound guardrails (content-policy refusals only
become failover signals); no embeddings, audio transcription (speech is
synthesis-only), batch, realtime/WebSocket, or files; no Kubernetes/Helm;
balanced-mode bindings process-local with a 4096-pair LRU cap; compressors
cover a fixed set of tool-output shapes; contributor/star figures from the
landing page (GitHub API rate-limited during research).

## completion-to-response (NoahStepheno)

**Identity.** ~800-line Go single-binary HTTP proxy (pure stdlib — go.mod has
zero require blocks) exposing `POST /v1/responses` and translating OpenAI
Responses API requests to any Chat Completions backend (Zhipu, DeepSeek,
Ollama, vLLM, Azure) and back, including full SSE event-chain synthesis;
companion Node gateway and bash scripts add multi-backend routing. Built
during the Codex CLI's Responses-only era. MIT.

**Maintenance.** Dead — 6 commits between 2026-05-02 and 2026-05-17, one
release (v0.1.0, 2026-05-02), archived read-only on 2026-08-16; 14 stars.
The DeepSeek thinking fix and multi-tool grouping exist only on unreleased
main.

**Sources.** Stats and status:
https://github.com/NoahStepheno/completion-to-response (repo page: 14 stars;
archived read-only banner dated 2026-08-16; six commits 2026-05-02/17) and
https://github.com/NoahStepheno/completion-to-response/releases (v0.1.0,
2026-05-02).

| Capability | Evidence | Gap |
|---|---|---|
| OpenAI Responses client endpoint (`POST /v1/responses`) over Chat Completions upstreams — input arrays, instructions, tools, json_schema, max_output_tokens, stream | https://github.com/NoahStepheno/completion-to-response/blob/main/cmd/server/main.go | #6 (the exact blueprint) |
| Full Responses SSE event-sequence synthesis from chat chunks (response.created → … → response.completed) with per-event output/content indexes | https://github.com/NoahStepheno/completion-to-response/blob/main/internal/transform/transform.go | #6 |
| Complete mapping tables: instructions→system, developer→system, max_output_tokens→max_tokens, finish-reason→status (tool_calls→requires_action), usage renames, resp_/msg_/fc_ id generation, late-system merge ordering rule | https://github.com/NoahStepheno/completion-to-response/blob/main/internal/transform/transform.go | #6 |
| Streaming tool-call reconstruction into `function_call` output items with stable item ids across fragments (and reverse grouping of consecutive function_call inputs) | https://github.com/NoahStepheno/completion-to-response/blob/main/internal/transform/transform.go | #6 |
| Reasoning preservation: `reasoning_content` deltas buffered and attached as `summary` on function_call items (DeepSeek thinking-mode; unreleased main) | https://github.com/NoahStepheno/completion-to-response/blob/main/internal/types/completion.go | #6 (thinking-content mapping) |
| Bearer passthrough with static-key fallback (the entire auth surface) | https://github.com/NoahStepheno/completion-to-response/blob/main/cmd/server/main.go | context |
| Multi-backend model-ID routing sidecar (Node) advertising alias models via `/v1/models` so Codex lists them | https://github.com/NoahStepheno/completion-to-response/blob/main/codex-gateway.js | context |
| Hardened SSE transport: 64 KB initial / 1 MB max scanner bound, [DONE] handling, flusher requirement, verbatim ≥400 forwarding; 10 s drain shutdown | https://github.com/NoahStepheno/completion-to-response/blob/main/cmd/server/main.go | context |
| Unit tests pinning the synthesized event contract (ordering + per-chunk counts) — the shape of contract tests a Responses endpoint owes | https://github.com/NoahStepheno/completion-to-response/blob/main/internal/transform/transform_test.go | context |

**Absent (verified).** Archived; no client-key verification, rate limiting,
caching/dedup, retry/fallback/load balancing, cost tracking, persistence,
metrics/tracing/request ids, MCP, or container packaging; Responses coverage
partial: no background mode, `store`/`previous_response_id` parsed but
unused, no built-in tools, no `sequence_number`, no error/incomplete_details
event synthesis, usage omitted from streamed completion, reasoning untested.

## GPTCache (zilliztech)

**Identity.** Python library (pip `gptcache`, MIT, by Zilliz — the Milvus
makers) for semantic caching of LLM responses, plus an optional FastAPI
server (`gptcache_server`, Docker `zilliz/gptcache`) exposing a generic
get/put API and an OpenAI-compatible chat proxy. The 2023 original of the
five-stage semantic-cache pipeline taxonomy that later semantic caches are
patterned on (a repeated design lineage, not a verified census). 8.2k stars.

**Maintenance.** Maintenance-mode / dead-in-practice — 34 releases Apr–Nov
2023, final release v0.1.44 on 2024-08-01, then 2 commits in Jul 2025 and a
one-day burst of 10 commits on 2026-09-22 (cache-reuse benchmark merges, no
release); README states "we no longer add support for new API or models";
100 open issues. README checkbox list is stale — trust the source tree.

**Sources.** Stats and status:
https://api.github.com/repos/zilliztech/GPTCache (metadata read by the run:
not archived, default branch main, last push 2026-09-22) and
https://github.com/zilliztech/GPTCache/releases (v0.1.44, 2024-08-01).

| Capability | Evidence | Gap |
|---|---|---|
| Five-stage pluggable cache pipeline (cache_enable_func → pre_embedding → embedding → data manager → similarity → post-process) with `next_cache` tiering — the canonical taxonomy | https://github.com/zilliztech/GPTCache/blob/main/gptcache/core.py | #10 |
| Rank-threshold similarity adjudication with 11 interchangeable evaluators (exact, vector-search distance, ONNX model, k-reciprocal, Cohere rerank, SBERT cross-encoder, time-window, JEV…) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/similarity_evaluation/time.py | #10 (semantic) |
| Temperature-aware cache skip (softmax over [skip, don't-skip]) and multi-answer post-processors (softmax/first/random) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/adapter/adapter.py | #10 |
| LLM-verification gate on cached answers (yes/no check; a "no" or exception degrades to a miss) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/processor/post.py | #10 |
| Eviction subsystem: soft-evict then threshold hard-delete, mark-then-sweep, periodic index rebuild; MemoryCacheEviction (LRU/FIFO/LFU/RR) and distributed RedisCacheEviction | https://github.com/zilliztech/GPTCache/blob/main/gptcache/manager/eviction/manager.py | #10 |
| Three-axis storage matrix: scalar (SQLite/Postgres/MySQL/DynamoDB/…), vector (Milvus/FAISS/pgvector/Qdrant/…), object store (disk/S3) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/manager/vector_data/manager.py | #10 |
| Session-scoped caching with pluggable hit filters (refuse repeats within a session) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/session.py | #10 |
| Adapter stream capture on miss and SSE synthesis from a cached answer on hit (`_construct_stream_resp_from_cache`) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/adapter/openai.py | #10 (hit-replay-as-SSE) |
| FastAPI cache server + Docker image with a language-agnostic get/put API (and a '/cache_skip ' magic prefix bypass) | https://github.com/zilliztech/GPTCache/blob/main/gptcache_server/server.py | context |
| Multi-tier cache chaining via `next_cache` (local L1 falling through to a shared L2) plus `search_only` probing | https://github.com/zilliztech/GPTCache/blob/main/gptcache/adapter/adapter.py | #10 |
| Cache-consistency self-heal between scalar and vector stores (embedding compare, forced miss + rewrite on mismatch) | https://github.com/zilliztech/GPTCache/blob/main/gptcache/adapter/adapter.py | #10 |
| Per-stage cache telemetry, hit callbacks, and persisted hit reports | https://github.com/zilliztech/GPTCache/blob/main/gptcache/report.py | context |

**Absent (verified).** Not a gateway: no auth/key management (server
authenticates nothing except cache-file download), no routing/retries/rate
limits/quotas/budgets, no cost tracking, no MCP, no protocol translation hub
(adapters are per-SDK monkey-patches; streaming proxy OpenAI-shaped only),
no external metrics export, no admin API/health checks; multimodal adapters
experimental.

## PromptCache (messkan)

**Identity.** Self-hosted semantic-caching proxy for LLM chat completions in
Go 1.24+ (Gin + embedded BadgerDB, single server binary under `cmd/api` with
`internal/` packages — not a library). Sits in front of an OpenAI-compatible
provider, embeds prompts, and returns cached answers above a two-threshold
band, with a small-model "intent verification" pass for gray-zone
candidates. Born Nov 2025 in the self-hosted "cut LLM costs" wave.

**Maintenance.** Quiet but not dead — 4 releases (v0.1.0 Nov 2025 → v0.4.0
2026-04-24), last commit 2026-08-18 (~2.5 months before 2026-11-04); 426
stars, 47 commits, bursty AI-assisted cadence; 4 open issues / 2 PRs
unanswered.

**Sources.** Stats and activity: https://github.com/messkan/prompt-cache
(repo page) and https://github.com/messkan/prompt-cache/releases (v0.1.0
Nov 2025 through v0.4.0 2026-04-24; later additions unreleased on main).

| Capability | Evidence | Gap |
|---|---|---|
| Two-threshold semantic matching: ≥0.70 direct hit, ≤0.30 miss, gray zone between (env-tunable, validated bounds) | https://github.com/messkan/prompt-cache/blob/main/internal/semantic/semantic.go | #10 (semantic) |
| Small-model intent verification for gray-zone hits (gpt-4o-mini / mistral-small / claude-3-haiku yes/no judge) | https://github.com/messkan/prompt-cache/blob/main/internal/semantic/providers.go | #10 |
| SSE streaming end-to-end: misses forward the provider stream while reassembling a non-streaming completion for the cache; hits synthesize OpenAI-style SSE chunks | https://github.com/messkan/prompt-cache/blob/main/RELEASE_v0.4.0.md | #10 (hit synthesis) |
| Runtime threshold and provider tuning via authenticated `GET/PATCH /v1/config` without restart | https://github.com/messkan/prompt-cache/blob/main/RELEASE_v0.4.0.md | context (runtime mutation) |
| Retryable upstream HTTP client (429/500/502/503/504, 500 ms × 2 backoff with 0–25% jitter capped 30 s) — not idempotency-aware | https://github.com/messkan/prompt-cache/blob/main/internal/http/client.go | #1 |
| Prometheus `/metrics` (hits/misses/gray-zone/failures, latency) plus JSON `/v1/stats` with hit-rate | https://github.com/messkan/prompt-cache/blob/main/internal/metrics/metrics.go | #12 |
| Constant-time bearer auth on management endpoints; inference and health stay public by design | https://github.com/messkan/prompt-cache/blob/main/internal/middleware/middleware.go | context |
| Cache lifecycle: 24 h TTL, 100k-entry LRU, hourly cleanup, `POST /v1/cache/warm` bulk insert, GET/DELETE management | https://github.com/messkan/prompt-cache/blob/main/internal/cache/cache.go | #10 |
| Namespace-partitioned cache isolation with ANN partition safety (namespaced lookups fall back to filtered linear scan so candidates never cross partitions) — unreleased on main | https://github.com/messkan/prompt-cache/blob/main/internal/semantic/semantic.go | #10 |
| Multi-provider embedding/verification backends (OpenAI, Mistral, Claude via Voyage embeddings) with an OpenAI→Claude translation lane | https://github.com/messkan/prompt-cache/blob/main/internal/semantic/providers.go | context |
| X-Cache/X-Similarity-Score/X-Cache-Key/X-Provider diagnostic response headers, allocation-free | https://github.com/messkan/prompt-cache/blob/main/internal/middleware/middleware.go | context |

**Absent (verified).** No rate limiting or request shaping (roadmap only),
no quotas/budgets/cost tracking, no per-tenant auth (single admin token; the
namespacing work is unreleased and explicitly "not tenant isolation"), no
clustering or distributed cache, no dashboard, no MCP, one data-plane
endpoint only; "ANN" index is brute-force O(n·d) with a stale-LRU-index bug
after middle removals; retries blindly re-POST; Claude adapter hardcodes
MaxTokens 1024.

## ollamaMQ (Chleba)

**Identity.** Single-crate Rust CLI/proxy server (edition 2024, tokio +
axum 0.8, flat `src/` of 7 files) acting as a message-queue dispatcher and
load balancer in front of Ollama, LM Studio, vLLM-style and "System One"
decision-model backends, with a built-in ratatui TUI dashboard. Zero
protocol translation — every request forwarded byte-for-byte. MIT.

**Maintenance.** Active — 67 commits, last commit 2026-10-30; ~10 tagged
releases v0.2.3 (2026-03-14) → v0.5.0 (2026-09-27) with Cargo.toml already
bumped to 0.5.1; 130 stars, solo maintainer plus one contributor.

**Sources.** Stats and activity: https://github.com/Chleba/ollamaMQ (repo
page: 67 commits; last commit 2026-10-30) and
https://github.com/Chleba/ollamaMQ/releases (v0.2.3 2026-03-14 through
v0.5.0 2026-09-27).

| Capability | Evidence | Gap |
|---|---|---|
| Per-user fair-share scheduling with exponentially decaying scores (300 s half-life), charged at dispatch regardless of outcome so failing clients cannot hold priority | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | new — admission queueing |
| Per-user FIFO queues with multi-layer bounded admission: 100/user queue cap (429), 512 MiB global queued-body budget (503), 64 MiB per-request body (413), 1 GiB blob allowance; metadata reads never queue | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | new — admission queueing |
| Least-connections load balancing with per-backend and per-model concurrency caps keyed on publisher/quant-stripped base names | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | #3, #13 |
| Warm-model state-affinity routing in three tiers: backends with the model loaded in RAM → cold backends → warned last-resort eviction | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | new — engine-state-aware routing |
| API-family detection with learning (a backend rejecting an endpoint twice gets that family excluded) and deterministic model matching (exact/tag/publisher/@quant-stripped, never substrings); strict model pinning with 503 after stuck_timeout | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | context |
| Model load/unload control via TUI keys and an admin HTTP API (GET /admin/models, POST /admin/models/load|unload), serialized per backend | https://github.com/Chleba/ollamaMQ/blob/master/src/control.rs | context (lifecycle management) |
| Declarative model-residency config applied additively at startup and on TUI reload ('r'), each load waiting up to 300 s for idle | https://github.com/Chleba/ollamaMQ/blob/master/appconf.yaml.example | #16 |
| Live ~10 Hz ratatui dashboard doubling as a runtime queue-admin console (VIP/Boost/block-user/block-IP/load/unload keys) | https://github.com/Chleba/ollamaMQ/blob/master/src/tui.rs | context (terminal TUI; #19 covers browser-served consoles) |
| Constant-time single-key auth (X-API-Key/Bearer) plus per-backend egress token substitution overriding client Authorization | https://github.com/Chleba/ollamaMQ/blob/master/src/main.rs | context |
| Background health probing every 10 s with per-backend endpoint memory and offline marking that excludes backends from routing | https://github.com/Chleba/ollamaMQ/blob/master/src/control.rs | #1 (health exclusion) |
| Fail-fast stuck_timeout 503 distinguishing unsatisfiable (pinned, no holder) from merely-busy requests; dispatcher scans past an unroutable queue head | https://github.com/Chleba/ollamaMQ/blob/master/src/dispatcher.rs | context |
| Size-rotated JSONL request/response logging with 64 KiB content caps and TUI full-content viewer; headers never logged | https://github.com/Chleba/ollamaMQ/blob/master/src/main.rs | context |
| Aggregated `/api/tags` + `/v1/models` across backends; the full Ollama + OpenAI + System One endpoint surface; opt-in catch-all passthrough | https://github.com/Chleba/ollamaMQ/blob/master/src/main.rs | context |
| Docker/compose/systemd packaging and a poison-tolerant lock recovery trait (~150 lock sites) | https://github.com/Chleba/ollamaMQ/tree/master | context |

**Absent (verified).** Single static plaintext API key (no hashes, ids,
rotation, budgets, quotas); no usage metering/persistence or cost accounting
(JSONL only — llm-proxy's ledger is well beyond —
`crates/llm-proxy-storage/src/sqlite.rs:105-131`); no request-path
retry/fallback (queues and fail-fasts instead); pure byte passthrough (no
translation, no Anthropic client endpoint, no content normalization/SSE
machine); no response/prompt/semantic caching, guardrails, MCP, metrics
endpoint, OTel, true RPM/TPM windows, TLS termination, or CI.

## Capability index

Areas → sections. Numbers refer to the compendium gap register; `new` items
are the wave-3 candidates listed in the intro.

| Area | Sections |
|---|---|
| Request-path retries, fallback, cooldowns (#1) | Switchyard, xLB, SMG, Antirion, Plano, AISIX, mcp-gateway, ThinkWatch, CLIProxyAPI, sub2api, Claude Code Router, Otari, done-hub, yolorouter, claude-code-proxy (raine), PromptCache, ollamaMQ (health exclusion) |
| Load-balancing / model-selection strategies (#13) | Switchyard, Antirion, Plano, AISIX, ThinkWatch, claude-code-proxy (raine), done-hub, Otari, ollamaMQ |
| Timeout hierarchy (new) | xLB (5 phases), Switchyard (chain deadline), yolorouter (7 phases) |
| Auth, keys, identity (#9; caller SSO/RBAC new) | AISIX, ThinkWatch, SMG, Antirion, mcp-gateway (key server), Otari, done-hub, CLIProxyAPI (absence), yolorouter, Claude Code Router, tokdiet (absence) |
| OAuth credential lifecycle (new) | claude-code-proxy (raine), CLIProxyAPI, sub2api, done-hub |
| Credential pools, rotation, quota state (#5) | CLIProxyAPI, sub2api, yolorouter, Claude Code Router |
| Budgets, cost, pricing (#2; synced price catalogs new) | Antirion, mcp-gateway, tokdiet, sub2api, Otari, yolorouter, done-hub, Switchyard (routing log), Plano (live cost catalogs); SMG and preflight verified absent |
| Rate limits, TPM, concurrency (#3) | xLB, SMG, Plano, AISIX, ThinkWatch, ollamaMQ, yolorouter, done-hub, Claude Code Router, preflight, mcp-gateway, PromptCache (absence) |
| Admission queueing (new) | SMG (priority classes + preemption), ollamaMQ (per-user FIFO + fair-share), xLB (queue_full backpressure), preflight (semaphore backpressure) |
| Engine-state-aware routing (new) | SMG, ollamaMQ, Switchyard (theoretical tracking) |
| Response caching (#10) | Antirion, AISIX, mcp-gateway, GPTCache, PromptCache; done-hub and CLIProxyAPI verified absent |
| Prompt-cache plumbing and cache locality (#14) | ostk-cache, claude-code-proxy (raine), CLIProxyAPI, sub2api, SMG, Claude Code Router, Otari, yolorouter, Plano, tokdiet (mutation guards), ThinkWatch (encrypted-history pinning), mistral.rs (engine-side KV cache) |
| Payload cost transforms + shadow-eval guard (new) | tokdiet, ostk-cache, yolorouter, Plano, Switchyard (compact) |
| Guardrails, DLP, secrets (#7) | preflight, Antirion, Plano, AISIX, ThinkWatch, mcp-gateway (tool poisoning), Otari, done-hub, Switchyard (redaction) |
| Upstream cancellation propagation (new) | xLB, sub2api (499 classification) |
| Observability: Prometheus (#12), OTel (#11), percentiles/TTFT (#17) | Switchyard, xLB, SMG, Antirion, Plano, AISIX, mcp-gateway, preflight, done-hub, yolorouter, PromptCache, ThinkWatch, Otari, mistral.rs |
| MCP (#8) | SMG, Antirion, Plano, AISIX, mcp-gateway, ThinkWatch, Claude Code Router (ToolHub), Otari, sub2api, done-hub, mistral.rs (client + server modes) |
| OpenAI Responses client endpoint (#6) | Switchyard, claude-code-proxy (raine), mistral.rs, SMG, AISIX, Plano, ThinkWatch, CLIProxyAPI, sub2api, Claude Code Router, yolorouter, completion-to-response; wider endpoint breadth is `context` (endpoints.md scope) |
| Cloud-native upstream auth (#4) | AISIX (Bedrock SigV4 / Vertex OAuth2 / Azure adapters), ThinkWatch (Bedrock SigV4 + Azure) |
| Web admin UI (#19) | Antirion (embedded console), mcp-gateway (wizard/doctor/dashboard), CLIProxyAPI (v8 web panel), Claude Code Router (web UI), yolorouter (web console), tokdiet (web dashboard); terminal TUIs (claude-code-proxy, ollamaMQ) and SMG's HTTP admin API are tagged `context` |
| Hot reload (#16) | mcp-gateway (~8 ms), ollamaMQ (TUI reload), CLIProxyAPI (management API), preflight (SIGHUP), AISIX (SIGHUP/etcd) |
| Hedged requests (new) | Antirion |

## Coverage appendix — all deduplicated wave-3 finds

Every candidate the nine niche hunts surfaced (121 repositories); ✦ marks
the 23 researched in full above. Stars/commit data as captured by the hunts
in November 2026; "dormant"/"dead" judgments are the hunts'.

| Repository | What it is | Signals |
|---|---|---|
| ✦ [NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) | Rust model-routing toolkit (libsy + bindings) | Rust · ~3.5k★ · pushed 2026-11-04 · very active |
| ✦ [katanemo/plano](https://github.com/katanemo/plano) | Envoy+proxy-wasm data plane for agentic apps | Rust/TS · ~7.1k★ · pushed 2026-10-24 · active |
| [Xerxes-2/clewdr](https://github.com/Xerxes-2/clewdr) | Rust reverse proxy for Claude (web sessions + Claude Code), OpenAI-compatible out | Rust · ~1.3k★ · pushed 2026-11-01 · active |
| ✦ [ThinkWatchProject/ThinkWatch](https://github.com/ThinkWatchProject/ThinkWatch) | AI+MCP gateway with SSO/RBAC, budgets, audit | Rust · ~815★ · pushed 2026-11-04 · active |
| ✦ [smg-project/smg](https://github.com/smg-project/smg) | Shepherd Model Gateway, engine-agnostic, KV-cache-aware | Rust · ~700★ · pushed 2026-11-04 · active |
| ✦ [eliasbiondo/antirion](https://github.com/eliasbiondo/antirion) | Rust gateway w/ embedded console, benchmarks vs Bifrost/LiteLLM | Rust · ~660★ · pushed 2026-10-31 · young/active |
| ✦ [api7/aisix](https://github.com/api7/aisix) | Rust-native AI gateway from the APISIX creators | Rust · ~212★ · pushed 2026-11-04 · active |
| ✦ [EeroEternal/xlb](https://github.com/EeroEternal/xlb) | Streaming-first thin-L7/L4 AI load balancer | Rust · ~173★ · pushed 2026-10-19 · dormant |
| ✦ [Chleba/ollamaMQ](https://github.com/Chleba/ollamaMQ) | Queue dispatcher/LB for Ollama/LM Studio with TUI | Rust · ~130★ · pushed 2026-10-30 · active |
| ✦ [ghuntley/preflight](https://github.com/ghuntley/preflight) | Rust DLP/secret-scan inspection proxy | Rust · ~123★ · pushed 2026-11-02 · dev dormant (CI green) |
| ✦ [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | Go proxy exposing CLI subscriptions as APIs | Go · 57.5k★ · pushed 2026-11-04 · extremely active |
| [coaidev/coai](https://github.com/coaidev/coai) | Multi-tenant AI one-stop platform (chat-nio successor) | Go+TS · 9.3k★ · pushed 2026-03-12 · slowed |
| [MartialBE/one-hub](https://github.com/MartialBE/one-hub) | one-api fork with per-channel proxies, MCP, payments | Go · 2.9k★ · pushed 2026-02-19 · bursty |
| [AmoyLab/Unla](https://github.com/AmoyLab/Unla) | Go MCP gateway converting REST/gRPC/WS into MCP servers | Go/TS · 2.25k★ · pushed 2026-10-20 · active |
| [ENTERPILOT/GoModel](https://github.com/ENTERPILOT/GoModel) | Go LiteLLM/Portkey alternative with caching/guardrails/MCP | Go · ~1.3k★ · pushed 2026-11-04 · very active |
| [zhu327/gemini-openai-proxy](https://github.com/zhu327/gemini-openai-proxy) | Minimal OpenAI→Gemini adapter (deprecated by native endpoint) | Go · 720★ · pushed 2025-10-07 · instructive-dead |
| [ferro-labs/ai-gateway](https://github.com/ferro-labs/ai-gateway) | Go gateway: 30 providers, 8 routing strategies, plugin guardrails | Go · 275★ · pushed 2026-11-02 · active |
| ✦ [yolorouter/yolorouter](https://github.com/yolorouter/yolorouter) | Go coding-CLI gateway with key pools and cost transforms | Go · 254★ · pushed 2026-10-28 · active |
| [EinStack/glide](https://github.com/EinStack/glide) | Cloud-native Go LLM gateway with router types | Go · 160★ · pushed 2024-08-12 · dormant |
| [openziti/llm-gateway](https://github.com/openziti/llm-gateway) | Zero-trust OpenAI-compatible proxy w/ semantic model routing | Go · 104★ · pushed 2026-10-14 · active |
| [KenyonY/openai-forward](https://github.com/KenyonY/openai-forward) | asyncio forwarding proxy archetype with cache + rate limits | Python · ~985★ · pushed 2025-03 · unmaintained |
| [Nya-Foundation/NyaProxy](https://github.com/Nya-Foundation/NyaProxy) | Provider-agnostic key-pool + LB policy gateway | Python · ~983★ · pushed 2026-07 · active |
| ✦ [mozilla-ai/otari](https://github.com/mozilla-ai/otari) | Mozilla.ai self-hosted gateway (virtual keys, budgets, MCP) | Python · ~834★ · pushed 2026-11-04 · very active |
| [NadirRouter/NadirClaw](https://github.com/NadirRouter/NadirClaw) | Complexity-routing cost optimizer (PolyForm NC) | Python · ~661★ · pushed 2026-10-22 · active |
| [aws-samples/bedrock-access-gateway](https://github.com/aws-samples/bedrock-access-gateway) | OpenAI⇄Bedrock Converse translator — deprecated for native endpoints | Python · ~1003★ · pushed 2026-09 · deprecated |
| [0xzr/freellmpool](https://github.com/0xzr/freellmpool) | Free-tier catalog + health-checked route pool | Python · ~144★ · pushed 2026-10-30 · active |
| [Nayjest/lm-proxy](https://github.com/Nayjest/lm-proxy) | Minimal FastAPI multi-provider gateway with virtual keys | Python · ~157★ · pushed 2026-11-03 · maintained |
| [Inebrio/Routerly](https://github.com/Inebrio/Routerly) | Node decision-and-control layer with explainable routing, RBAC | TypeScript · ~107★ · pushed 2026-11-04 · active |
| [youssefvdel/qwengate](https://github.com/youssefvdel/qwengate) | Bun gateway exposing chat.qwen.ai accounts as OpenAI API | TypeScript · ~209★ · pushed 2026-08 · gray-zone pattern ref |
| ✦ [agiwhitelist/tokdiet](https://github.com/agiwhitelist/tokdiet) | Local token-metering + context-compaction proxy with A/B proof | TypeScript · ~32★ · pushed 2026-06 · dormant |
| [blue-pen5805/llm-proxy-on-cloudflare-workers](https://github.com/blue-pen5805/llm-proxy-on-cloudflare-workers) | Serverless multi-provider proxy w/ credential pools, virtual models | TypeScript · ~88★ · pushed 2026-10-21 · active |
| [zuisong/gemini-openai-proxy](https://github.com/zuisong/gemini-openai-proxy) | Gemini-behind-OpenAI across Deno/Workers/Bun/Node runtimes | TypeScript · ~433★ · pushed 2026-04-02 · slow-moving |
| [CassiopeiaCode/CosyRedactGateway](https://github.com/CassiopeiaCode/CosyRedactGateway) | Cloudflare Worker privacy gateway (Gitleaks-compatible + entropy, reversible) | JavaScript · ~106★ · pushed 2026-10-31 · active |
| [hbsgithub/deno-azure-openai-proxy](https://github.com/hbsgithub/deno-azure-openai-proxy) | Single-file Deno OpenAI→Azure rewriter | TypeScript · ~139★ · pushed 2024-03-23 · dormant |
| [dext7r/ollama-api-pool](https://github.com/dext7r/ollama-api-pool) | Workers proxy pool for Ollama APIs (KV/Postgres/Redis stores) | JavaScript · ~26★ · pushed 2026-01-21 · idle |
| [kasuboski/openai-gateway](https://github.com/kasuboski/openai-gateway) | Hono Worker proxying via AI Gateway; providers from KV + secrets | TypeScript · ~2★ · pushed 2025-04-29 · dormant pattern ref |
| [Jhongwe1/edge-llm-gateway](https://github.com/Jhongwe1/edge-llm-gateway) | Single-Worker full-stack gateway (D1 + Durable Objects + cron) | TypeScript · ~2★ · pushed 2026-08-06 · small, instructive |
| [ShirokamiRyzen/Neko-router](https://github.com/ShirokamiRyzen/Neko-router) | Bun+Elysia gateway: response cache, token telemetry, key quotas | TypeScript · ~5★ · pushed 2026-11-03 · active |
| ✦ [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) | Flagship agent control plane (rules, pools, ToolHub MCP) | TypeScript · ~38k★ · pushed 2026-10-10 · active |
| [lidge-jun/opencodex](https://github.com/lidge-jun/opencodex) | Own the agent model picker; auto-configures agent clients | TypeScript · ~18.2k★ · pushed 2026-11-04 · very active |
| [ericc-ch/copilot-api](https://github.com/ericc-ch/copilot-api) | Copilot-as-OpenAI/Anthropic adapter (namesake pointer) | TypeScript · ~4.15k★ · pushed 2025-11-10 · stalled |
| [BenedictKing/ccx](https://github.com/BenedictKing/ccx) | Go protocol-translation gateway with console, channels, failover | Go · ~3.98k★ · pushed 2026-11-03 · very active |
| [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) | Python Anthropic-endpoint emulation over LiteLLM | Python · ~3.77k★ · pushed 2026-06-23 · slow-moving |
| ✦ [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy) | Rust Anthropic-compatible proxy over subscription upstreams | Rust · ~694★ · pushed 2026-10-31 · active |
| [jianshuo/ccglass](https://github.com/jianshuo/ccglass) | Logging proxy + dashboard inspecting agent traffic | JavaScript · ~893★ · pushed 2026-10-11 · active |
| [luohy15/y-router](https://github.com/luohy15/y-router) | Edge Anthropic→OpenAI translator (archived; superseded upstream) | TypeScript · ~383★ · archived 2026-01-11 · instructive-dead |
| [m0n0x41d/anthropic-proxy-rs](https://github.com/m0n0x41d/anthropic-proxy-rs) | Rust Anthropic→OpenAI-compat proxy (thinking/tools/SSE) | Rust · ~94★ · pushed 2026-05-29 · modest |
| [achetronic/claude-oauth-proxy](https://github.com/achetronic/claude-oauth-proxy) | Go OAuth PKCE token manager for Anthropic Teams/Enterprise | Go · ~10★ · archived 2026-03-18 · instructive-dead |
| [PublicAffairs/openai-gemini](https://github.com/PublicAffairs/openai-gemini) | Serverless Gemini-behind-OpenAI incl. `:search` suffix, extra_body hatch | JavaScript · ~3.67k★ · pushed 2026-03-17 · maintained |
| [stulzq/azure-openai-proxy](https://github.com/stulzq/azure-openai-proxy) | Go OpenAI→Azure translator (path/auth/api-version rewriting) | Go · ~1.33k★ · pushed 2024-06-25 · dormant |
| [yasuoiwakura/openai-ollama-api-bridge](https://github.com/yasuoiwakura/openai-ollama-api-bridge) | Python OpenAI→Ollama-native bridge with queue mode + failover | Python · 3★ · pushed 2026-09-21 · tiny but thoughtful |
| ✦ [NoahStepheno/completion-to-response](https://github.com/NoahStepheno/completion-to-response) | Go Responses-over-ChatCompletions synthesis proxy | Go · 14★ · archived 2026-08-16 · dead-by-choice |
| [dongfangzan/local-openai2anthropic](https://github.com/dongfangzan/local-openai2anthropic) | Bidirectional Anthropic⇄OpenAI translator daemon (OA2A) | Python · 21★ · pushed 2026-06-30 · active |
| [unclecode/antomix](https://github.com/unclecode/antomix) | Anthropic↔~12 providers with in-session $$ commands, fan-out | JavaScript · 27★ · pushed 2025-08-10 · early-stage |
| [ulab-uiuc/LLMRouter](https://github.com/ulab-uiuc/LLMRouter) | De-facto RouteLLM successor: 16+ routers, xRouteBench | Python · ~3.1k★ · pushed 2026-10-19 · active research |
| [Writesonic/GPTRouter](https://github.com/Writesonic/GPTRouter) | Universal-API gateway with health-check fallback | TypeScript · ~454★ · pushed 2024-04 · dormant |
| [anyscale/llm-router](https://github.com/anyscale/llm-router) | Cost/quality routing methodology + paper (arXiv 2406.18665) | Python · ~261★ · pushed 2024-07 · dormant, cited |
| [stanford-futuredata/FrugalGPT](https://github.com/stanford-futuredata/FrugalGPT) | Pioneer of budget-constrained LLM cascades | Notebook · ~291★ · pushed 2025-02 · dormant idea repo |
| [ulab-uiuc/GraphRouter](https://github.com/ulab-uiuc/GraphRouter) | ICLR 2025 GNN router over tasks/queries/LLMs | Python · ~83★ · pushed 2025-12-30 · paper-complete |
| [ulab-uiuc/Router-R1](https://github.com/ulab-uiuc/Router-R1) | NeurIPS 2025 RL-trained multi-round router/aggregator | Python · ~155★ · pushed 2025-12-30 · paper-complete |
| [thu-nics/TokenRouter](https://github.com/thu-nics/TokenRouter) | NeurIPS 2026 token-level routing system | Python · ~75★ · pushed 2026-10-20 · active |
| [MilkThink-Lab/RouterEval](https://github.com/MilkThink-Lab/RouterEval) | EMNLP 2025 routing benchmark (200M+ records, 8.5k LLMs) | Python · ~125★ · pushed 2026-06-03 · maintained |
| [RouteWorks/RouterArena](https://github.com/RouteWorks/RouterArena) | Open router evaluation platform + live leaderboard | Python · ~155★ · pushed 2026-10-31 · active |
| [eclipse-lmos/lmos-router](https://github.com/eclipse-lmos/lmos-router) | Eclipse LMOS agent routing via declarative specs + embeddings | Kotlin · ~42★ · pushed 2026-11-03 · active |
| ✦ [deanxv/done-hub](https://github.com/deanxv/done-hub) | one-hub fork with agent-CLI OAuth channels, quota-ledger fixes | Go · ~815★ · pushed 2026-10-27 · active |
| ✦ [Wei-Shaw/sub2api](https://github.com/Wei-Shaw/sub2api) | Subscription-to-API multi-tenant gateway (design ref; ToS-flagged) | Go · ~45.4k★ · pushed 2026-11-04 · extremely active |
| [cita-777/metapi](https://github.com/cita-777/metapi) | Meta-aggregation of one-api-family panels with balance-aware routing | TypeScript · ~3.34k★ · pushed 2026-09-06 · active |
| [VoAPI/VoAPI](https://github.com/VoAPI/VoAPI) | Commercialized one-api distribution panel (multi-currency billing) | Go · ~1.09k★ · pushed 2026-01-27 · lightly maintained |
| [stevessr/Veloera](https://github.com/stevessr/Veloera) | new-api fork — canonical dissolve-into-upstream dead fork | Go · moved · pushed 2026-03-04 · unmaintained |
| [akl7777777/shell-api](https://github.com/akl7777777/shell-api) | one-api fork turned freemium/single-user aggregator | Go · ~152★ · pushed 2024-11-05 · dormant |
| [RixAPI/Rix-API](https://github.com/RixAPI/Rix-API) | Closed-source New API rewrite; docs-only public repo | JS/Go · ~63★ · pushed 2026-09-15 · product closed-source |
| ✦ [zilliztech/GPTCache](https://github.com/zilliztech/GPTCache) | Seminal semantic-cache library (five-stage taxonomy) | Python · ~8.2k★ · pushed 2026-09-22 · maintenance-mode |
| [codefuse-ai/ModelCache](https://github.com/codefuse-ai/ModelCache) | Ant Group production semantic cache (multi-tenant Redis recall) | Python · ~930★ · pushed 2025-06-30 · dormant |
| [redis/redis-vl-python](https://github.com/redis/redis-vl-python) | Official RedisVL SemanticCache extension | Python · ~430★ · pushed 2026-11-04 · active |
| ✦ [messkan/prompt-cache](https://github.com/messkan/prompt-cache) | Go semantic cache proxy with gray-zone judge | Go · ~425★ · pushed 2026-08-18 · quiet |
| [upstash/semantic-cache](https://github.com/upstash/semantic-cache) | Upstash fuzzy KV on Vector (clean API design; cloud-tied) | TypeScript · ~300★ · pushed 2024-11-21 · dormant |
| [kushals256/jevcache](https://github.com/kushals256/jevcache) | MorrowCache: local semantic cache for OpenAI-compat APIs | TypeScript · ~12★ · pushed 2026-11-04 · young/active |
| ✦ [os-tack/ostk-cache](https://github.com/os-tack/ostk-cache) | Rust prompt-cache hit-rate engineering proxy | Rust · ~10★ · pushed 2026-06-12 · dormant; private deps |
| [rohanarun/computer-use-cache](https://github.com/rohanarun/computer-use-cache) | Replay cache for computer-use/agent trajectories | JavaScript · ~47★ · pushed 2026-09-20 · active |
| [LMCache/LMCache](https://github.com/LMCache/LMCache) | KV-cache management layer for vLLM/SGLang | Python/C++ · ~12.3k★ · pushed 2026-11-04 · very active |
| [kvcache-ai/Mooncake](https://github.com/kvcache-ai/Mooncake) | Kimi's disaggregated KVCache platform (FAST '25) | C++ · ~7k★ · pushed 2026-11-04 · very active |
| [protectai/llm-guard](https://github.com/protectai/llm-guard) | Security toolkit: input/output scanner taxonomy (archived) | Python · ~3.2k★ · archived 2026-07 · canonical-dead |
| [guardrails-ai/guardrails](https://github.com/guardrails-ai/guardrails) | Input/Output guards w/ Hub validators, re-ask repair loop | Python · ~7.6k★ · pushed 2026-11-03 · active |
| [NVIDIA-NeMo/Guardrails](https://github.com/NVIDIA-NeMo/Guardrails) | Colang programmable guardrails (flows, rails) | Python · ~7.4k★ · pushed 2026-11-04 · very active |
| [protectai/rebuff](https://github.com/protectai/rebuff) | Self-hardening injection detector (archived; canary pattern) | TypeScript · ~1.5k★ · archived 2024-08 · dead, instructive |
| [deadbits/vigil-llm](https://github.com/deadbits/vigil-llm) | Ensemble scanner library + REST API (YARA, vector DB, canaries) | Python · ~509★ · pushed 2024-01-31 · dead, instructive |
| [data-privacy-stack/presidio](https://github.com/data-privacy-stack/presidio) | De-facto PII engine (analyzer/anonymizer/image/structured) | Python · ~11.7k★ · pushed 2026-11-02 · community-governed |
| [StackOneHQ/defender](https://github.com/StackOneHQ/defender) | Injection defense at tool-result ingestion (22 MB ONNX CPU model) | TypeScript · ~132★ · pushed 2026-11-04 · very active |
| [InditexTech/cerbia](https://github.com/InditexTech/cerbia) | Security gates with multi-layer de-obfuscation before scanning | Python · ~46★ · pushed 2026-11-04 · active |
| [meta-llama/PurpleLlama](https://github.com/meta-llama/PurpleLlama) | LlamaFirewall: PromptGuard 2, CodeShield, AlignmentCheck | Python · ~4.5k★ · pushed 2026-10-30 · active |
| [occludra/gateway](https://github.com/occludra/gateway) | OpenAI-SDK security proxy chaining Presidio+secrets+injection, fail-closed | Python · ~49★ · pushed 2026-08-16 · young |
| [AgentOps-AI/agentops](https://github.com/AgentOps-AI/agentops) | Agent telemetry SDK: session replays, cost tracking | Python · ~5.9k★ · pushed 2026-06-25 · semi-active |
| [samanhappy/mcphub](https://github.com/samanhappy/mcphub) | MCP hub: aggregated SSE/Streamable/stdio endpoints + pgvector routing | TypeScript · ~2.6k★ · pushed 2026-11-03 · active |
| [metatool-ai/metamcp](https://github.com/metatool-ai/metamcp) | MCP meta-server: namespacing, middleware, multi-tenancy | TypeScript · ~2.8k★ · pushed 2026-06-22 · moderately active |
| [sparfenyuk/mcp-proxy](https://github.com/sparfenyuk/mcp-proxy) | De-facto MCP transport bridge (stdio↔SSE/Streamable HTTP) | Python · ~2.8k★ · pushed 2026-11-04 · active |
| [1mcp-app/agent](https://github.com/1mcp-app/agent) | Unified MCP runtime/hub with progressive tool discovery | TypeScript · ~527★ · pushed 2026-11-03 · active |
| [burugo/one-mcp](https://github.com/burugo/one-mcp) | MCP reverse-proxy platform + Anthropic Skills export | Go · ~422★ · pushed 2026-09-06 · maintained |
| [smart-mcp-proxy/mcpproxy-go](https://github.com/smart-mcp-proxy/mcpproxy-go) | Meta-tool MCP proxy with tool quarantine + SARIF scanners | Go · ~407★ · pushed 2026-11-03 · active |
| [ip2a/mcpstore](https://github.com/ip2a/mcpstore) | Rust MCP manager: control plane + data plane, per-agent scoping | Rust · ~433★ · pushed 2026-11-04 · active |
| ✦ [MikkoParkkola/mcp-gateway](https://github.com/MikkoParkkola/mcp-gateway) | Meta-tool MCP gateway with security primitives (PolyForm NC) | Rust · ~85★ · pushed 2026-11-02 · very active |
| [open-webui/mcpo](https://github.com/open-webui/mcpo) | Inverse proxy: MCP servers as OpenAPI/REST endpoints | Python · ~4.4k★ · pushed 2026-05-17 · maintained |
| [mudler/LocalAI](https://github.com/mudler/LocalAI) | Self-hosted OpenAI/Anthropic-compatible AI engine | Go · 50.2k★ · pushed 2026-11-04 · flagship |
| [mostlygeek/llama-swap](https://github.com/mostlygeek/llama-swap) | Zero-dep model hot-swap proxy for local servers | Go · 6.18k★ · pushed 2026-11-04 · very active |
| [theroyallab/tabbyAPI](https://github.com/theroyallab/tabbyAPI) | ExLlamaV3 OpenAI-compatible backend (hobby-grade) | Python · 1.51k★ · pushed 2026-11-04 · active |
| [abetlen/llama-cpp-python](https://github.com/abetlen/llama-cpp-python) | Ubiquitous OpenAI-compatible llama.cpp shim (slowing) | Python · 10.7k★ · pushed 2026-10-01 · slowed |
| [oobabooga/textgen](https://github.com/oobabooga/textgen) | Major llama.cpp frontend, now dual-protocol local server | Python · 47.7k★ · pushed 2026-08-17 · active |
| [janhq/jan](https://github.com/janhq/jan) | Offline ChatGPT replacement w/ Rust core + :1337 OpenAI API | Rust · 45.4k★ · pushed 2026-11-04 · very active |
| [mozilla-ai/llamafile](https://github.com/mozilla-ai/llamafile) | Single-file LLM distribution (Cosmopolitan) | C++ · 26.6k★ · pushed 2026-11-04 · active |
| ✦ [EricLBuehler/mistral.rs](https://github.com/EricLBuehler/mistral.rs) | Rust inference engine, dual-protocol serve | Rust · ~7.8k★ · pushed 2026-11-03 · active |
| [inference-gateway/inference-gateway](https://github.com/inference-gateway/inference-gateway) | Cloud-native Go gateway with MCP + A2A + OPA guardrails | Go · 217★ · pushed 2026-11-04 · daily commits |
| [LostRuins/koboldcpp](https://github.com/LostRuins/koboldcpp) | Single-executable local inference, many wire compatibilities | C++ · 12.1k★ · pushed 2026-11-04 · very active |
| [cuihuan/awesome-ai-gateway](https://github.com/cuihuan/awesome-ai-gateway) | 240+-gateway meta-list: scorecard, benchmarks, handbook, datasets | Markdown · ~150★ · pushed 2026-11-04 · active |
| [ai-gateways/awesome-ai-gateways](https://github.com/ai-gateways/awesome-ai-gateways) | Vendor-neutral taxonomy incl. aisix and llmgateway yields | Markdown · ~29★ · pushed 2026-11-03 · active |
| [howardpen9/awesome-ai-api-proxy](https://github.com/howardpen9/awesome-ai-api-proxy) | Relay-station gray market catalog + weekly pricing intelligence | HTML/data · ~102★ · pushed 2026-11-01 · maintained |
| [tensorchord/Awesome-LLMOps](https://github.com/tensorchord/Awesome-LLMOps) | Highest-traffic LLMOps list (bitrouter, shim yields) | Markdown · ~6k★ · pushed 2026-10-16 · active |
| [yenanjing/awesome-model-routing](https://github.com/yenanjing/awesome-model-routing) | Routing-first collection (AIClient2API, ClawRouter yields) | Markdown · ~35★ · pushed 2026-11-03 · active |
| [talhaanwarch/awesome-llmops-platforms](https://github.com/talhaanwarch/awesome-llmops-platforms) | Self-hostable-only catalog, weekly CI refresh, honesty badges | Markdown · ~39★ · pushed 2026-10-26 · active |
| [sihyeong/Awesome-LLM-Inference-Engine](https://github.com/sihyeong/Awesome-LLM-Inference-Engine) | Engine taxonomy incl. gateway section (ferro-labs yield) | Markdown · ~250★ · pushed 2026-10-30 · active |
| [12britz/awesome-ai-gateways](https://github.com/12britz/awesome-ai-gateways) | Capability-organized checklist (semantic cache, cost, guardrails) | Markdown · ~12★ · pushed 2026-07-14 · stable checklist |
| [Router-One/awesome-llm-gateway](https://github.com/Router-One/awesome-llm-gateway) | Managed-router tier + cascade research pointers | Markdown · 0★ · pushed 2026-10-12 · vendor-curated |
| [Ar9av/awesome-agent-control-plane](https://github.com/Ar9av/awesome-agent-control-plane) | Lifecycle framing: identity, policy-at-action, blast radius | Markdown · ~4★ · pushed 2026-08-07 · small |

Corrections welcome via the usual research-note flow; the registry
(`repos-registry.json`) is the machine-readable counterpart of this survey.
