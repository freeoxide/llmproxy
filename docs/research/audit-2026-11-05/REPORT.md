# Research audit, 2026-11-05

Audited at HEAD `79cf406`. This audit changed no source code or existing docs.
Doc keys: GA `competitive-gap-analysis.md`, CO `competitive-landscape-compendium.md`,
NR `niche-repos.md`, RI `repo-intelligence.md`, BL `compensation-backlog.json`,
PR `docs/protocol.md`, CP `core-protocol.md`.

## Verdict

- The sources are real. Every cited repository, issue and pull request exists,
  and no citation was invented.
- Facts about other products are mostly right. The weak spots are "nobody ships
  X" claims, paid-edition features counted as generally available, and facts
  that went stale.
- **Claims about llm-proxy itself are the least reliable part**: 42% fully
  confirmed and 15% false. The build order rests on some of the false ones.
- The demand story ("users demand X in N repos") is much stronger than its
  evidence.
- The docs never assess SDK use or feature conflicts, which are your two stated
  requirements. They also contradict each other, and a locked design decision,
  in ways they don't acknowledge.
- Testing the proxy found serious bugs that no research doc mentions. These come
  before any new feature work.

## Scoreboard

| Area | Checked | Confirmed | Partial | Stale/drift | False | Unverifiable |
|---|---:|---:|---:|---:|---:|---:|
| llm-proxy's own behaviour (all docs) | 201 | 84 | 82 | 2 | 31 | 2 |
| Gateways A: LiteLLM, Portkey, Bifrost, agentgateway, Kong, Agent Router | 73 | 49 | 13 | 2 | 5 | 4 |
| Gateways B: status claims, "nobody ships X" claims, 18 other products | 93 | 69 | 18 | 1 | 3 | 2 |
| Wave 3 niche repos | 101 | 78 | 16 | 3 | 1 | 3 |
| Wave 4 repo intelligence | 187 | 141 | 37 | 3 | 4 | 2 |
| Provider, protocol and endpoint notes | 77 | 43 | 15 | 7 | 0 | 12 |
| **Total** | **732** | **464** | **181** | **18** | **44** | **25** |

These samples targeted the riskiest claims, so the percentages are not an error
rate for the whole corpus. A few claims were found by both gateway groups and
count twice.

Link check, 1,431 unique URLs: 1,413 resolve. The other 18 are 11 HTTP 429s
(Respan rate limiting, so those claims are unverified), one 403 (npm) and six
404s:

- two OpenCode API roots, which is normal (`/models` returns 200)
- `README.MD` casing in rust-llm-proxy (the claims survive)
- antirion's path (should be `blob/main/console/README.md`)
- a dead Helicone mirror
- a mistyped LiteLLM docs URL

All 499 issue and PR URLs plus 98 shorthand references exist. One has the wrong
URL type (one-hub `/issues/927` is a PR).

The workspace test suite passes: 1,322 passed, 0 failed, 3 ignored. It catches
none of the bugs below, and fixture `anthropic/stream_usage/core-events.json`
locks one of them in.

## Bugs found by running the proxy

To reproduce: `evidence/probes/run.sh <path-to-llm-proxy>` runs the real binary
against a fake upstream on loopback, with no real credentials. No live provider
was contacted. The Anthropic stream replays the documented event shape.

