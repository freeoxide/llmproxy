# llm-proxy self-claims audit (HEAD 79cf406, 2026-11-05)

Read-only against the repo. Experiments live in `exp/` next to this file; outputs `exp/wire.out` (T01-T08), `exp/e2e.out` (E1-E9), `exp/e2e_e10_e11.out` (E10-E11), `home/serve.log` + `home/serve.exit` (real binary). No code changed between 6bcf1ef (cited by RI) and HEAD: `git diff 6bcf1ef HEAD -- crates apps` is empty.

Doc keys: GA competitive-gap-analysis.md, CO competitive-landscape-compendium.md, BL compensation-backlog.json, RI repo-intelligence.md, NR niche-repos.md, RM README.md, RR docs/research/README.md, PR docs/protocol.md, CP core-protocol.md, P providers.md.

Code keys: core_pipeline.rs, routes/*, middleware.rs, state.rs under crates/llm-proxy-server/src; transport.rs, discovery.rs, adapter/*, under crates/llm-proxy-provider/src; core.rs, anthropic.rs, openai.rs, zen.rs, client/* under crates/llm-proxy-protocol/src; provider_config.rs, metrics.rs, token/* under crates/llm-proxy-core/src; sqlite.rs under crates/llm-proxy-storage/src.

External-API shape statements (OpenAI Responses SSE, Gemini camelCase, OpenAI tool_choice) are from API knowledge, not run against live services.

## Totals

- CONFIRMED: 84
- PARTIAL: 82
- STALE: 2
- FALSE: 31
- UNVERIFIABLE: 2
- rows: 201

## A1. Gap analysis: baseline (GA:40-108)

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| GA:42-49 | Four API routes under mandatory /providers/{p} prefix plus /health, /ready, /version | CONFIRMED | routes/mod.rs:50-52,69-79 |
| GA:50 | 32 MiB body limit | CONFIRMED | routes/mod.rs:37,62 |
| GA:51-53 | Provider routes share a configurable TimeoutLayer returning 408; health/version exempt | PARTIAL | True as written (routes/mod.rs:49-57,64-67; required config field provider_config.rs:109-110). Misleading by omission: it only bounds the service future, i.e. until the response head. A stream outlives it (E10: request_timeout=1s, stream ran 2.5s and delivered late data). No total stream deadline exists. |
| GA:54-56 | Hub-and-spoke core with 10 content kinds | PARTIAL | Enum has 10 variants (core.rs:137-201). No client decoder ever constructs Document/Audio/Video. OpenAI decode drops image_url parts silently (openai.rs:127-141, T08); Anthropic `document` -> 400 (client/anthropic.rs:271-280, T02); Anthropic URL image dropped silently (anthropic.rs:565-573 + 171-187, T02). |
| GA:56-58 | Client adapters OpenAI Chat + Anthropic; upstream adapters Chat, Anthropic, Responses, Gemini | PARTIAL | All exist. But Responses decoder reads flat top-level fields (zen.rs:120-139): against OpenAI-documented SSE (`response.completed.response.usage`, `output_item.added.item`) it loses usage and tool names and reports EndTurn (T06). Gemini part has no camelCase rename (zen.rs:172-182): `functionCall` is ignored (T07). Fixtures are hand-written in the same flat shape (fixtures/responses/stream_tool/input.sse), so tests cannot catch it. Whether OpenCode Zen emits the flat shape: not checked. |
| GA:59-60 | In-band errors after visible output | PARTIAL | Only Anthropic-upstream/Anthropic-client path. Chat-upstream `{"error":..}` frame is skipped as malformed (adapter/openai_chat.rs:45-52; E7): client sees synthetic end_turn. CoreEvent::Error to an OpenAI client is swallowed (client/openai_chat.rs:685-691, core_pipeline.rs:1508-1511; E9): stream ends `[DONE]` with no error frame. Anthropic client gets `error` then synthetic message_delta/message_stop (E5, E9). All record ResponseCompleted. |
| GA:60-62 | 3-second SSE keep-alive comments on idle streams | PARTIAL | Keep-alive attached only after FirstEvent (core_pipeline.rs:878-883). E5: nothing (no headers, no comment) for 4.0s before first upstream frame; `:` comment appears 3s after. Upstream silence >60s still kills the stream via hard-coded idle watchdog (transport.rs:73,269-279). |
| GA:63-64 | mode=none loopback-only enforced; client-keys argon2id PHC | CONFIRMED | provider_config.rs:1650-1659; middleware.rs:419-425,484-487 |
| GA:64-65 | Admin API behind constant-time bearer | CONFIRMED | crates/llm-proxy-api/src/auth.rs:30-48 (SHA-256 digests compared with XOR fold) |
| GA:65-67 | api_key SecretString: redacted Debug, skip serialize, zeroize on drop | CONFIRMED | provider_config.rs:374-375; secrecy 0.10. Nit: transient header copies are plain Strings (transport.rs:324). |
| GA:67-68 | Stored as plaintext in operator TOML on disk | PARTIAL | Shipped templates use `${LLM_PROXY_*_KEY}` interpolation (providers/opencode-go.toml.example:8, apps/llm-proxy/src/defaults.rs:28); `init` writes 0600 (permissions.rs). Plaintext only if the operator types it. No encryption at rest: true. |
| GA:69-71 | Static alias; no request-path retry; only discovery retries | CONFIRMED | provider_config.rs:392-394; core_pipeline.rs:558-574,792-808; discovery.rs:131-153 (cited :160-170 shows only the classifier) |
| GA:72-74 | Catalogs static/live/hybrid, single-flight refresh, atomic disk cache | CONFIRMED | provider_config.rs:711-720; catalog_service.rs:218,410-459; tests catalog_service.rs:589,944 |
| GA:75-77 | Discovery keeps only id/display_name/context_length | CONFIRMED | discovery.rs:272-288. Note context_length reads only `context_length\|inputTokenLimit`, so OpenAI and Anthropic (`max_input_tokens`) lists yield None. |
| GA:77-78 | ModelCard hardcodes created:0, null created_at | CONFIRMED | routes/models.rs:129,133 (cited :24-36 is the struct, not the hardcode) |
| GA:79-82 | SQLite ledger, background writer, retention; only body-derived column is body_hash | PARTIAL | Ledger real (sqlite.rs:103-139,379-474) but opt-in; default is NoopBus (apps/llm-proxy/src/state.rs:92). model_requested/streaming are also body-derived. No payloads: true. |
| GA:83-84 | Read-only /usage/summary\|requests\|keys, window/group_by, cursor | CONFIRMED | api routes.rs:53-55,116-117; reader.rs:125,384-391,422 |
| GA:85-86 | Lock-free atomic counters incl. per-provider-model counts | PARTIAL | model_counts and latencies are `Mutex<..>` (metrics.rs:35,37). Others atomic. Nothing reads any of it: get_snapshot has no non-test caller (only routes/mod.rs:459,467 inside cfg(test)). |
| GA:86-87 | Request-id injection; graceful shutdown (shutdown.rs) | PARTIAL | Request id: routes/mod.rs:145-153. shutdown.rs exists, but serve_with_deadline starts the deadline at startup (apps/llm-proxy/src/commands/serve.rs:99,134-149): under default config the server force-exits with status 1 after 30s, no signal needed. Ran the real binary: exit=1, log "graceful-shutdown deadline elapsed" at t=30.0s. See BUG-1. |
| GA:88-92 | Token-bucket RPM, keyed by key id else IP, loopback exempt | CONFIRMED | middleware.rs:117-129; core_pipeline.rs:168-180. Exemption is only for unauthenticated direct loopback with trust_forwarded_headers=false. |
| GA:94-97 | Dedup window rejects identical in-flight path+body with 409 | FALSE | Off by default: default_dedup_window=ZERO (provider_config.rs:273-275), templates "0ms" (config.toml.example:21, defaults.rs:18). Not in-flight tracking: a fixed window from first sighting (middleware.rs:58-83). E4 at window=500ms: duplicate at 700ms while first still in flight -> 200; duplicate after first completed -> 409; different passthrough callers, same body -> 409. Bodies >64 KiB bypass (middleware.rs:62-64). |
| GA:97-98 | Forwarded-header trust, CORS, panic catching | CONFIRMED | middleware.rs:295-326; routes/mod.rs:94-104,111-144 |
| GA:99-100 | Offline tiktoken with chars/4 fallback | CONFIRMED | token/tiktoken.rs:50,150-; cited counter.rs:282 is inside a #[test] (compendium corrected this, gap analysis did not) |
| GA:101-102 | Per-provider fixed-point USD prices, estimation only | CONFIRMED | provider_config.rs:405,472-498; core_pipeline.rs:429-463 |
| GA:102-103 | Per-provider egress forward proxy socks5h/socks5/http/https | CONFIRMED | transport.rs:124-136,293; apps state.rs:57-71 |
| GA:103 | Env-var interpolation | CONFIRMED | env_interpolate.rs; provider_config.rs:1522 |
| GA:103-104 | musl single-binary releases | UNVERIFIABLE | dist-workspace.toml:13 targets musl. But `cargo tree --target x86_64-unknown-linux-musl -i openssl-sys` shows non-vendored openssl-sys via native-tls; no openssl-src in Cargo.lock; .cargo/config.toml comment says only musl-tools is installed. `gh api repos/freeoxide/llmproxy/releases` returns 0 releases. Could not run the musl build. |
| GA:104-107 | hot_reload=true rejected at load | CONFIRMED | provider_config.rs:1532-1539 |

## A2. Gap analysis: rest of doc

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| GA:112-115 | Keys: no budgets/scope/expiry; config edit + restart | CONFIRMED | provider_config.rs:245; middleware.rs:404-437 |
| GA:119 | No encryption-at-rest equivalent (plaintext TOML) | PARTIAL | No encryption: true. "Plaintext TOML" overstated, see GA:67-68. |
| GA:123-126 | URL-path provider + alias; no retries/fallback/cooldown | CONFIRMED | core_pipeline.rs:242-319 |
| GA:134-140 | Per-key/IP RPM; TPM and concurrency absent | CONFIRMED | core_pipeline.rs:172-180; no `Semaphore` in crates/apps |
| GA:148-158 | Prices appear only in provider_config.rs and core_pipeline.rs; no spend enforcement | PARTIAL | Also provider_registry.rs:387-398 (compendium has this), defaults.rs:52-60, storage Cost columns. No enforcement: true. |
| GA:165-169 | Dedup window rejects identical in-flight requests with 409 | FALSE | As GA:94-97 |
| GA:172 | "origin protection exists but the second caller still fails" | FALSE | Off by default; when on, E4 shows a duplicate past the window reaches origin while the first is in flight, and bodies >64 KiB always do. |
| GA:176 | No content inspection, redaction, moderation | CONFIRMED | No such code in crates/apps. (Upstream error-body key redaction exists: provider/error.rs; not content inspection.) |
| GA:184-190 | Collects 1000 latency samples and "computes percentiles in-process"; no /metrics, TTFT, trace export; percentiles never exposed | PARTIAL | metrics.rs:12,35,223-239. Helpers are never called outside tests, so nothing is computed at runtime; counters are also unexposed, not just percentiles. |
| GA:194,367 | "Keep-alive itself is parity" | PARTIAL | See GA:60-62 |
| GA:195 | Ledger keeps digests only; records only key id | CONFIRMED | sqlite.rs:105-131. Logs are separate: malformed-chunk warnings log 200 chars of data (adapter/openai_chat.rs:48-49); upstream error body logged at warn (core_pipeline.rs:369-375). |
| GA:199 | No MCP | CONFIRMED | grep: no mcp in crates/apps .rs/.toml |
| GA:207-209 | Two client formats, four upstream, count_tokens; other endpoints future | CONFIRMED | routes/mod.rs:69-79; PR:18-21 |
| GA:213 | Records normalized usage including cache/reasoning token splits | PARTIAL | Columns exist (sqlite.rs:117-119). reasoning_tokens is None in all four adapters (adapter/mod.rs:361, anthropic.rs:629, responses.rs:693, gemini.rs:553). Cache only from Anthropic and DeepSeek-style fields. |
| GA:232-240 | Passthrough BYOK; token used as credential at core_pipeline.rs:727-730; 401 when missing; shipped 2026-06-20 | PARTIAL | Feature real, commit 201e29e 2026-06-20 verified; 401 via error_response.rs:54. Cite wrong: swap is core_pipeline.rs:294-304, :727-730 only reads a flag. Also accepts x-goog-api-key (:418). With client-keys enabled, the proxy client key is forwarded upstream and the user x-api-key ignored (E11). |
| GA:247-248 | No data-posture routing | CONFIRMED | No such code |
| GA:257-259 | Over-limit gets 429 (middleware.rs:136); no queueing | PARTIAL | 429 true: core_pipeline.rs:177-180, error_response.rs:220-224. Cite wrong: middleware.rs:136 is `max_requests_per_minute: f64`. No Retry-After header anywhere (grep). |
| GA:267-268 | Targets stock OpenAI/Anthropic SDKs via base-URL swap | PARTIAL | Anthropic wire is deny_unknown_fields: `stop_sequences`, `top_k`, tool `cache_control` -> 400 (anthropic.rs:9-44,577-586; E8, T02). Client-key auth reads only Authorization, so Anthropic SDK default x-api-key -> 401 (middleware.rs:531-546; E11). OpenAI: `response_format` etc silently dropped (T08). |
| GA:272-276 | Single static bearer api_key; grep sigv4/bedrock/... zero Rust/TOML | PARTIAL | Grep result confirmed. "Bearer" wrong: AuthStyle has Bearer/XApiKey/XGoogleApiKey/Both (provider_config.rs:507-516), plus passthrough. |
| GA:283-286 | CoreContent::Text has cache; CacheControl type only, no TTL | CONFIRMED | core.rs:142,261-265 |
| GA:286-288 | "no adapter translates markers, injects them, or routes for cache locality" | FALSE | Translates: client/anthropic.rs:117-119,222-224,356-361; client/openai_chat.rs:84-95; adapter/anthropic.rs:711-715. T01 shows a system marker reaching upstream. Injection/locality absent: true. Real defect is partial: message markers dropped by adapter/anthropic.rs:640,825. |
| GA:289-290 | Cache read/write usage columns already land in the ledger | PARTIAL | Columns yes. Anthropic stream: message_start usage ignored (adapter/anthropic.rs:349-361) and a `{"output_tokens":N}` message_delta fails to parse (anthropic.rs:633 required input_tokens) so no UsageDelta at all (T03). Fixture fixtures/anthropic/stream_usage/core-events.json locks in the absence. |
| GA:296-300 | parse_models discards lifecycle; created:0; nothing warns | CONFIRMED | discovery.rs:272-288; models.rs:129,133 |
| GA:307-311 | CLI commands; read-only admin; egress | CONFIRMED | cli.rs:23-75; routes.rs:53-55; transport.rs:293 |
| GA:320-324 | Listener plain HTTP; upstream TLS is "reqwest rustls-tls" | FALSE | Listener part true. Upstream: provider Cargo.toml:23 does not disable default features, so `default-tls` and `rustls-tls` are both on (cargo tree). reqwest tls.rs:589-594 selects native-tls when default-tls is enabled; repo never calls use_rustls_tls. Active stack is native-tls. |
| GA:328-331 | State single-node | CONFIRMED | sqlite path; in-process limiter/dedup/metrics |
| GA:348-352 | Gaps 1,2,3,5 statements | CONFIRMED | as above |
| GA:353 | Upstream Responses exists, client surface does not | CONFIRMED | routes/mod.rs:69-79 |
| GA:360 | Alias resolves to exactly one provider | PARTIAL | Aliases are per-provider model maps; the provider is fixed by the URL and an alias cannot select one (provider_config.rs:392-394). |
| GA:361 | CacheControl cannot express TTLs; nothing translates or pins | PARTIAL | TTL and pin true; "nothing translates" false (GA:286-288). |
| GA:364 | Percentiles computed in-process, never exported | PARTIAL | See GA:184-190 |
| GA:367 | ClientCancelled persists with no usage payload; delivered tokens never settle | CONFIRMED | core_pipeline.rs:975-993; E6 row err=ClientCancelled http=200 in=None |

## A3. Near-term candidates / blueprints / build order

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| GA:382-391; CO:690-699,800-804 | Streaming path already awaits first CoreEvent before commit, so a 429/5xx/connect error can advance to fallback "in that window"; "fallback slots in" | PARTIAL | Wrong seam. 429/5xx/connect return at send_stream (core_pipeline.rs:792-808), before FirstByteResult exists. FirstByteResult only guards post-200 frames and closes on the first encoded event, which is a prelude: message_start (adapter/anthropic.rs:349-361), first chat chunk (adapter/openai_chat.rs:56-62), response.created (responses.rs:64-71), first Gemini chunk (gemini.rs:57-63). E5: 200 committed at message_start, so `overloaded` before any text cannot fail over. Non-stream path handle_core_once is separate. |
| GA:388-391; CO:696-699 | Needs a usage-row migration | CONFIRMED | request_id is PK (sqlite.rs:105); CREATE IF NOT EXISTS only, no migration mechanism exists (sqlite.rs:103-139,152). |
| GA:392-405; CO:703-716 | Budgets over existing ledger; window start "persisted with the key record" | PARTIAL | No key record exists: clients is a TOML map of id->PHC (provider_config.rs:245). Ledger optional and write-only from the server (EventBus, storage/lib.rs:103-112); reader is pub(crate) in llm-proxy-api. Rehydration has no API. |
| CO:805-809 | "the ledger already records settled cost, so enforcement is a counter over it" | PARTIAL | Cost is None unless provenance=ProviderReported (core_pipeline.rs:595-599,1417-1421). Default chat stream asks include_usage:false (adapter/openai_chat.rs:557-568; E1) and documented Anthropic delta yields no usage (T03), so streaming spend is often NULL. Budget would undercount by default. |
| GA:406-413; CO:720-726,810-813 | TPM reuses the offline counter; "cheap and refundable"; keyed by CallerId | PARTIAL | Counter exists, but count_tokens text extraction ignores tools, tool_result and images (token_count.rs:50-108), the bulk of coding-agent context; non-OpenAI models get chars/4 (tiktoken.rs:150-). Key is id-or-IP with loopback exempt. |
| GA:414-418; CO:749-753,814-817 | /metrics on admin listener walking the snapshot; counters and samples already exist | PARTIAL | Counters exist. Admin listener requires [auth.admin] and [storage] (provider_config.rs:1596-1603) and is built with no Metrics handle (server lib.rs:38-42), so it is not a drop-in. |
| GA:419-427; CO:730-737 | Cache done-condition: "dedup window still rejects concurrent identical misses before origin" | FALSE | Dedup is off by default and time-based (E4). If on, prepare_request runs dedup before any cache lookup (core_pipeline.rs:182-185), so a second identical request within the window gets 409, not the required HIT. The done-condition is self-contradictory. |
| CO:818-821 | Canonical CoreRequest gives a stable key and "the dedup window already discriminates identical requests" | PARTIAL | Dedup hashes raw path+body up to 64 KiB (middleware.rs:62-65,85-90), not CoreRequest. CoreRequest also omits provider and caller. |
| GA:428-433; CO:741-745 | Credential state "persisted via the existing storage layer" | PARTIAL | Storage is a single requests ledger with a write-only bus; no state store. |
| CO:822-824 | [[provider.credentials]] extends api_key "without changing request handling" | FALSE | Done-condition (401 lockout, credential id in rows) requires selection at core_pipeline.rs:294-304, feedback from :558-574/:792-808, new ledger column. api_key is a required field (provider_config.rs:375). |
| GA:434-440; CO:764-770,825-828 | TTL: add a field to core CacheControl; "CacheControl is the only missing field"; done when 1h marker round-trips | FALSE | See section E. |
| GA:441-450; CO:780-789,829-832 | Lifecycle "field work plus serde(default)" | PARTIAL | Also needs per-protocol ModelCard (one shape today, models.rs:24-36), and deprecated-use warning needs a request-time catalog lookup that today runs only when catalog.enforce=true (core_pipeline.rs:255-259). |

## A4. Compendium baseline and capability intros (CO)

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| CO:172 | Client surface/body limit/TimeoutLayer | PARTIAL | As GA:42-53 |
| CO:173 | Translation row | PARTIAL | As GA:54-58 |
| CO:174 | Streaming row | PARTIAL | As GA:59-62 |
| CO:175 | Auth row | PARTIAL | As GA:63-68 |
| CO:176-179 | Routing, Catalogs, Persistence, Admin rows | CONFIRMED | As GA:69-84 |
| CO:180 | Metrics row: lock-free incl. per-model; graceful shutdown | PARTIAL | As GA:85-87 |
| CO:181 | Dedup rejecting identical in-flight requests | FALSE | As GA:94-97 |
| CO:182 | Extras row incl. musl | UNVERIFIABLE | Musl as GA:103-104; rest confirmed |
| CO:183 | Cancellation row | CONFIRMED | E6 |
| CO:189-190,218-221,252-257,282-287,337,430,458 | Auth, routing, rate, pricing, guardrails, MCP, protocol intros | CONFIRMED | As GA equivalents; compendium pricing cite (provider_registry.rs:387-398, lib.rs:36, defaults.rs:52-60) is correct |
| CO:313-316 | Dedup rejects in-flight | FALSE | As GA:94-97 |
| CO:363-367 | Percentiles computed in-process | PARTIAL | As GA:184-190 |
| CO:401-402 | Keep-alive parity | PARTIAL | As GA:60-62 |
| CO:487-489 | Cache/reasoning splits | PARTIAL | As GA:213 |
| CO:510 | BYOK row with core_pipeline.rs:727-730 | PARTIAL | As GA:232-240 |
| CO:512 | 429 cite middleware.rs:136 | PARTIAL | As GA:257-259 |
| CO:517-518,644 | Static bearer only | PARTIAL | As GA:272-276 |
| CO:540-546 | Markers/ledger | FALSE | As GA:286-288 |
| CO:607-608 | reqwest rustls-tls | FALSE | As GA:320-324 |
| CO:641-660 | Gap register detail column | CONFIRMED | Except #13 (:653), #14 (:654), #17 (:657): see GA:360,361,364 |
| CO:669; NR:29,1053 | "upstream cancellation propagation" is a candidate gap | FALSE | Already implemented and tested: drop guard core_pipeline.rs:1488-1494, tests/core_pipeline.rs:1715; E6 shows upstream dropped in all three phases (non-stream, pre-first-event, post-first-event). |

## B. Compensation backlog (BL, line = "candidate" key)

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| BL:6 #1 | Forwards once; only discovery retries | CONFIRMED | Crate: blueprint 1 targets core_pipeline + storage, so server+core+storage, provider only for classification |
| BL:23 #2 | 200-then-error first frame "surfaces" in-band instead of recovering | PARTIAL | Chat upstream: the frame is dropped as malformed; if first, client gets 502 ProviderDecode, not in-band (E7); if after role chunk, masked as success (E7). In-band surfacing exists only for Anthropic `error` and Responses `response.failed`. Needs provider decoder change first. |
| BL:38 #3 | Fed only from Anthropic-dialect fields; no prompt_tokens_details; price at own rates | PARTIAL | Grep confirmed. DeepSeek-style prompt_cache_hit/miss also feed (adapter/mod.rs:347-366, T04). Pricing at own rates already exists (core_pipeline.rs:442-449). core.rs:639-641 is Cost, Usage is :683-685. Crate: provider+protocol. |
| BL:55 #4 | context_length surfaced; counter exists; no preflight | CONFIRMED | Facts true. Priority challenge: context_length is None for OpenAI/Anthropic discovery, and counter ignores tool_result/tools (token_count.rs:65-76), so high-priority rejection has poor inputs. |
| BL:70 #5 | Propagate disconnects; leaving while awaited leaves "no row at all" | FALSE | Propagation already done (E6). A row does exist: RequestReceived is emitted before the await (core_pipeline.rs:504-516,739-751,sqlite.rs:192-216); E6 shows dangling row recv=true done=false. Only settlement is missing. Rationale cites done-hub decoupling, the opposite of propagating cancel. |
| BL:85 #6 | Rate limit facts; no TPM/spend/reservation; all-or-nothing keys | CONFIRMED | Facts true. Crate wrong: limiter is server/middleware.rs. Contradicts CO:709,806-807 and GA:399. |
| BL:101 #7 | Upstream omits usage -> "records nothing"; force Accept-Encoding: identity | PARTIAL | Records a row with zeros and usage_provenance=synthetic_zero (core_pipeline.rs:1413-1416, sqlite.rs:261; E5). That is already the flag. No Accept-Encoding is sent at all: no compression feature on reqwest (cargo tree) and E2 upstream saw none. include_usage injection is the real fix (adapter/openai_chat.rs:557-568). Use UsageProvenance, per CP:145-151. |
| BL:115 #8 | One flat TimeoutLayer plus one idle watchdog; "single flat" to be replaced | PARTIAL | Already four phases: connect 10s (transport.rs:290), headers 60s (:20,176-178,204-206), idle 60s (:73), TimeoutLayer to head (E10). Real gaps: only request_timeout is operator-configurable (no non-test caller of with_stream_idle_timeout/with_upstream_headers_timeout), no total stream deadline, headers 60s < default 300s so long non-stream calls die at 60s. Crate: provider+core+server. |
| BL:131 #9 | Client surface is Chat + Anthropic only | CONFIRMED | Crate: also protocol, per PR:176-182 |
| BL:146 #10 | Strip client headers; redirects; sanitize client request ids; "no header policy" | PARTIAL | Strip: already true by construction (ProxyRequest has no inbound headers, transport.rs:40-52; E2). Request ids: inbound x-request-id ignored (routes/mod.rs:145-153; E2), nothing to sanitize. Redirect: REAL and worse, main client follows redirects and sends x-api-key cross-host (E3). FORBIDDEN_HEADERS exists (provider_config.rs:1364-1374). Crate should be provider. Inverse gap unlisted: client anthropic-beta/anthropic-version are lost. |
| BL:160 #11 | mode=none rests on bind alone; refuse non-loopback binds | PARTIAL | Second clause already implemented (provider_config.rs:1650-1659). Host/Origin guard absent: true. CORS opt-in: true (routes/mod.rs:94-97). |
| BL:175 #12 | Accepts only static bearer keys | PARTIAL | As GA:272-276. Priority challenge: CO:969-970 flags these patterns as ToS-gray; CO:755-760 defers cloud auth for lack of a config decision, same blocker. |
| BL:191 #13 | Passes Thinking through; converts no reasoning params | PARTIAL | Thinking kept only on Anthropic (with signature) and chat (as reasoning_content, signature lost); dropped on Responses/Gemini. reasoning_effort->Responses reasoning.effort is a conversion (responses.rs:536-542). Raw reasoning_effort leaks into Anthropic body (adapter/anthropic.rs:487-492), raw thinking into chat (adapter/openai_chat.rs:538). Crate: provider, per PR:166-168. |
| BL:206 #14 | Heartbeats during silent generation; guaranteed terminal usage | PARTIAL | Heartbeat exists after first event (E5). Terminal usage chunk exists but carries zeros when upstream sends usage after finish (E1). |
| BL:220 #15 | No normalization, no injection, no mutation boundary | PARTIAL | No cache-bust handling: true. But every request is fully re-encoded, so byte stability is absent. Crate wrong: request bodies are encoded in provider adapters, not core. Marker passthrough (section E) is a prerequisite. |
| BL:236 #16 | Replay gaps | CONFIRMED | GeminiPart has no signature (zen.rs:172-182); reasoning omitted when empty (openai.rs:99, adapter/openai_chat.rs:285-291). Crate: protocol+provider. |
| BL:250 #17 | Ledger records only key id | CONFIRMED | sqlite.rs:108; metadata.user_id not persisted. Note streaming rows already lose upstream model (BUG-10). |
| BL:265 #18 | Keys reach all the catalog exposes; allow/deny only at catalog level | PARTIAL | Catalog check runs only when catalog.enforce=true (core_pipeline.rs:255-259), default false; keys reach beyond the catalog. |
| BL:280 #19 | Client side Chat + Anthropic only | CONFIRMED | routes/mod.rs:69-79. Crate: also protocol. |
| BL:296 #20 | Only health notion is discovery transient retry | CONFIRMED | /ready is static (routes/health.rs:23-25) |
| BL:310 #21 | In-process counters, no export; "lock-free" | PARTIAL | No export: true. Not all lock-free (metrics.rs:35,37). |
| BL:325 #22 | Operator-typed, no refresh; need explicit unpriced state | PARTIAL | No refresh: true. Row-level unpriced already distinct (cost NULL vs 0, core_pipeline.rs:595-599; provider_config.rs:474-475). Summary folds to 0 (api reader.rs:246,253-258). |
| BL:341 #23 | Requires restart | CONFIRMED | Crate: also server (AppState is immutable Arcs, state.rs:31-46) and core |
| BL:356 #24 | Dedup only 409-rejects concurrent identical requests; retry re-bills | PARTIAL | Retry re-executes: true. Dedup is off by default and time-based (E4), not "concurrent". Dedup would also 409 an idempotent replay ahead of any claim logic. |
| BL:369 #25 | Only validates scheme; refuse non-loopback w/o auth | PARTIAL | Scheme-only: true (provider_config.rs:1226,1376-1379). Second clause already implemented, duplicates #11. |
| BL:381 #26 | validate static, status liveness | CONFIRMED | validate.rs; status.rs (PID only). `models --live` is an existing partial probe (cli.rs:58-62). |
| BL:394 #27 | Admin API reports only completed usage | FALSE | /usage/requests has no completed_at filter (reader.rs:368-404), and rows exist from RequestReceived, so in-flight rows are returned with completed_at null. Missing: state/elapsed, and dangling rows are indistinguishable. Crate: live state needs server. |
| BL:407 #28 | "already defers in-band errors until after visible output"; ecosystem masks truncation | FALSE | llm-proxy itself masks: E7 truncated and error-after-output both end end_turn + ResponseCompleted; E9 OpenAI client gets no error. Leading errors go 200 in-band, not deferred (tests/core_pipeline.rs:2578). Contradicts PR:146 and tests/core_pipeline.rs:1624. Priority should be high. Crate: provider+protocol+server. |
| BL:422 #29 | SSE decoders keep tool-arg fragment state; tolerate quotes in reassembly; longest-match tool names | FALSE | Decoders pass argument deltas through unparsed (adapter/openai_chat.rs:257-266, PR:142), keep only index maps, no name matching exists. Failure modes do not apply. Drop or rescope. |
| BL:435 #30 | No concurrency cap in egress | CONFIRMED | No Semaphore in crates/apps |
| BL:449 #31 | One static key per provider | CONFIRMED | provider_config.rs:375 (plus passthrough) |
| BL:463 #32 | Self-loop prevention absent | CONFIRMED | No hop header or self-check found; proposal stands |
| BL:475 #33 | Prove retention knobs; orphan sidecars | PARTIAL | Knobs real and already proven by tests (sqlite.rs:1102,1167). No sidecar data exists. Nearly moot. |
| BL:488 #34 | Three layers; validate prints without origins | CONFIRMED | validate.rs:26-43; serve --port only |
| BL:499 #35 | Read-only admin API substrate | CONFIRMED | routes.rs:52-55 |
| BL:512 #36 | Inbound surface two dialects | CONFIRMED | routes/mod.rs:69-79. Crate: also protocol. |
| BL:524 #37 | "emits no Cache-Control/Pragma headers anywhere" | PARTIAL | SSE sets no-cache (core_pipeline.rs:891-894). Admin API has none: true (routes.rs:58-61). Candidate stands. |
| BL:535 #38 | When llm-proxy exhausts its own retries | PARTIAL | No retries exist, depends on #1. No x-should-retry/Retry-After anywhere: true. |

## C. Repository intelligence and niche repos

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| RI:27-28 | No request-path retry outside discovery.rs:145 | CONFIRMED | as GA:69-71 |
| RI:28-29 | "flat TimeoutLayer" | PARTIAL | As BL #8 |
| RI:29-30 | 32 MiB; no prompt_tokens_details/cached_tokens in crates/apps | CONFIRMED | grep re-run: none |
| RI:31-32 | Core usage carries cache fields at core.rs:639-641 | PARTIAL | Those lines are Cost; Usage is :683-685 (R2 cites both, intro and BL #3 do not) |
| RI:33-35 | Rate limiter keys | CONFIRMED | core_pipeline.rs:172-176 |
| RI:54 | No streaming tokens/sec telemetry | CONFIRMED | No such field in metrics.rs |
| RI:125 | context_length, no preflight; other routes absent | CONFIRMED | models.rs:35,142; routes/mod.rs:69-79 |
| RI:177 | Opt-in CORS, no Host validation; dedup only 409s concurrent duplicates | PARTIAL | CORS/Host true (grep: no Host check). Dedup clause as GA:94-97. |
| RI:243,306,379,442,499,563,634,763,877,936,1060,1223,1274 | Absence lists (routing engines, OIDC/RBAC, OAuth, A2A, WASM, Redis, mgmt write API, semantic cache, queueing, DLP, etc.) | CONFIRMED | grep over crates/apps for oidc\|jwks\|rbac\|totp\|saml\|a2a\|wasm\|otlp\|prometheus\|siem\|compaction\|plugin\|idempotency: no functional hits; Cargo.toml has no redis/etcd/postgres client |
| RI:697 | Thinking.signature passthrough-only; no encrypted_content | PARTIAL | Grep true. Passthrough only on Anthropic; other adapters drop the signature. |
| RI:697 | "builds reqwest with bundled roots only" | FALSE | default-tls is active (native-tls, OS trust store), see GA:320-324; bundled webpki roots only apply if rustls is selected, which it is not. |
| RI:817 | Tiktoken + chars/4 mismatches non-OpenAI vocabularies | CONFIRMED | tiktoken.rs:150-; response carries no estimated flag (token_count.rs:20-24) |
| RI:1002 | Lists `{model}` URL templating as a gap vs llm-proxy | FALSE | Implemented: adapter/mod.rs:283-317; used by providers/opencode-zen.toml.example:25 |
| RI:1002 | Other done-hub gaps (Gemini client, cached-token normalization, header passthrough, no-cache on admin) | CONFIRMED | As backlog; E2 for headers |
| RI:1115 | One flat 300 s TimeoutLayer plus single idle watchdog | PARTIAL | 300s is a template value, field is required with no default (provider_config.rs:109-110). Omits connect 10s and headers 60s (transport.rs:20,290). |
| RI:1115 | "OS TCP keepalive on upstream sockets (llm-proxy sets keepalive only on accepted client sockets)" | FALSE | reqwest 0.12.28 defaults tcp_keepalive=15s, interval 15s, retries 3 (async_impl/client.rs:303-305); pooled_http_builder does not override (transport.rs:286-291). |
| RI:1177 | SSRF-safe egress with redirects disabled absent | CONFIRMED | transport.rs:286-291; E3 |
| RI:1323 | Client-facing event machine is Anthropic-shaped; no response_format/json_schema | PARTIAL | Grep true. Two encoders exist (client/anthropic.rs:544, client/openai_chat.rs:480). |
| RI:1372 | No request-body normalization; rejects at 32 MiB; raw cache columns, no ratios | CONFIRMED | As BL #15 caveat |
| RI:1428 | No compaction, session attribution, etc | CONFIRMED | grep |
| RI:2620-2622 R2 | Cache fields fed only from Anthropic dialects | PARTIAL | As BL #3 |
| RI:2628-2629 R3 | Counter + context_length, no preflight | CONFIRMED | As BL #4 caveat |
| RI:2635-2636 R4 | Ledger records only key id | CONFIRMED | sqlite.rs:108 |
| RI:2641-2643 R5 | "A single flat timeout cannot express..." | PARTIAL | As BL #8 |
| RI:2649 R6 | Static bearer keys | PARTIAL | As GA:272-276 |
| RI:2655 R7 | "no way to distinguish unpriced from free" | FALSE | cost NULL vs price 0 ("unmetered", provider_config.rs:474-475); row API returns Option (reader.rs:114,336). Only the summary folds to 0 (reader.rs:246). |
| RI:2662-2667 R8 | Per-identity RPM; no TPM/spend/model scope | CONFIRMED | As BL #6 |
| RI:2678 R10 | Rejects hot_reload; read-only admin | CONFIRMED | provider_config.rs:1532-1539 |
| RI:2686-2688 R11 | No normalization, injection, mutation boundary | CONFIRMED | As BL #15 caveat |
| NR:87 | BYOK parity, cite core_pipeline.rs:727-730 | PARTIAL | As GA:232-240 |
| NR:204,234,237,245,292,419,508,642,730,1023 | Parity/advantage notes (tiktoken, neutral IR, no tools, ledger, argon2, retention) | CONFIRMED | Ledger is opt-in; otherwise true |
| NR:815 | "No request dedup window (llm-proxy has one)" | PARTIAL | Exists, off by default, as GA:94-97 |
| NR:815 | Digests-only posture | CONFIRMED | sqlite.rs:130-131 |

## D. READMEs, protocol.md, smaller research docs

| Doc | Claim | Verdict | Evidence |
|---|---|---|---|
| RM:3-5,10-16,28-29,35-38 | Layout, routes, commands | CONFIRMED | routes/mod.rs:69-79; cli.rs |
| RM:15 | llm-proxy-api is "admin and user HTTP API" | PARTIAL | Admin only; no user API (routes.rs:52-55) |
| RR:7-10 | Client and upstream support | CONFIRMED | As GA:207 |
| RR:34,49-50 | 72 products/859 rows; 38 candidates 12/19/7 | CONFIRMED | Parsed repos-registry.json and compensation-backlog.json |
| PR:18-21,26-29,32-33 | Support table, routes | CONFIRMED | routes/mod.rs:69-79 |
| PR:58-59 | Adapters do not import client adapters | CONFIRMED | grep `client::` in provider/src: none |
| PR:72 | Bounded opaque provider hints | PARTIAL | Only stream_options is ever set (client/openai_chat.rs:239-247). adapter/mod.rs:145-149 logs that no adapter forwards hints while openai_chat reads one. |
| PR:80-91 | Ten content kinds | PARTIAL | As GA:54-56 |
| PR:101-107 | Response fields; "bounded provider metadata" | PARTIAL | provider_meta is always empty in all adapters |
| PR:109-111 | Unknown stops "remain explicit instead of being silently mapped to a successful stop" | PARTIAL | Explicit in core and ledger (sqlite.rs:170). Client wire maps Unknown/Error/Refusal to end_turn/stop with a warn (client/anthropic.rs:455-473; client/openai_chat.rs:444-454). Stream default is EndTurn (core_pipeline.rs:1431). |
| PR:121-133 | Event list matches CoreEvent | CONFIRMED | core.rs:734- |
| PR:141-142 | Stable indices; string deltas | CONFIRMED | adapters. Nit: OpenAI client tool index is the content index, so first tool after text is index 1. |
| PR:143 | Usage-only/finish-only events not discarded | PARTIAL | Anthropic message_delta without input_tokens is discarded (T03). Chat usage on a non-final, non-empty-choices chunk is ignored (adapter/openai_chat.rs:125-141). |
| PR:144 | Upstream IDs preserved | PARTIAL | True for Anthropic and chat. Responses reads top-level id only (T06: id None). |
| PR:145 | Usage not labeled provider-reported unless from provider | FALSE | OpenAI non-stream with no usage -> zeros labelled ProviderReported (openai.rs:292-293, adapter/mod.rs:358-365; T04, E7). Also contradicts core.rs:670-671. |
| PR:146 | Upstream disconnect finalized per client protocol | CONFIRMED | E7; but ledger records success, see BUG-4 |
| PR:147-148 | Error after visible output emitted in-band | PARTIAL | As GA:59-60 |
| PR:149-150 | No duplicate on first-event buffering | CONFIRMED | tests/core_pipeline.rs:1042 |
| PR:152-154 | Other kinds buffered or rejected | PARTIAL | Document/Audio/Video are dropped with a warn, not rejected (client/anthropic.rs:406-429) |
| PR:158-164 | Lossy policy: warn or reject when omission changes behavior | FALSE | response_format, n, seed go to metadata.raw and are never forwarded, no warning; image_url silently dropped (T08). 1h-marked block deleted with only a warn, HTTP 200 (E8). |
| CP:145-151 | Provenance: 5 values | PARTIAL | Enum has 3: ProviderReported, SyntheticZero, Unknown (core.rs:717-725). No locally-estimated or translated-then-counted. |
| CP:50-51 | Do not forward client auth or arbitrary fields without allowlist | CONFIRMED | E2; passthrough is explicit opt-in |
| CP:17-19 | Provider-only fields must not leak after translation | FALSE | reasoning_effort to Anthropic, thinking to chat (adapter/anthropic.rs:487-492, adapter/openai_chat.rs:538) |
| CP:69-70 | Do not finalize on first finish marker alone | FALSE | Encoders finish at MessageStop and drop later usage (client/openai_chat.rs:515-517,671-682; client/anthropic.rs:606-611,665-671; E1) |
| CP:71-72 | completed/failed/incomplete/transport are distinct terminals | FALSE | All -> ResponseCompleted EndTurn (E5, E7, E9). `response.incomplete` unhandled (responses.rs:203). |
| CP:84 | Error before output -> normal HTTP error when possible | FALSE | Leading stream error returns 200 + in-band by design (core_pipeline.rs:1177-1212; tests/core_pipeline.rs:2578). Chat first-frame error does give 502 (E7). |
| CP:142 | Never present synthetic usage as provider-reported | FALSE | As PR:145 |
| CP:190-192 | Dedup scoped to simultaneous submissions, avoid canceling legitimate repeats | FALSE | Time window, not simultaneous; cross-caller; 409s a sequential repeat (E4). Off by default. |
| CP:196-198 | Disconnect cancels upstream incl. first-frame wait | CONFIRMED | E6 |
| CP:204,208-209 | Billing durability; failed write stays pending | FALSE | Full channel drops events (sqlite.rs:477-478); failed batch "events lost" (sqlite.rs:418-423); force-exit skips close (serve.rs:148 vs 105). |
| P:87 | Rewrite max_completion_tokens only when target expects | FALSE | Serialized as max_completion_tokens to every chat upstream (openai.rs:190-195; T05c). Doc is advisory. |
| P:91 | Preserve Responses completed/failed/incomplete | PARTIAL | Non-stream maps status (responses.rs:654-659). Stream ignores response.incomplete, and failed is masked for OpenAI clients (E9). |
| P:159-172 | OpenCode surfaces | CONFIRMED | providers/opencode-*.toml.example; no route maps responses/gemini by default |
| endpoints.md:all | No claims of existing code | CONFIRMED | Normative only; absent from routes |
| config.toml.example:10-12 | log_format settable; env wins | PARTIAL | Config value is never read: serve.rs:25 uses LogFormat::from_env (provider_config.rs:121-123 admits this). |
| config.toml.example:14 | RPM "per client IP" | STALE | Keyed by key id when authenticated since 499b22a 2026-10-13 (core_pipeline.rs:172-176). Same at middleware.rs:132,147. |
| middleware.rs:24,33 | Dedup "default 500 ms" | STALE | Only for RequestDeduplicator::new(); runtime default is 0 (provider_config.rs:273). Test harness uses 500ms (tests/core_pipeline.rs:204), likely source of the docs' assumption. |

## E. Prompt-cache TTL: why "CacheControl is the only missing field" is false

Wire and adapter facts (T01, T02, E8):

| Input from client on /v1/messages | What happens | Code |
|---|---|---|
| system text block with `{"type":"ephemeral","ttl":"1h"}` | Whole block silently deleted (warn only), HTTP 200 | anthropic.rs:112-117 deny_unknown_fields; client/anthropic.rs:101-106 |
| message text block with same marker | Whole block silently deleted (warn only), HTTP 200. E8 upstream body had only USER-OK | anthropic.rs:174-186 |
| system text `{"type":"ephemeral"}` | Marker reaches Anthropic upstream | adapter/anthropic.rs:711-715 |
| message text `{"type":"ephemeral"}` | Decoded into core, then dropped, no warning | adapter/anthropic.rs:640,825 |
| tool definition with cache_control | 400 `unknown field cache_control` | anthropic.rs:576-586 |
| tool_result with cache_control | Dropped silently; inner block built with `cache: None` | client/anthropic.rs:178-217 |
| image with cache_control | Dropped with warn | client/anthropic.rs:231-235 |
| thinking / tool_use with cache_control | Dropped silently | client/anthropic.rs:242-267; no field on variants, core.rs:165-189 |
| OpenAI client `cache_control` with ttl | ttl dropped silently (no deny_unknown_fields) | openai.rs:10-14; client/openai_chat.rs:84-95 |

Needed beyond core.rs:261-266: wire CacheControl (anthropic.rs:113, openai.rs:11), decoders, `cache` on ToolResult/Image/ToolUse/Thinking/Tool or an equivalent, adapter/anthropic.rs:640,714,825,789-792, and the done-condition "ledger records cache-write tokens" needs adapter/anthropic.rs to read message_start usage and anthropic.rs:633 to stop requiring input_tokens (T03).

## F. Bugs found

| # | Severity | Where | What | How I know |
|---|---|---|---|---|
| BUG-1 | Critical | apps/llm-proxy/src/commands/serve.rs:99,134-149; default provider_config.rs:269-271 | Shutdown deadline starts at startup. With default config the server exits status 1 at t=30s with no signal, dropping in-flight work and skipping ledger close (serve.rs:148 vs 105). `shutdown_timeout = "0s"` avoids it. Origin 256fada 2026-06-18, no test. | Ran real binary: health 200 at 30s, gone at 34s, exit=1 elapsed=32s, ERROR "graceful-shutdown deadline elapsed" at 21:28:56 vs start 21:28:26 (home/serve.log) |
| BUG-2 | High | anthropic.rs:112-117; client/anthropic.rs:101-106; anthropic.rs:174-186 | `ttl` on cache_control silently deletes the block from the prompt, HTTP 200 | E8 |
| BUG-3 | High | anthropic.rs:631-636; adapter/anthropic.rs:337-344,349-361 | Documented Anthropic `message_delta` usage `{"output_tokens":N}` fails parse, event skipped: no usage, stop_reason `max_tokens` becomes `end_turn`; message_start usage ignored. Ledger synthetic_zero, no cost. A cumulative delta with input_tokens works. Fixture fixtures/anthropic/stream_usage/core-events.json locks this in. | T03, T03b |
| BUG-4 | High | adapter/openai_chat.rs:45-52; client/openai_chat.rs:685-691; core_pipeline.rs:1372-1396,1508-1511 | Upstream stream failure recorded as success. Chat `{"error":..}` skipped; truncated stream ends end_turn; OpenAI client gets no error frame, just `[DONE]`; Anthropic client gets `error` then synthetic end_turn. All ResponseCompleted. | E5, E7, E9 |
| BUG-5 | High | adapter/anthropic.rs:640,825 | Message-level cache markers dropped on Anthropic to Anthropic, so only system prompts can cache | T01 |
| BUG-6 | High | client/openai_chat.rs:515-517,671-682; client/anthropic.rs:606-611,665-671 | Standard usage-after-finish ordering: client-visible usage is zeros, ledger correct | E1 |
| BUG-7 | High | adapter/openai_chat.rs:557-568 | Every chat stream explicitly sends include_usage:false unless the OpenAI client asked; Anthropic clients never can. Ledger synthetic_zero, no cost. | E1 upstream bodies |
| BUG-8 | High (security) | transport.rs:286-291; reqwest redirect.rs:239-251 | Main client follows redirects with default policy; x-api-key and x-goog-api-key plus body go to a cross-host target. Bearer is stripped. discovery.rs:34-51 has the safe policy. | E3 |
| BUG-9 | Medium | core_pipeline.rs:403-417 | passthrough + client-keys: proxy client key sent upstream, user's x-api-key ignored; no config validation | E11 |
| BUG-10 | Medium | core_pipeline.rs:513,748,1428 | Streaming ledger rows lose model_upstream under aliases (core.model.upstream never set); non-stream rows keep it | E7 |
| BUG-11 | Medium | openai.rs:292-293; adapter/mod.rs:358-365 | Missing usage labelled ProviderReported zeros, cost 0 | T04, E7 |
| BUG-12 | Medium | adapter/mod.rs:362; core_pipeline.rs:442-445; provider_config.rs:490-491 | DeepSeek miss tokens become cache_creation and input_tokens goes to 0. With default cache_creation price 0, uncached input costs $0: this deflates cost unless the operator sets it, inflates only if priced above input. Anthropic clients see input_tokens 0. | T04, reading |
| BUG-13 | Medium | middleware.rs:58-83 | Dedup keyed without caller; window from first sighting, not in-flight | E4 |
| BUG-14 | Medium | transport.rs:20,176-178 | Hard-coded 60s headers timeout undercuts request_timeout 300s for non-streaming calls that send headers late; no config path. | Reading; not run |
| BUG-15 | High if routed | zen.rs:120-139; responses.rs:73-81,137,157 | Responses decoder vs OpenAI-documented SSE: tool name `<missing_function_name>`, id `__responses_missing_0__`, no usage, EndTurn. Encoder wraps function_call under role/content; drops temperature/top_p/max_output_tokens, claiming the API has no such parameter (responses.rs:553-570). Default templates do not route to it. | T06; shape from API knowledge |
| BUG-16 | High if routed | zen.rs:172-182; gemini.rs:425-435 | `functionCall` camelCase ignored, tool calls lost; `stream` field in body and no streamGenerateContent/alt=sse URL. Default templates do not route to it. | T07; shape from API knowledge |
| BUG-17 | Medium | adapter/openai_chat.rs:516-518; responses.rs:586-588 | tool_choice "auto" encoded as `{"type":"auto"}`; OpenAI documents a string | T05c; not run live |
| BUG-18 | Medium | anthropic.rs:9-44; client/anthropic.rs:53 | stop_sequences, top_k -> 400; no field for them | T02, E8 |
| BUG-19 | Medium | openai.rs:127-141; client/openai_chat.rs:249-252 | image_url, response_format, n, seed silently lost | T08 |
| BUG-20 | Medium | adapter/anthropic.rs:487-492; adapter/openai_chat.rs:538 | reasoning_effort sent to Anthropic, thinking sent to chat, unfiltered | reading |
| BUG-21 | Low-Med | core_pipeline.rs:1105-1111,848-864 | Pre-first-event disconnect leaves a dangling row forever (recv, no completion) | E6 |
| BUG-22 | Low | openai.rs:90-91 | Delta chunks carry `"role":""` | E1 |
| BUG-23 | Low | Cargo.toml of provider :23 | Two TLS stacks compiled; docs and comment imply rustls; musl openssl unvendored | cargo tree |
| BUG-24 | Low | api reader.rs:246,253-258 | Summary shows 0 cost for unpriced rows | reading |
| BUG-25 | Low | apps/llm-proxy/src/commands/serve.rs:25 | Config log_format ignored | reading |
| BUG-26 | Low | sqlite.rs:418-423,477-478 | Failed batch and full channel lose events (CP:204,208-209) | reading |

## G. Backlog: crate mapping and priority

| # | Named crate | Verdict | Suggested | Priority note |
|---|---|---|---|---|
| 1 | provider | Partly | server + core + storage, provider for classification | ok |
| 2 | server | Partly | provider decoder first, then server | ok |
| 3 | protocol | Partly | protocol + provider | ok |
| 4 | server | ok | + provider context_length capture | Challenge: inputs are weak |
| 5 | server | ok | | Rescope to settlement only |
| 6 | core | Wrong | server + storage + core config | Remove USD reservation or reopen blueprint 2 |
| 7 | provider | Partly | provider + server + protocol enum | Drop Accept-Encoding |
| 8 | provider | Partly | provider + core + server | Rescope: configurable and total deadline |
| 9 | server | Partly | + protocol | ok |
| 10 | server | Wrong | provider | Keep redirect clause, high; drop others |
| 11 | server | ok | | Drop bind clause |
| 12 | provider | Partly | + core | Challenge: medium, ToS flagged |
| 13 | protocol | Wrong | provider | ok |
| 14 | server | ok | + protocol encoder | Rescope |
| 15 | core | Wrong | provider | Depends on marker fix |
| 16 | protocol | Partly | + provider | ok |
| 17 | storage | Partly | + server | ok |
| 18 | core | Partly | + server | ok |
| 19 | server | Partly | + protocol | ok |
| 20 | provider | Partly | + server | ok |
| 21 | core | Partly | + server route | ok |
| 22 | core | ok | + api summary | ok |
| 23 | apps | Partly | + server, core | ok |
| 24 | server | Partly | + storage | ok |
| 25 | core | ok | | Drop bind clause |
| 26 | apps | ok | | ok |
| 27 | api | Partly | + server | Rescope |
| 28 | protocol | Partly | provider + protocol + server | Raise to high |
| 29 | protocol | Wrong | provider if kept | Drop |
| 30 | server | ok | | ok |
| 31 | core | Partly | + provider, server | ok |
| 32 | server | ok | + core | ok |
| 33 | storage | ok | | Drop: already proven, no sidecars |
| 34 | apps | ok | | ok |
| 35 | api | ok | | ok |
| 36 | server | Partly | + protocol | ok |
| 37 | api | ok | | ok |
| 38 | server | ok | | Blocked on #1 |

Unlisted but warranted: BUG-1, 2, 3, 4, 5, 6, 8 have no candidate and outrank most of the list.

## H. Contradictions

- BL #6 "in-flight USD reservation" vs CO:709,806-807 and GA:399-401 "deliberately not reservation-based".
- BL #2 "stage until first output" vs GA:382-384 and CO:800-802 "pre-commit window already exists".
- BL #8 "single flat" vs transport.rs phases; NR intro treats timeout hierarchy as absent.
- BL #14 heartbeats as a gap vs GA:194,367 and CO:401 "parity".
- BL #11 and #25 both propose the same bind refusal, already implemented.
- BL #28 "never synthesize success" vs PR:146 and tests/core_pipeline.rs:1624, which require synthetic terminal.
- BL #5 "propagate disconnects" vs its rationale citing done-hub decoupling; CP:196 requires cancel, RI:1002 lists WithoutCancel as a gap.
- BL #7 `usage_estimated` flag vs CP:145-151 provenance model.
- BL #13/#15 crate choices vs PR:166-168 (provider edge).
- BL #5 high, #9 high, #23 medium vs CO:791-798 and GA ranking, which defer these or rank them last. Different waves, but neither doc reconciles.

## I. Citations

- 80 local cited locations resolve and none is out of range. 93 entries tagged MISSING are clone-relative paths from other repos (91 in RI), e.g. `crates/aisix-proxy/src/a2a.rs`, `providers/base/common.go`, `docs/workload-identity.md`, which read like local paths. `research-repos/` and `research-issues/` are gitignored and absent; `ref/` holds only REFS. So that evidence cannot be rechecked from a clean checkout.
- No drift from comment stripping: line numbers were taken post-strip and code is unchanged since 6bcf1ef. The "verified 2026-10-27" tags precede the 2026-10-28 merge, but the stripped commits are dated 2026-10-14 and cited lines match HEAD.
- Wrong cites (never pointed at the claim, checked at e90fb06 too): middleware.rs:136 for 429; core_pipeline.rs:727-730 for credential swap; core.rs:639-641 for Usage (it is Cost); counter.rs:282 for the fallback (test code). Weak: models.rs:24-36 for hardcoded values (struct only, values at :129,133); provider_config.rs:735-747 for deny_unknown_fields (attribute is :734); discovery.rs:160-170 for retry (classifier; loop is :131-153).

## J. Not checked

Live upstream behaviour of OpenAI, Anthropic, Google or OpenCode Zen; the musl build; the 60s headers timeout by running it; claims about competitors (other workers); counts in the coverage appendices beyond those listed.
