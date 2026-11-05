# Fact audit: docs/research/niche-repos.md (wave 3) — 2026-11-05

Method: `gh api` repo metadata for all 125 github.com repos named in the doc (live.tsv), commit counts via Link headers, file fetches from main (not pinned; drift tolerated), docs-site fetches. Nothing built or run. Fetched content treated as data; no embedded instructions to the agent were found in any fetched file.
Legend: C=CONFIRMED P=PARTIAL D=DRIFT F=FALSE U=UNVERIFIABLE. Format: `doc line | claim | verdict | evidence`.

## A. Identity and maintenance stats (23 sections)
Every section's stars, created date, license, language, commit count, release tag/date and last-push were compared to live data. All match except where noted below (commit counts are exact: Switchyard 701 (doc "~100 since 10-01" n/a), xLB 38, ostk-cache 104, raine 687, SMG 4497 (doc 4,479 — digit swap), Antirion 534, AISIX 988 (doc 987), mcp-gateway 3191, ThinkWatch 913, preflight 5, tokdiet 12, CLIProxyAPI 4712 (doc 4,709), sub2api 7849 (doc ~7,850), CCR 989, done-hub 472, yolorouter 606, completion-to-response 6, GPTCache 515, PromptCache 47, ollamaMQ 67).

| doc line | claim | verdict | evidence |
|---|---|---|---|
| 54-95 Switchyard | Apache-2.0, 3,483 stars, v0.3.0 2026-09-22, v0.4.0a1, issue #1026 roadmap, main@f60a7a2 | C | api: 3483★/356 forks (doc 354), releases v0.4.0a1 2026-10-29, v0.3.0 2026-09-22, #1026 "[Roadmap] Switchyard v0.4.0 (November 2026)" open, sha f60a7a2291 exists |
| 98-135 xLB | created 2026-10-14, 38 commits, 173★, edition 2021, MIT, 0 issues | C | created 2026-10-14, 38 commits, 176★, `edition = "2021"`, 0 open issues |
| 108 | "9 releases (v0.0.1–v0.0.9)" | P | 9 git tags exist; GitHub Releases API lists only 5 (v0.0.1,2,7,8,9) |
| 177-215 raine | 687 commits, 694★, v0.1.48 2026-11-01, MIT | C | 687 commits, 694★; tag v0.1.48 exists, commit 49cf152 "release v0.1.48" dated 2026-10-31T23:04Z; Releases list tops at v0.1.47 (minor) |
| 216-256 mistral.rs | MIT, v0.9.5 2026-10-19, ~7.8k★ | C | 7808★, v0.9.5 2026-10-19 |
| 257-299 SMG | 699★, 4,479 commits, v1.14.0 2026-11-03 | C | 699★, 4497 commits (digit swap), v1.14.0 2026-11-03 |
| 300-340 Antirion | created 2026-10-17, 534 commits, v0.1.0/0.1.1 2026-10-18, 662★, "co-authored with Claude" | C | created 2026-10-17, 534 commits, releases 2026-10-18, 658★; 15 of last 30 commits carry Co-Authored-By Claude |
| 342-382 Plano | Apache-2.0, 7.1k★, 0.4.39 2026-10-24, DigitalOcean | C | 7110★, release 0.4.39 2026-10-24, planoai.dev links "DigitalOcean acquires Katanemo Labs" |
| 348 (also appendix line 1073-ish "Rust/TS", registry products[plano]) | "with a TypeScript `planoai` CLI" | F | cli/pyproject.toml + cli/planoai/*.py (23 .py files). TypeScript in repo is apps/www (marketing site), packages/ui, skills. https://github.com/katanemo/plano/tree/main/cli |
| 383-429 AISIX | created 2026-04-17, 987 commits, v1.0.0 2026-09-04, v1.8.0 2026-10-29, 212★/48 forks, 18 crates | C | 988 commits, 212★, 48 forks, v1.8.0 2026-10-29, 18 crates/*/Cargo.toml |
| 430-470 mcp-gateway | PolyForm NC 1.0.0, 3,191 commits, 85★, v3.5.1 2026-09-04, v4.0.0-beta.3 2026-10-06 | C | LICENSE "PolyForm Noncommercial License 1.0.0"; 3191 commits; tags/dates match |
| 471-515 ThinkWatch | BSL 1.1, 10M tokens/10k MCP calls, GPL-2.0+ change, created 2026-04-02, 913 commits, v3.11.0 2026-11-03 | C | LICENSE: BSL 1.1, 10,000,000 Billable Tokens / 10,000 MCP, Change Date 2030-04-02, Change License GPL-2.0-or-later |
| 516-563 preflight | created 2026-09-20, 5 commits all that day, no release, 123★, main@eb13fc0 | C | 5 commits all 2026-09-20 (one author), 123★, sha eb13fc015 exists, 11 forks, 0 watchers |
| 564-604 tokdiet | 12 commits 06-16/18, npm 0.1.0, 32★, MIT | C | 12 commits, created 2026-06-16, pushed 2026-06-18, 32★; 12/12 recent commits AI-marked; npm page 403 to bot (not checkable) |
| 605-650 CLIProxyAPI | MIT, 57.5k★/8.8k forks/4,709 commits, v8.0.54 2026-11-04, Go 1.26, main@78a9cc9 | C/D | 57530★, 8812 forks, 4712 commits, go 1.26.10; latest release already v8.0.55 (drift) |
| 651-694 sub2api | LGPL-3.0, 45.4k★, ~7,850 commits, v0.2.19 2026-10-28, Go 1.27 | C | 45441★, 7849 commits, v0.2.19 2026-10-28, go.mod `go 1.27.3`, LGPL-3.0 |
| 695-737 CCR | MIT, 38k★, 989 commits, v3.1.3 2026-10-09, "started the day after Claude Code" | C | 37993★, 989 commits, v3.1.3 2026-10-09, created 2025-02-25; open_issues 1188 ≈ 950+237 |
| 738-781 Otari | created 2026-04-02, 835★, v0.37.0 2026-11-04, Python 3.13+ | C | 838★, requires-python >=3.13, release v0.38.0 now (drift) |
| 753 | "(repo page; 835 commits ...)" | P | live commit count is 2113; "835" is the star count transposed into the commit field |
| 750 | "~90 alembic migrations" | P | 144 alembic version files in tree |
| 782-824 done-hub | Apache-2.0, 472 commits, 815★, v1.23.22 2026-09-29, fork of one-hub | C | 472 commits, 815★, README "基于 one-hub 二次开发" |
| 825-867 yolorouter | Apache-2.0, 606 commits, 254★, v0.1.0 2026-07-21 → v0.2.9 2026-10-27, HEAD 7ac9750 | C | 606 commits, 255★, v0.2.9 published 2026-10-28, sha 7ac9750 exists, 20 releases |
| 868-906 completion-to-response | 6 commits 05-02..17, v0.1.0, 14★, archived 2026-08-16, MIT, stdlib only | C | 6 commits, archived, updated_at 2026-08-16, go.mod has no require; MIT only in README (no LICENSE file; API license null) |
| 870 | "~800-line Go single-binary" | P | transform.go 627 + main.go 374 = 1001 lines (plus types, config) |
| 907-948 GPTCache | 8.2k★, final release v0.1.44 2024-08-01, README "no longer add support" | C | 8219★, v0.1.44 2024-08-01, README line 17 quote present; doc "34 releases Apr–Nov 2023" not exactly reproduced (41 releases total 2023-04..2024-08) -> minor |
| 949-988 PromptCache | 426★, 47 commits, v0.4.0 2026-04-24, last commit 2026-08-18 | C | all match |
| 989-1031 ollamaMQ | 67 commits, v0.5.0 2026-09-27, Cargo 0.5.1, edition 2024, 7 src files | C | all match; no CI workflow (only FUNDING.yml) confirms "or CI" absence |