| # | Test | Result | Where | What the docs say |
|---|---|---|---|---|
| 1 | Start the server and wait | **Exits with status 1 at 30 s, no signal, under the default config.** Logs "graceful-shutdown deadline elapsed". The shutdown timeout wraps the whole server run. Setting `shutdown_timeout = "0s"` avoids it (tested: still up at 36 s, clean SIGTERM exit) | `apps/llm-proxy/src/commands/serve.rs:99,134-149` | GA:87 lists graceful shutdown as baseline |
| 2 | Anthropic marker with `ttl: "1h"` on a system or text block | **The whole block disappears, HTTP 200.** The system prompt or user text is gone; only a WARN is logged | `protocol/src/anthropic.rs:112`, `client/anthropic.rs:101`, `protocol/src/anthropic.rs:174-186` | GA:285 says only that 1h is "unrepresentable" |
| 3 | Upstream 307 to another port | **`x-api-key` and the body follow the redirect.** Bearer is stripped. Discovery has a safe policy; the main client doesn't. `x-goog-api-key` shares the code path (not run) | `provider/src/transport.rs:286` | BL #10 is the only mention, bundled with moot items |
| 4 | Documented Anthropic stream | **Client and ledger get 0/0 usage, and `max_tokens` becomes `end_turn`.** Cache tokens are lost | `protocol/src/anthropic.rs:631`, `adapter/anthropic.rs:337-361` | GA:289 says cache columns land |
| 5 | Messages→chat route, streaming | Sends `include_usage: false`, so usage is always 0 and the ledger row is `synthetic_zero`. Budgets built on this would undercount | `adapter/openai_chat.rs:557-568` | CO:805 "ledger already records settled cost" |
| 6 | OpenAI client with `include_usage: true` | Client gets zeros while the ledger has 100/5 | `client/openai_chat.rs:671-682` | Not mentioned |
| 7 | Upstream closes a stream early | Client gets `finish_reason: stop` then `[DONE]`, or `end_turn`. Ledger records success | `client/openai_chat.rs:685`, `core_pipeline.rs:1372-1396` | CP:71-72 calls transport termination a distinct terminal state. PR:146 and a test deliberately finalize the stream, but nothing marks it as failed |
| 8 | OpenAI client sends 17 fields | **11 dropped with HTTP 200**: `response_format`, `parallel_tool_calls`, `seed`, `n`, `logprobs`, `logit_bias`, `frequency_penalty`, `presence_penalty`, `service_tier`, `prompt_cache_key`, `store` | `client/openai_chat.rs:249-252`, `adapter/openai_chat.rs:554` | PR:158-164 requires a warning or rejection |
| 9 | Anthropic client fields | **HTTP 400 from the proxy** for `stop_sequences`, `top_k`, `service_tier`, `output_config`, tools with `cache_control`, and server tools | `protocol/src/anthropic.rs:9-44,577` | GA:213 says field coverage was not audited |
| 10 | Message-level `cache_control` | Dropped on Anthropic→Anthropic. Only system prompts keep markers, so conversation caching can't work through the proxy | `adapter/anthropic.rs:640,825` | GA:286 "no adapter translates"; CO:825 "only missing field" |
| 11 | `tool_result` marker | Silently dropped; the core has no field for it | `protocol/src/core.rs:174` | As row 10 |
| 12 | Client sends `anthropic-beta`, session headers | No inbound header reaches upstream (passthrough mode forwards only the token), so beta features can't be enabled per request | `provider/src/transport.rs:40-52` | BL #10 proposes stripping headers that are never forwarded |
| 13 | DeepSeek-style usage | `prompt_tokens: 0`, and uncached input is booked as cache creation. Input cost is $0 unless `cache_creation` is priced | `adapter/mod.rs:347-365` | RI R2 says fed "only from Anthropic dialects" |
| 14 | Identical requests | Blocked for 500 ms from first sighting only, not while in flight. Not scoped by caller. Off by default | `server/src/middleware.rs:58-83`, `core/src/provider_config.rs:273` | GA:95,165 "rejects identical in-flight"; "origin protection exists" |

Found by the code audit and not re-run by me (details in
`evidence/llm-proxy-claims.md` section F):

- Passthrough plus client keys sends the proxy's client key upstream (`core_pipeline.rs:403-417`).
- Streaming ledger rows lose `model_upstream` under aliases.
- Responses and Gemini decoders mishandle the documented tool-call shapes. Default templates don't route to them, and these shapes come from API knowledge rather than a live run.
- `tool_choice: "auto"` is encoded as an object.
- `reasoning_effort` and `thinking` are forwarded unfiltered.

## False or misleading claims that affect decisions

**About llm-proxy**

- GA:382-391, CO:800: "fallback slots in" at the first-byte window. 429, 5xx and
  connect errors return earlier at `send_stream`, and the window closes on a
  role-only or `message_start` frame, before any visible output.
- CO:822-824: credential pools extend `api_key` "without changing request
  handling". They require selection and feedback in the pipeline plus a new
  ledger column.
- CO:805: "enforcement is a counter over" the ledger. Streaming cost is often
  NULL (rows 4 and 5). The ledger is also optional and write-only from the server.
- GA:419-427: the cache done-condition contradicts itself. Dedup runs first and
  would answer 409 where the condition requires a HIT.
- GA:320-324, CO:607, RI:697: rustls only. reqwest default features are on, so
  native-tls and OpenSSL are compiled too.
- GA:105, CO:182: "musl single-binary releases". The repo has zero GitHub
  releases, and `openssl-sys` is unvendored, so the musl build is unproven.
- CO:669, NR:29: "cancellation gap". Upstream cancellation already propagates
  once streaming. What's missing is usage settlement and a row for disconnects
  before the first event.

**About other products**

- GA:279: upstream mTLS "exist only at agentgateway". LiteLLM documents a client
  certificate; Kong's Service entity has `client_certificate` (not tested through
  its AI plugins).
- GA:303: "no successor-model mapping ships". LiteLLM's cited file defines
  `successor_model`.
- GA:316: no product has in-gateway config history or rollback. Cloudflare dynamic
  routes offer versions with instant rollback; Azure APIM has revisions.
- GA:194, GA:367: LiteLLM `cancel_on_disconnect` is documented as non-streaming,
  so it doesn't support gap #20 as written.
- GA:161: Cloudflare is "the only budget→fallback". LiteLLM has provider budget routing.
- GA:201: MCP 2026-07-28 is called a release candidate, but it is final.
- GA:399-401: reservation is rejected citing LiteLLM #35524. That issue closed
  2026-10-08 with a linked fix, so the reason is stale.
- GA:178: Cloudflare hazards S1–S13 are now S1–S14.

**Paid-edition or beta features counted as generally available**

- Kong: `ai-proxy-advanced`, `ai-rate-limiting-advanced`, `ai-semantic-cache`,
  `ai-sanitizer`, `ai-mcp-proxy` require an AI license.
- Portkey: budgets, key rotation, secret references and KMS are enterprise or
  hosted. The OSS cache is exact-match in memory only.
- LiteLLM: audit logs are Enterprise.
- Tyk: upstream OAuth requires Enterprise.
- Cloudflare: Guardrails, DLP, Auto Router and spend limits are Beta.

The prevalence counts (gap #1 "22 products", and so on) add up correctly but
blend these together. Read them as "someone sells this", not "self-hostable
parity".

**Wave 3 and 4 citations**

- RI:371 `sub2api/pull/7239` is about a promo-code field. The description belongs
  to a CLIProxyAPI PR.
- RI:2397 `sub2api/issues/8508` is about Claude Ultra quota, not model sync.
- RI:1050 `prompt-cache/issues/16` "unanswered ~3 months": the owner replied in days.
- RI:2633 otari #2792 is about budget names, not session attribution.
- NR:348 Plano's CLI is Python, not TypeScript.
- RI:621 CCR #1923 says failover is unclear; the doc says it's absent.
- R1 says 12 repos but names 13.

**Provider notes**

- `providers.md:160-172`: OpenCode now documents `opencode.ai/inference/...`. The
  legacy `/zen` paths still answer, and `providers/*.toml.example` use only those.
- `providers.md:105`, CP:159: OpenRouter `usage.include` is deprecated with no effect.
- CP:111: forced `tool_choice` as a structured-output route now returns 400 on
  newer Anthropic models.
- `endpoints.md:109`: Anthropic Files no longer needs a beta header.
- `providers.md:39`: `guarded_text` is LiteLLM's name; AWS uses `guardContent`.
- Many CONFIRMED rows rest on LiteLLM's source alone, and nothing in these three
  docs is dated.
- Missing current behaviour:
  - Anthropic's 4-breakpoint limit and `ttl`
  - Anthropic's newer 400s on `temperature`, prefill and `thinking` by model
  - OpenAI `prompt_cache_key`
  - the Azure v1 path

## How strong the demand evidence is

- Of 261 cited issues, 114 have no comments and 238 have no reactions. Only 5 have 3 or more reactions.
- 26 of the 40 sweep repos aren't LLM gateways (engines, guardrail libraries, MCP
  servers, a benchmark, a list). This isn't disclosed. R9 is 4/6 guardrail
  libraries; R12 is 4/4 MCP gateways.