## B. Capability claims (3+ per repo) and absences
| doc line | claim | verdict | evidence |
|---|---|---|---|
| 69 Switchyard | retry backoff 250 ms→2 s, Retry-After wins | C | crates/libsy-llm-client/src/client.rs: INITIAL_RETRY_DELAY 250ms, MAX_RETRY_BACKOFF 2s, "Retry-After wins" |
| 70 | `failure_cooldown_ms` default 5000; `fallback_on_rate_limit` default true | C | docs/reference/toml_schema.md lines 58-60 |
| 69/71 | evidence URL run.rs for retry/rate-limit policy | D | constants live in client.rs; `fallback_on_rate_limit` not in run.rs/client.rs/backend.rs (only the schema doc) |
| 72 | one `timeout_ms` covering all attempts, retry delays, stream reads | C | toml_schema.md:61 |
| 90 | "Theoretical KV-cache hit-rate tracking" | C | cache_eligibility.rs: env SWITCHYARD_THEORETICAL_CACHE, "Memory will grow forever so this is only for evals" |
| 93-95 | Absent: no MCP, no caching, no auth | C | no mcp path in 1028-file tree; only cache_eligibility.rs matches "cache" |
| 109 xLB | timeouts connect 5s, optional first_byte, idle, total, fin | C | config.toml: connect="5s", first_byte optional, idle=600s, total=3600s, fin=30s |
| 112 | buffered replay ≤64 MiB | C | docs/architecture.md:145 `retry.max_body_bytes` default 64 MiB |
| 120 | "non-idempotent POST/PUT/PATCH reroute only on unambiguous pre-execution statuses" | P | docs/changelog.md: opt-in via `retry.non_idempotent_retry_statuses`; "Until the narrower list is configured, non-idempotent requests keep the default behaviour and are still replayed on in-doubt 5xx" |
| 121 | `xlb_ttft_delta_ms` + 16 further metrics | C | docs/observability.md lists it plus ~19 others |
| 123 | integration suite ~87 KB | C | tests/integration.rs 87,428 bytes |
| 130 | Absent: no admin API/hot reload | C | only SignalKind::terminate in main.rs |
| 140 ostk-cache | strips `cch=<hex>;`; median of trailing 5 gaps; WARM/DEAD; 83% churn | C | billing_strip.rs:1, write_policy.rs:31-35, rebuild.rs:208 "Jun-11 soak measured 83% of write spend" (self-measured by the project) |
| 136-150 | MIT/Apache, 3 private crates | C | Cargo.toml license "MIT OR Apache-2.0"; deps git os-tack/haystack (private) |
| 163 | usage rewriting + fabricated errors to trigger client compaction | C | usage_truth.rs header: answers with `invalid_request_error` to push Claude Code into auto-compact (this manipulates client; doc reports neutrally) |
| 190 raine | retry on 429/500/502/503/504, 2s→30s | C | src/retry.rs: RETRY_INITIAL_DELAY_MS 2000, MAX 30_000, factor 2, statuses 429\|500\|502\|503\|504 |
| 214 | quote "no administrative mutation API, credential API, or remote shutdown route" | C | docs/src/content/docs/reference/http-api.md:178 (repo; not on the live site pages checked) |
| 217-255 mistral.rs | Anthropic gaps: signatures empty, cache_creation always 0, is_error not translated | C | anthropic-messages-api.md:123-125 states exactly this; anthropic.rs:1313,2725 |
| 233 | routes /re_isq, /v1/skills, /v1/containers, /v1/agent/approvals | C | route_registry.rs |
| 261 SMG | L0 256 MB / L1 50 MB; x-smg-priority; retry 50ms ×1.5 →30s ±20%, ≤1 MiB | C | lightseek.org/smg/v1.14.0 pages: defaults 268435456 and 52428800, `--retry-backoff-multiplier 1.5`, jitter 0.2, max-buffered-request-bytes 1 MiB |
| 296 | "150+ Prometheus metrics" | C | metrics reference lists 210 distinct `smg_` names |
| 296 | Absent: no USD cost tracking | C | repo's `policies/cost/` is a routing cost-function (worker scoring), not USD; no pricing files |
| 304 Antirion | 27-crate workspace, 13 fuzz targets, files cited exist | C | 28 crates/*/Cargo.toml, 13 fuzz_targets; all 12 cited paths exist |
| 331 | benchmark vs Bifrost/LiteLLM, published raw results | P | Real harness and published tables (README: p99 0.95 ms vs Bifrost 95.2 ms, LiteLLM 100% errors) but these are the project's own numbers on its own mock; doc caveat at 339 "largely flagged invalid" is correct (comparison.md "Invalid or skipped" lists most H1/C1/C4/C5 reps) |
| 336 | link `.../blob/console/README.md` | D | 404 (no branch "console"); file is at blob/main/console/README.md |
| 346 Plano | governor + acap crates in llm_gateway | C | crates/llm_gateway/Cargo.toml lines 19-20 |
| 353 | planoai.dev self-reports "115+ LLMs across 1M+ interactions" | U | static fetch of planoai.dev and /research did not contain it (JS-rendered?) |
| 368 | models.dev catalog in configuration reference | C | docs.planoai.dev config reference mentions models.dev |
| 386 AISIX | budgets "not part of OSS" | C | README:139 "Budgets and spend caps are AISIX Enterprise and ..." |
| 398 | "<10 MiB idle memory" | P | project's own release note ("an idle gateway now sits under ...") reported as capability, not measured |
| 407 | ArcSwap snapshots | C | aisix-core/src/snapshot.rs uses arc_swap::ArcSwap |
| 458 mcp-gateway, 1059 | "~8 ms hot reload" | P | README:229,265 marketing ("about 8ms", "~8ms"); no benchmark in repo seen; doc presents as fact in Capability index |
| 450-460 | PolyForm NC; 110+ capability YAMLs; retry forwarding not implemented | C | LICENSE; README:343; CHANGELOG:590 |
| 454 | response cache listed under #10 in index (line 1049) | P | gateway.example.yaml `cache_tools: true`, `cache_ttl` — MCP tool-result cache, not LLM response cache |
| 477 ThinkWatch | 131 rules in 20 groups | C | v3.11.0 release notes line 21 |
| 513, 1049 | "advertised LLM response caching unverified (cache.rs purpose not read)"; index omits ThinkWatch from Response caching | P | crates/gateway/src/cache.rs doc comment: "Redis-based exact-match cache for LLM responses ... non-streaming ... temperature == 0 ... All users share the same cache". Capability exists; doc undersells it and index row is incomplete |
| 485 | five built-in roles admin/operator/user/viewer/service | U | README says "Five built-in roles, from Super Admin to Viewer"; exact names not checked in source |
| 541 preflight | 16-permit semaphore → 503 `inspection_capacity`; SIGHUP reload; non-constant-time bearer | C | main.rs:291, 434, 297-300; main.rs:399 `v == format!("Bearer {k}")` |
| 562 | hard-wired to single "underclass" upstream | C | README:10,26,33 |
| 586 tokdiet | "published −71% input tokens"; 66-task A/B; shadow-eval 50-sample window | C | README:7,22,26-27 (5.07M→1.46M tokens; quality 64/66 vs 63/66, one model MiniMax-M3, author-run). Doc says "published" so attribution is OK, but it is self-reported |
| 596 | "no Prometheus", dormant | C | no metrics/prometheus paths in 98-file tree |
| 619 CLIProxyAPI | 3 rotation strategies, request-retry 3, 48-line bootstrap budget | C | config.example.yaml:170 (round-robin/weighted-round-robin/fill-first), :218 `request-retry: 3`, :410 "48 SSE lines" |
| 646 | downstream auth = flat plaintext api-keys list | C | config.example.yaml `api-keys:` list |
| 640 | cloaking/impersonation | C | config: `disable-claude-cloak-mode`, "Claude Code CLI disguise", uTLS, shared CLI fingerprint defaults (lines 255,484-513) |
| 667-690 sub2api | all 14 cited source paths exist; provider-adversarial TLS fingerprint profiles | C | tree: backend/ent/schema/tls_fingerprint_profile.go etc.; README:21-27 ToS warning |
| 696-737 CCR | ToolHub tool_hub.resolve/invoke; x-ccr-fallback-model | C | ccrdesk.top/toolhub, /routing (domain linked from official README, repo homepage) |
| 739-781 Otari | 403 `Otari-Error-Code: budget_exceeded`, `Otari-Budget-Scope`, Retry-After | C | docs/budgets.md:142-146; all 11 cited docs exist |
| 782-824 done-hub | cached_tokens omitempty bug, zero-value tests | C | commit 63d74b4763 message; README#L127-133 evidence link only has a one-line summary |
| 805 | "Codex OAuth with version-gated UA" | C | providers/codex/base.go: DefaultCodexUserAgent "codex_cli_rs/0.159.0", code comment "伪装的 Codex CLI 版本号" (spoofed Codex CLI version), originator "codex_cli_rs" |
| 826-866 yolorouter | key bench clamp 1 s–10 min; 7 timeout phases; 150-pair benchmark median +12.6% | C | keypool.go:35-36; config.go:54-65; docs/concise-output-benchmark.md: median +12.6%, 44 of 150 negative |
| 863 | no MCP | C | no "mcp" path in 1391-file tree |
| 879 completion-to-response | no sequence_number, partial Responses coverage | U | not checked in code |
| 912 GPTCache | 11 evaluators, files present | C | similarity_evaluation dir has 11 modules; all 8 cited files exist |
| 955 PromptCache | 0.70/0.30; judge models; MaxTokens 1024; 500ms retry + jitter | C | semantic.go:47-48; providers.go:148-152, 834; client.go:24,133 |
| 990 ollamaMQ | 300 s half-life, 100/user, 1 GiB blob, 3 tiers | C | dispatcher.rs:735, 799, 803 |

## C. Self-published benchmarks and superlatives reported in the doc
| doc line | claim | verdict | note |
|---|---|---|---|
| 331 Antirion | benchmark vs Bifrost/LiteLLM | P | doc stays neutral and adds caveat (339). Numbers (0.53 ms vs 19.7 ms; LiteLLM 100% errors at 5,000 RPS) are the vendor's own harness; LiteLLM 100% error at 5k RPS on 4 vCPU suggests a config/harness effect, not independent |
| 458/1059 mcp-gateway | ~8 ms hot reload | P | README marketing figure |
| 398 AISIX | <10 MiB idle | P | release-note figure |
| 586 tokdiet | −71% tokens | C (attributed "published") | single-model, author-run |
| 353 Plano | 115+ LLMs / 1M+ interactions | C (labelled "vendor claim") / U (not located) | |
| 142 ostk-cache | 83% of cache-write spend | not flagged as self-measured | project's own soak (rebuild.rs comment) |
| 833 yolorouter | +12.6% output reduction | C (attributed "published ... benchmark") | author-run, 150 pairs |

## D. Capability index (lines 1032-1060) spot checks
| line | cell | verdict | evidence |
|---|---|---|---|
| 1049 | Response caching: AISIX | C | config.example.yaml `cache:` section with semantic block |
| 1049 | Response caching: mcp-gateway | P | tool-result cache (`cache_tools`), not LLM completions |
| 1049 | Response caching row omits ThinkWatch | P | ThinkWatch has exact-match Redis LLM cache (see B) |
| 1049 | done-hub, CLIProxyAPI "verified absent" | C | done-hub `middleware/cache.go` is only Cache-Control headers; no response-cache path in CLIProxyAPI tree (internal/cache = auth/thinking replay caches) |
| 1048 | SMG and preflight "verified absent" for budgets | C | SMG: no pricing; preflight: none |
| 1038 | Timeout hierarchy: yolorouter (7 phases), xLB (5 phases) | C | config.go; config.toml |
| 1056 | Upstream cancellation: sub2api (499 classification) | C | failover_loop.go:331 "标记为 499" |
| 1055 | Cloud-native auth: ThinkWatch Bedrock SigV4 | C | crates/gateway/src/bedrock/sigv4.rs |
| 1059 | Hot reload: preflight SIGHUP, AISIX SIGHUP/etcd, mcp-gateway ~8 ms | C/P | SIGHUP verified in both; ~8 ms is marketing |
| 1043 | Auth: CLIProxyAPI (absence), tokdiet (absence) | C | plaintext api-keys; 127.0.0.1-only |
| 1053 | MCP: CCR ToolHub, done-hub, sub2api management MCP | C | tool_hub.*; mcp/server.go exists; managementmcp/server.go exists |
| 1052 | Prometheus: preflight | C | src/metrics.rs |

## E. Coverage appendix signals (lines 1062-1193)
Script (cmp.py) parsed all 121 rows and compared ★ and "pushed/archived" date with live API.
- Stars: 0 rows differ by >6% (all within a few stars; "~" forms match).
- Pushed dates: 114 exact; 6 off by 1 day (live push after the 2026-11-03 capture: 1mcp-app/agent, mcpproxy-go, mcp-gateway, awesome-ai-gateways, awesome-model-routing; classed DRIFT); NadirClaw doc 2026-10-22 vs live 2026-11-04 (DRIFT, 13 days); completion-to-response "archived 2026-08-16" vs pushed 2026-05-17 (consistent: archive date, API updated_at 2026-08-16).
- Archived flags verified: KenyonY/openai-forward (archived on GitHub; doc says "unmaintained" only), aws-samples/bedrock-access-gateway, luohy15/y-router, achetronic/claude-oauth-proxy, protectai/llm-guard, protectai/rebuff, completion-to-response.
- stevessr/Veloera: live repo exists, 0★, pushed 2026-03-04 (doc: "moved") C.
- akl7777777/shell-api: doc says "one-api fork"; API fork=true, consistent.
- Appendix 1072 Xerxes-2/clewdr: description "reverse proxy for Claude (web sessions + Claude Code)" and no ToS flag (see F).
- Row count 121 matches intro "121 candidate repositories". The "nine niche hunts" (line 1064) is not checkable.
- Wave-3 research URLs (310 ok, 63 "200", 1 404 = antirion console/README, 1 403 = npmjs bot wall) from url_results.json.

## F. Legal / ToS-sensitive capabilities (reported as neutral capability rows unless noted)
| repo | what doc says | ToS/ban risk mentioned in doc? | repo's own words |
|---|---|---|---|
| CLIProxyAPI (605-650) | "turns coding-CLI subscriptions ... into ... local APIs"; row 640 "Client fingerprint cloaking / CLI impersonation (pinned CLI wire fingerprints, uTLS, CCH signing, zero-width obfuscation)" tagged "ToS-gray; documented as pattern only"; intro 615 "Core value prop ... is ToS-gray" | Yes (explicit, but "gray", no mention of account bans) | README has no disclaimer at all; it carries residential-proxy sponsor ads ("one-IP-per-account", "multi-account operations", lines 85,93) and lists "Claude Code multi-account load balancing" |
| sub2api (651-694) | "Its README carries a ToS-violation warning"; "provider-adversarial machinery"; appendix "ToS-flagged" | Yes, strongest | README:25 "Using this project may violate the terms of service of Anthropic and other upstream providers"; :27 "assume no liability for account bans, service interruptions"; sells pooled subscriptions as `sk-` keys with payments (account pooling/resale), TLS fingerprint profiles, per-account proxy rotation |
| raine/claude-code-proxy (177-215) | Neutral: "lets Claude Code drive subscription-backed upstreams — Codex/ChatGPT, Kimi, Grok, Cursor Agent"; OAuth lifecycle row | No | README:111-113 "Provider subscriptions, model access, terms, and account enforcement remain under each provider's control. Unofficial clients may carry account risk." |
| done-hub (782-824) | Row 805 "Agent-CLI OAuth channel types: Claude Code console OAuth, Codex OAuth with version-gated UA ..., Gemini CLI published credentials, Antigravity"; no caveat | No | providers/codex/base.go: sends `codex_cli_rs/<ver>` UA + originator, code comment "伪装的 Codex CLI 版本号" (disguised/spoofed version header); README has no disclaimer; also a multi-user resale panel (payments, invites) |
| Switchyard (87) | "Caller-credential forwarding (use your Claude login upstream) incl. a Claude Code plugin" | No | examples/claude/README: "routes between Sonnet and Opus using your Claude login" |
| ostk-cache (163) | "fabricated Anthropic-shaped errors to trigger client-side compaction", rewrites reported usage | No (not a ToS issue per se; client-deception) | usage_truth.rs header |
| clewdr (appendix 1072, not researched in full) | "Rust reverse proxy for Claude (web sessions + Claude Code)" | No | README has no disclaimer text matching terms/ban/risk |
| qwengate (1098) | "gray-zone pattern ref" | Yes | README:13 "Users must comply with chat.qwen.ai's terms of service" yet :94 "Use 3+ accounts for round-robin rotation to bypass cooldown limits" |
| howardpen9/awesome-ai-api-proxy (1183) | "Relay-station gray market catalog" | Partly | not fetched |
| Cloaking in CLIProxyAPI/sub2api and account pooling in CLIProxyAPI, sub2api, yolorouter (key pools of own API keys: not ToS-sensitive), CCR (credential pools incl. Kimi/Grok OAuth rotation) | CCR pool row 719 describes "credential pools ... rotation on refresh-401 (Kimi/Grok); macOS keychain" with no risk note | No | |

## G. Maturity / demand-evidence weakness (created date, ★, commits, other)
| repo | created | ★ | commits | notes |
|---|---|---|---|---|
| EeroEternal/xlb | 2026-10-14 | 176 | 38 (5 days) | .agents/skills + AGENTS.md; 0 issues; dormant after 10-19; 19 forks, 16 watchers |
| eliasbiondo/antirion | 2026-10-17 | 658 | 534 in 14 days | 15 of last 30 commits Claude co-authored; 15 forks, 3 watchers, 2 contributors; star count 2 weeks old |
| ghuntley/preflight | 2026-09-20 | 123 | 5 (one day) | AGENTS.md; 0 watchers; no release |
| agiwhitelist/tokdiet | 2026-06-16 | 32 | 12 (3 days) | 12/12 recent commits AI-marked; 1 npm release; dormant |
| os-tack/ostk-cache | 2026-05-06 | 10 | 104 (one author) | private deps, can't build, dormant since 06-12 |
| NoahStepheno/completion-to-response | 2026-05-02 | 14 | 6 | archived |
| yolorouter/yolorouter | 2026-07-21 | 255 | 606 | 6 forks, 0 watchers, 1 open issue, 2 contributors |
| MikkoParkkola/mcp-gateway | 2026-01-25 | 85 | 3191 | 23/30 recent commits AI-marked; 0 watchers; non-commercial license |
| raine/claude-code-proxy | 2026-04-18 | 694 | 687 | genuine adoption signals (165 forks, 30 contributors) but 6.5 months old |
| ThinkWatch, Otari, AISIX | 2026-04 | 814 / 838 / 212 | 913 / 2113 / 988 | <7 months old; Otari/AISIX corporate-backed |
| Switchyard | 2026-05-19 | 3483 | 701 | NVIDIA; server is "demo-stability" |
| messkan/prompt-cache | 2025-11-22 | 426 | 47 | 43 forks |
Doc already labels many of these as young/dormant; it does not label yolorouter, mcp-gateway, raine or ThinkWatch as young. Stars alone overstate adoption for Antirion (658★ in 14 days, 3 watchers).

## H. Registry cross-check (repos-registry.json, 72 products)
All 23 wave-3 repos have a registry row; status text matches doc. Registry repeats the Plano error ("Rust (proxy-wasm plugins) + TypeScript CLI") (F).