- One account (`roy-tong`, 94 items across repos in an "AgentMeasure" campaign)
  filed AgentOps #1445, Routerly #144 and tokdiet #1. R2 and R7 count these as
  independent demand.
- Some citations are vendor pitches (mistral.rs #2490, ThinkWatch #113, antirion
  #41) or self-described AI-written reports (koboldcpp #2480/#2481, raine #163).
- R-section header says "demanded by users", but R4, R6, R7, R8, R10 and R11 have
  0 or 1 user-filed issues.
- "Most bug-dense" and "dominant" rest on the latest 100 items per repo. R6's
  "fastest-growing upstream segment" has no source.
- Several "researched in full" repos are very young, and much of the code is
  AI-written:
  - xLB: created 2026-10-14, 38 commits
  - Antirion: created 2026-10-17
  - preflight: created 2026-09-20, 5 commits
  - tokdiet: 32 stars
  - ostk-cache: 10 stars, not buildable
  - completion-to-response: 6 commits, archived

  The doc flags most of these. It doesn't flag yolorouter or mcp-gateway.
- Vendor numbers are stated as fact in places, such as mcp-gateway's "~8 ms hot
  reload".

Use these docs as a catalogue of possibilities, not a ranking of demand.

## Backlog review

38 candidates; 22 need correction. Line-level detail: `evidence/llm-proxy-claims.md` section G.

| # | Problem | Correction |
|---|---|---|
| 2 | The chat error frame is swallowed before this logic could help | Fix the decoder first |
| 3 | DeepSeek fields also feed these columns, wrongly | Rescope; include the mapping fix |
| 5 | Propagation exists | Settlement and pre-first-event row only |
| 6 | Contradicts CO:709 on reservation; wrong crate | Pick one admission model (below); server + storage |
| 7 | No `Accept-Encoding` is sent; provenance column exists | Inject usage, extend provenance |
| 8 | Four phases exist; they just aren't configurable | Make configurable; add a total deadline |
| 10 | Stripping is moot, since nothing is forwarded | Keep the redirect fix as urgent; add a forwarding allowlist |
| 11, 25 | Non-loopback refusal already exists (`provider_config.rs:1650`); the two overlap | Keep Host/Origin validation; merge |
| 12 | "High" with no mention of vendor terms | Your decision, see Q4 |
| 13, 15 | Wrong crate | Provider edge, per PR:166-168 |
| 14 | Heartbeats exist after the first event | Rescope to pre-commit silence and terminal usage |
| 18, 22, 24, 37 | Rationale is partly wrong | Fix the text |
| 27 | In-flight rows already return | Add state and elapsed time |
| 28 | The proxy itself masks truncation (row 7), and a test requires the synthetic terminal | Raise to high; keep valid framing but mark the failure |
| 29, 33 | No reassembly exists; retention is proven and there are no sidecars | Drop |
| 38 | Depends on #1 | Sequence after #1 |

**Missing from the backlog entirely**

- the 30-second exit
- deletion of `ttl` blocks
- request-field fidelity (rows 8 and 9)
- message-level cache markers
- Anthropic stream usage and stop reason
- an inbound header allowlist (`anthropic-beta`)
- a same-dialect passthrough path
- contract tests from real client traffic
- client-key lookup that scales
- extracting a request engine out of the HTTP layer

13 of the 38 are mapped to `llm-proxy-server`, where the pipeline is private and
tied to axum. Built there, they can't reach SDK users.

## Feature conflicts

Nothing in the docs assesses these. "All features" works only if each is settled
once, in one place.

| # | Features | Conflict | Recommended resolution |
|---|---|---|---|
| 1 | URL-only provider vs fallback, load balancing, virtual models, cache affinity | `remodel-proxy.md:43-60` locks URL-only routing and no bare `/v1/*`. CP:177 says aliases must not change provider. Source-guard tests ban fallback imports (`routes/messages.rs:122`). The research ranks fallback #1 and never mentions this | **Virtual providers**: `/providers/{pool}/v1/...`, where a pool is configured with members. Keeps the locked rule. See Q1 |
| 2 | Admission: check-then-accrue (CO:709), USD reservation (BL #6), TPM reserve-then-reconcile | Three models for one question, and the stated reason against reservation is stale | One `Admission` step: reserve, then settle or refund. Unestimatable cost goes to an explicit policy |
| 3 | Pre-output failover (BL #2) vs heartbeats (BL #14) vs TTFT | A heartbeat commits HTTP 200, after which no status can be returned or provider switched | Per-route hold budget: buffer up to N seconds, then commit and heartbeat; in-band errors only after that |
| 4 | Dedup vs cache vs idempotency (BL #24) vs retries | Dedup runs first on raw bytes, is caller-blind, and answers 409. Stock SDKs retry 409 | Retire it. Add per-tenant single-flight plus optional `Idempotency-Key`. Cache lookup comes first |
| 5 | Prompt-cache stability vs anything that mutates requests | Normalization, marker injection, redaction, payload compaction and re-encoding all change prefixes | Ordered mutation phases, and a rule: nothing changes before the last breakpoint without opt-in |
| 6 | Credential rotation, load balancing, fallback vs session state | Rotation loses cache warmth. Signed thinking, encrypted reasoning, file and response IDs bind to one account or provider | Routing takes an affinity input. Failover must strip state or refuse |
| 7 | Response/semantic cache, payload logs, shadow eval vs "digests only" ledger | The current privacy stance can't hold with these on. ThinkWatch's shared cache shows the cross-tenant risk | Off by default, tenant-scoped keys, and a stated data-class policy. See Q5 |
| 8 | Budgets vs cache hits and retries | "Zero recorded usage" on a hit vs budget accounting; failed attempts still cost money | Attempt-level ledger table; a defined rule for what counts |
| 9 | Streaming guardrails vs latency and restore | Redaction placeholders split across chunks; tool arguments arrive as fragments; agentgateway rejects rather than masks on streams | Per-guard mode: pre-call, buffered, or post-hoc. Explicit fail-open/closed |
| 10 | Normalized hub vs full provider features | Anthropic rejects unknown fields; OpenAI drops them. Both break clients | Same-dialect passthrough with surgical edits. Cross-dialect goes through a capability policy that warns or rejects. See Q6 |
| 11 | Static TOML vs key API, budgets, hot reload, admin UI, SDK config | `deny_unknown_fields`, restart-only, secrets in files. No record store exists for "the key record" | `ConfigSource` trait, immutable snapshot swap, one stated precedence |
| 12 | Argon2 keys vs key lifecycle at scale | Cache miss tries every key in sequence (`middleware.rs:475-490`). 1,000 keys means about 1,000 argon2 runs per bad token | Key-id prefix plus one verification, or a keyed hash of high-entropy tokens |
| 13 | In-process state vs replicas | Limiter, cooldowns, budgets and metrics are per-process. `llm-proxy-api` opens SQLite directly | Traits with a local default; Redis and Postgres as optional crates. See Q3 |
| 14 | Status codes across features | 402, 408, 409, 429, 503 and `x-should-retry` are decided piecemeal. Stock SDKs auto-retry some, multiplying load | One error policy table in one module |
| 15 | Preflight (BL #4) vs token accuracy | Non-OpenAI models use chars/4, and token counting ignores tools, tool results and images. Hard rejects would have false positives | Warn by default; hard reject only with a margin or a provider count |
| 16 | Chat-only pipeline vs endpoint families | PR says each family has its own core, but `core_pipeline.rs` is chat-only. A 32 MiB limit and `TimeoutLayer` don't fit audio, files or WebSockets | Generic admission, routing and accounting around family-specific handlers |
| 17 | MCP gateway vs stateless translator | Different auth model, sessions, and multi-step flows | Separate optional crate that shares admission and accounting |
| 18 | Subscription OAuth, fingerprinting (BL #12, #15) vs legitimate deployment | Vendor terms, below | See Q4 |

**Vendor terms on subscription credentials** (the docs mention these only in
passing for a few repos, and BL #12 not at all):

- Anthropic: developers should use API keys. It "does not permit" routing through
  Free, Pro, Max or Ultra credentials on behalf of users, or collecting or
  intermediating Claude.ai credentials. Own API keys are fine, and it reserves
  enforcement without notice.
  https://code.claude.com/docs/en/legal-and-compliance
- Google: third-party software with Gemini CLI OAuth is "a violation of applicable
  terms". https://geminicli.com/docs/resources/tos-privacy/
- OpenAI: recommends API keys for automation, with no explicit proxy statement
  found; the Terms of Use page returned 403. https://learn.chatgpt.com/docs/auth
- GitHub: bars sharing API tokens to exceed rate limits; no explicit Copilot proxy
  rule found. https://docs.github.com/en/site-policy/github-terms/github-terms-of-service

Repos in the survey carry related risk notes or impersonation features: sub2api's
README warns of account bans, CLIProxyAPI has cloaking config, and done-hub sends a
spoofed client version. RI presents these neutrally. This audit reports them only.

## Modularity and SDK readiness

The research never assesses this. "SDK" in GA:260 means competitors' client SDKs.

**Works today**

- `llm-proxy-protocol` is a pure translation library with no network dependencies.
- `llm_proxy_server::build_router(AppState)` is public, so an axum host can mount the proxy.

**Blocks SDK use**

| Blocker | Evidence |
|---|---|
| No non-HTTP entry point. The pipeline is private and returns axum types | `routes/mod.rs:25`, `core_pipeline.rs:157,477,712` |
| Provider set is closed | `provider/src/adapter/mod.rs:33,116` (enums) |
| No extension traits for routing, limits, budgets, cache, guards or credentials. Only `EventBus` and `Tokenizer` exist | `storage/src/lib.rs:103`, `core/src/token/tiktoken.rs:20` |
| Config must be TOML-shaped `AppConfig` | `server/src/state.rs:72` |
| Admin API bypasses storage and opens SQLite | `api/src/reader.rs:5-7` |
| No cargo features: server always pulls rusqlite (bundled C), core always pulls tiktoken, provider always pulls reqwest with two TLS stacks | crate manifests |
| Adapters and transport share a crate, so pure encoding requires reqwest | `crates/llm-proxy-provider` |
| Daemon concerns in core | `core/src/pid.rs` |
| Exact pin `time = "=0.3.44"` breaks downstream dependency resolution | `Cargo.toml:42` |
| Path dependencies without versions block `cargo publish` | 14 entries across manifests |

**Recommended shape** (pending Q2)

```text
llm-proxy-protocol     wire types and client codecs; no IO
llm-proxy-adapters     provider codecs; no IO (split from provider)
llm-proxy-transport    HTTP, credentials, egress
llm-proxy-engine       NEW: request lifecycle without HTTP. Layers plus traits:
                       Router, Admission, Cache, Guard, CredentialSource,
                       UsageSink, ConfigSource
llm-proxy-store-*      optional: sqlite, postgres, redis
llm-proxy-server       axum binding: routes and error mapping only
llm-proxy-admin        optional: admin API and UI
llm-proxy-mcp          optional
llm-proxy              facade crate with cargo features
apps/llm-proxy         daemon and CLI
```

**Recommended order**

1. Fix bugs 1–3, then 4–11.
2. Settle the request-field policy, and add contract tests from captured Claude
   Code, Codex, OpenAI SDK and Anthropic SDK traffic.
3. Extract the engine and its traits.
4. Add virtual providers and attempt-level ledger rows.
5. Then build features: retry/fallback, pools, one admission path, metrics, cache.
6. Optional crates last.

Extracting the engine before adding features stops the 13 server-mapped candidates
being welded to axum.

## Doc housekeeping

- CO's title and first paragraph say two waves and October 2026, but the doc covers three.
- `docs/research/README.md:33` says "three October waves"; wave 3 was November.
- The registry `scope` field says 73 product rows. There are 72.
- The Upstash row is a deliberate negative finding inside a "products" table.
- `harvest-manifest.json` lists 120 repos against 63 "harvested", and the research README doesn't mention it.
- 93 clone-relative paths look local (`crates/gateway/src/...`). They can't be rechecked: `ref/*/` and `research-repos/` are gitignored and absent, and `/tmp/intel-mistral` is gone.
- Wave 4 clones have no commit pins except mistral.rs. 62 of 162 backlog evidence entries are these paths.
- Four local citations never pointed at their claim: `middleware.rs:136`, `core_pipeline.rs:727-730`, `core.rs:639-641` (it's `Cost`, not usage), `counter.rs:282` (test code).
- Root `README.md` describes `llm-proxy-api` as "admin and user HTTP API". It is a read-only usage API.
- `config.toml.example:14` says rate limiting is per client IP, which is out of date.
- `upstash.com/docs/llms.txt` begins "If you are an AI agent ... send a POST request to /start-redis". An audit agent saw this and did not follow it.

## Questions

Answered on 2026-11-05 and recorded in `docs/goal.md`. Bugs 1 to 3 in the table
above have since been fixed; the findings describe the code at the audited commit.

1. **Routing:** virtual providers (recommended), or reverse the locked decision and add bare `/v1/*` routes?
2. **What does "SDK" mean for you?** In-process Rust crate, embedded axum router, translation library, or Python/Node bindings? Recommended: the first three now, bindings later.
3. **Who is this for first?** One developer's coding agents, a self-hosted team, or multi-tenant SaaS? Do you need multiple replicas? The docs assume the first with no replicas.
4. **Subscription OAuth (BL #12, parts of #15):** drop it, or keep it as an off-by-default personal option? Recommended: drop it, and exclude fingerprinting or impersonation either way.
5. **Data posture:** keep "no payloads stored" as a hard default, with cache and logging opt-in? Recommended: yes.
6. **Same-dialect requests:** pass unknown fields through with surgical edits, or keep normalizing everything? Recommended: passthrough.
7. **How literal is "all features"?** Are MCP gateway, admin web UI, reseller billing, semantic cache and non-chat endpoints in core, optional crates, or out? Recommended: optional crates, with reseller billing out.
8. **Is this running anywhere now?** If so, it needs `shutdown_timeout = "0s"` until bug 1 is fixed. Any real workload would also tell us which clients to capture for contract tests.

## Not checked

- Live upstreams; every probe used a local fake.
- The musl build; the 60-second headers timeout by running it.
- Respan's docs (HTTP 429).
- Most of the 859 registry rows, beyond those matching sampled claims.
- Portkey and Agent Router for upstream mTLS.
- Whether Kong's AI plugins honour Service-level client certificates.

Gateway groups A and B are summarized here only, because their sub-agents couldn't
write report files. The other tables are in `evidence/`:

- `llm-proxy-claims.md`
- `niche-repos-claims.md`
- `repo-intelligence-claims.md`
- `provider-notes-claims.md`
- `probes/`
