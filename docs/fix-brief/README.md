# Fix brief: finish every completed plan

All eleven numbered plans and the provider-routing remodel under `docs/completed/` are
marked "Completed", but a static audit on 2026-11-09 (master @ `fe6e66f`) found work that
is partial, missing, untested or described wrongly. This folder lists every item: 188 in
total (18 P0, 97 P1, 73 P2). Your job is to close all of them.

## Files

| File | Plan |
|---|---|
| [plan-01.md](plan-01.md) | 01 Bug fixes |
| [plan-02.md](plan-02.md) | 02 Request fidelity |
| [plan-03.md](plan-03.md) | 03 Engine and crate split |
| [plan-04.md](plan-04.md) | 04 Routing and failover |
| [plan-05.md](plan-05.md) | 05 Admission and budgets |
| [plan-06.md](plan-06.md) | 06 Cache and dedup |
| [plan-07.md](plan-07.md) | 07 Hardening and operations |
| [plan-08.md](plan-08.md) | 08 Client dialects |
| [plan-09.md](plan-09.md) | 09 Optional crates |
| [plan-10.md](plan-10.md) | 10 Bindings |
| [plan-11.md](plan-11.md) | 11 Research corrections |
| [plan-remodel.md](plan-remodel.md) | Provider-routing remodel (items `R-n`) |

Read this page fully, then open a plan file only when you work on its items. Each plan
file stands alone: "Done — leave alone", work items, rule violations, rejected findings.

## Rules (read before touching code)

1. Read `AGENTS.md`, `docs/goal.md` and `docs/README.md` first. Every rule in `AGENTS.md`
   applies: no `unwrap`/`expect`/`panic!`/indexing/slicing in non-test code, no `#[allow]`,
   no `let _ =` on a `Result`/`Future`, `thiserror` in libraries, tests assert observable
   behaviour, one public path per item, no design narratives in user-facing docs.
2. Run the quality gates after every work item, not only at the end:
   ```
   cargo fmt --all
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   ```
   Also run `cargo test --workspace` (default features) and
   `cargo test -p llm-proxy-sdk --no-default-features` when you touch feature gates.
   Run `cargo shear` before adding or removing a dependency. `bindings/node` and
   `bindings/python` are separate workspace roots; build and test them from their own
   directories when you touch them.
3. Red-green for every FIX and TEST item: write the test first, watch it fail for the
   stated reason, then fix. If a test passes before the fix, the test is wrong;
   strengthen it until it fails on the old code.
4. Do not start servers, daemons or live upstreams by hand. Use the in-process fakes the
   test suites already use (loopback fake upstreams, the in-repo RESP server, `oneshot`
   on the router). `#[ignore]`d live tests stay ignored.
5. Line numbers were correct on 2026-11-09; if they drifted, find the symbol by name.
   If an item is already fixed or the finding was wrong, record that with evidence in
   your final report and move on. Do not invent work.
6. `DECIDE` items carry a recommended default. Take it unless the code shows it is
   wrong; record the choice in your final report, not in user-facing docs.
7. When a fix changes what a plan's "Done" section says, update that plan file in
   `docs/completed/` in the same change so it states the truth. Keep plan text factual
   and short.
8. Git: work on one branch off this one (for example `fix/finish-plans`), never on
   `master`. Commit per plan area (one or a few commits per plan) with messages like
   `fix(engine): check key allowlists before cache and idempotency replay`. Push the
   branch after every plan area so no work is lost if the session ends. At the end open
   one pull request into `master` whose description is the final report.
9. Do not edit the files in `docs/fix-brief/` except to delete the whole folder (and its
   row in `docs/README.md`) in the last commit, once every item is closed.

## Order of work

1. All **P0** items across plans (correctness, security, money/data loss, false "Done
   when"). Suggested sequence: 02 (silent field drops), 09 (Redis budget race, guardrail
   fail policy), 07 (reload half-apply), 03 (allowlist order), 05 (status table,
   charging), 10 (binding errors and cancellation), 01, 04, 06, 08, remodel.
2. All **P1** items (missing features claimed done, missing behavioural tests on key
   paths).
3. All **P2** items (docs, naming, small tests, rule-violation cleanup).
4. Final: run every gate above, then write the report: each item ID with `done` /
   `rejected (why)` / `decided (what)`, plus the names of tests added.

## Item format

Each work item is `NN-k [TYPE] [PRIORITY] title` with **Where / Now / Wanted / How /
Test / Done when / Risk**.

Types: `FIX` (change behaviour), `TEST` (add or strengthen a behavioural test),
`DOC` (fix stale or wrong text), `DECIDE` (pick between two outcomes; default given).

## Index of all work items (P0 first)

### P0
- 01-1 [FIX] [P0] Reject an undecodable block nested in `tool_result` content with HTTP 400
- 04-1 [FIX] [P0] Failover strip does not strip signed thinking, file ids, or anything on same-protocol failover
- 03-1 [FIX] [P0] Enforce the per-key allowlist at slot 2, before idempotency replay, cache and single-flight
- 02-1 [FIX] [P0] Do not silently drop `tool_result.is_error` on Chat, Responses and Gemini
- 02-2 [FIX] [P0] Carry `tool_choice` to Gemini as `toolConfig.functionCallingConfig`
- 02-3 [FIX] [P0] Unknown `tool_choice` variants: reject by name (Chat, Responses, Anthropic)
- 02-4 [FIX] [P0] Fail (or honour lossy) when requested thinking cannot fit under `max_tokens`
- 05-1 [FIX] [P0] Put 508 in the status table and stamp `x-should-retry` on every client-router response
- 05-2 [TEST] [P0] Make "every status is a row" compiler-checked and router-level
- 05-3 [FIX] [P0] Stop bare (unprefixed) tokens from costing one argon2 run per key
- 05-5 [FIX] [P0] Charge billed cost on pre-commit stream failure
- 07-1 [FIX] [P0] Make `restart_required_changes` refuse `auth.admin.key_store`, `[replica]` and `[mcp]`
- 09-1 [FIX] [P0] Redis settlement must never expose less than the final exposure to a concurrent admission
- 09-2 [FIX] [P0] Redis unsettled leases must actually release their reservation (crash bound is currently false)
- R-3 [FIX] [P0] A stalled pagination token must fail the refresh, not return a truncated catalog
- 10-1 [FIX] [P0] Dropping an `EventStream` must cancel the upstream request
- 10-2 [FIX] [P0] Node `for await ... break` never cancels (iterator has no `return()`)
- 10-6 [FIX] [P0] Node pre-commit stream errors lose `shouldRetry` and `retryAfterSeconds` (snake_case vs camelCase)

### P1
- 01-2 [TEST] [P1] Prove `validate` and startup refuse passthrough + client-keys
- 01-3 [TEST] [P1] Pin chat usage mapping: `cached_tokens`, `reasoning_tokens`, DeepSeek hit/miss, no cache creation
- 01-4 [TEST] [P1] Pin Responses usage mapping for reasoning tokens
- 01-5 [FIX] [P1] OpenAI-client usage must report prompt tokens including cache, and must not call cache creation "miss"
- 01-6 [TEST] [P1] Assert the Responses encoder carries `temperature`, `top_p`, `max_output_tokens`
- 01-7 [TEST] [P1] Make the encode-fixture runner compare `input`; refresh the stale Responses fixtures
- 04-2 [FIX] [P1] The ladder total deadline does not bound an in-flight attempt
- 04-3 [TEST] [P1] Server-level `upstream_timeouts.total` tightens the ladder
- 04-4 [FIX] [P1] Rate-limit reset parsing: scope by header name, parse RFC 3339 timestamps
- 04-5 [TEST] [P1] A 429's cooldown lasts until the advertised reset
- 04-6 [FIX] [P1] Guard-rejected (billed) attempt has no attempt row
- 04-7 [FIX] [P1] `DEFAULT_UPSTREAM_HEADERS_TIMEOUT` still public and used by SDK/bridge clients
- 04-8 [DECIDE] [P1] Per-family upstream-timeout overrides (claimed in Done, absent)
- 04-9 [DECIDE] [P1] Heartbeats "before commit"
- 04-10 [TEST] [P1] Done-when: a pool survives a 5xx AND a connect error from its FIRST member, ledger shows every attempt
- 02-5 [FIX] [P1] Clamp Gemini same-format `thinkingBudget` per model
- 02-6 [DECIDE] [P1] `response_format` cross-format (Gemini, Responses)
- 02-7 [DECIDE] [P1] `seed` cross-format
- 02-8 [DECIDE] [P1] `parallel_tool_calls`, `store`, `prompt_cache_key` into Chat providers
- 02-11 [DECIDE] [P1] Key the disposition table per (client format, provider protocol)
- 02-12 [FIX] [P1] One definition of the never-forwarded header set; validation must reject `browser-*`
- 02-13 [FIX] [P1] Endpoint-family forwarder must use `select_forwarded_headers`
- 02-14 [FIX] [P1] Capture Gemini `thoughtSignature` in streaming responses
- 02-15 [TEST] [P1] Gemini signature capture from the non-streaming response, from a wire body
- 02-16 [TEST] [P1] Cross-format image legs: Gemini `inlineData`/`fileData`, OpenAI `image_url`, Anthropic
- 02-18 [TEST] [P1] Gemini `thinkingConfig` reaches the wire
- 02-19 [TEST] [P1] OpenAI `reasoning_content` is replayed outbound
- 03-2 [FIX] [P1] Streaming path: run pre-call guards after the budget reservation (slot 7 after slot 6)
- 03-3 [FIX] [P1] Apply guard request rewrites at slot 8 (after inspect and reserve), not before routing
- 03-4 [FIX] [P1] Make `UsageSink` an actual pluggable seam
- 03-6 [FIX] [P1] `tiktoken-rs` must not reach consumers that do not count tokens
- 03-7 [FIX] [P1] CI: feature-build and dependency-closure guards
- 03-8 [FIX] [P1] CI: `cargo publish --dry-run` check for the published crates
- 03-9 [DECIDE] [P1] "Contract tests pass unchanged through the binary"
- 03-14 [FIX] [P1] reqwest 0.13 (OTLP exporter) has no TLS backend
- 08-1 [DECIDE] [P1] Responses tool-level `strict`: reject `strict: true` by name cross-format, carry it same-format
- 08-2 [TEST] [P1] Assert a usage-ledger row, with the dialect, for Responses, Gemini and Ollama requests
- 08-3 [TEST] [P1] Streaming error tests for all three dialects
- 08-4 [TEST] [P1] Server-tool rejection for Responses `web_search` and Gemini `googleSearch`
- 08-5 [FIX] [P1] Gemini `promptTokenCount` must include cache reads (and expose `cachedContentTokenCount`)
- 08-6 [FIX] [P1] Gemini streaming terminal chunk drops `thoughtsTokenCount`
- 08-8 [TEST] [P1] Make the `/api/tags` route test non-vacuous and cover pools
- 08-10 [TEST] [P1] `previous_response_id` survives same-format and is rejected by name cross-format
- 06-1 [FIX] [P1] Reject invalid `~` escapes in `strip_volatile` pointers at config load
- 06-2 [FIX] [P1] `inject_anthropic_markers`: only mark eligible blocks, and only the last one per message
- 06-4 [TEST] [P1] Prefix-stability tests for reasoning translation, cross-format, Gemini, Responses and Chat
- 06-5 [TEST] [P1] Make the shared-fate and leader-vanish tests prove sharing (counters), and fix the lazy-future bug
- 06-6 [TEST] [P1] Registry-drain test: nothing is retained after completion
- 06-7 [TEST] [P1] Idempotency replay and in-flight 409 never reach admission
- 06-8 [TEST] [P1] Cache-hit metric asserted on the hit path
- 06-12 [DOC] [P1] `remodel-proxy.md` still documents `dedup_window`, which now fails to load
- 05-4 [FIX] [P1] Rebuild day budget and rpd from the ledger at startup (and narrow the claim)
- 05-6 [FIX] [P1] Family routes: reserve a real input estimate
- 05-7 [FIX] [P1] Re-price the budget reservation on failover (`rekey`)
- 05-9 [FIX] [P1] Flag mixed priced/unpriced summary groups
- 05-10 [FIX] [P1] pricing-sync: never write a silent $0 for a missing dimension
- R-1 [FIX] [P1] Classify every `/providers/{p}/v1/chat/*` path as OpenAI-shaped for layer errors
- R-2 [DECIDE] [P1] One error shape for `/providers/{p}/v1/models`
- R-4 [FIX] [P1] `catalog()` ignores discovery and TTL when `[provider.catalog]` is omitted
- R-5 [TEST] [P1] End-to-end tests for catalog enforcement (HTTP level)
- R-6 [FIX] [P1] Catalog cache read path must reject a symlinked or non-directory cache dir
- R-7 [FIX] [P1] `validate_routes` must check every route kind (and compare exactly) at config load
- R-9 [TEST] [P1] Behaviour test for the loopback rate-limit exemption (`also: plan 11`)
- 07-2 [FIX] [P1] Serve `/metrics` on the `admin-ui` dedicated admin listener
- 07-4 [FIX] [P1] Stamp the hop header on discovery requests
- 07-5 [FIX] [P1] Config-time self-loop check must understand wildcard binds
- 07-6 [FIX] [P1] Include the MCP gateway listener in the self-loop listener list
- 07-7 [DECIDE] [P1] Foreign `Origin` without Fetch Metadata: keep the carve-out, fix the done-when
- 07-8 [FIX] [P1] Put `/health`, `/ready`, `/version` behind the host guard
- 07-9 [FIX] [P1] `doctor` must honour `[provider.egress]` (reachability and probe)
- 07-10 [FIX] [P1] Give the "no one-token probe for protocol" WARN a named fix
- 07-13 [TEST] [P1] Make the reload done-when unit test distinguish the old and the new upstream
- 07-14 [FIX] [P1] Config errors must not echo userinfo or query strings from endpoints
- 07-22 [FIX] [P1] Model discovery ignores `[provider.egress]` (found while verifying 07-4/07-9)
- 10-3 [FIX] [P1] Python: context manager and explicit close; document that a held reference defeats "break cancels"
- 10-4 [DOC] [P1] Remove or correct every "dropping / breaking cancels" claim that the code does not honour
- 10-5 [DECIDE] [P1] `request_timeout_secs` is inert on the bridge path
- 10-7 [FIX] [P1] Constructor config errors must be `ProxyError` in Python and Node
- 10-8 [TEST] [P1] Retry policy fields are untested in every language
- 10-10 [FIX] [P1] CI builds the Python wheel only on macOS: add the Linux and Windows wheels, an sdist, and an install matrix
- 10-11 [FIX] [P1] Node: build prebuilt binaries per platform in CI and publish per-platform packages
- 10-12 [FIX] [P1] Prove "no Rust toolchain" on every platform: a clean-install job that cannot see cargo
- 10-13 [FIX] [P1] Release workflow for the bindings, with version-sync checks
- 09-3 [FIX] [P1] Redis cooldown writes must be monotonic (max wins), like Postgres `GREATEST`
- 09-4 [FIX] [P1] Guard fail policy must apply on the masking pass (`transform_request` / `transform_response`)
- 09-5 [DECIDE] [P1] `deny_only` / `post_hoc` guards given a masking rule silently do nothing
- 09-6 [DECIDE] [P1] Semantic cache key covers prompt text only
- 09-7 [FIX] [P1] MCP gateway token check must be constant-time and accept `Bearer` case-insensitively
- 09-9 [FIX] [P1] Config view redaction must not depend on a key-name denylist alone
- 09-10 [TEST] [P1] Admin usage views have no body-level test, and the test fixture's usage reader cannot work
- 09-11 [TEST] [P1] CI never builds the all-off or per-feature matrix
- 09-12 [TEST] [P1] Live suites are `#[ignore]`d, never run in CI, and some pass vacuously
- 09-13 [TEST] [P1] `[replica]` startup refusals have no test (families refusal exists but only in a feature-off build)
- 11-1 [DOC] [P1] Fix backlog #6 `crate` field: `llm-proxy-store` does not exist
- 11-2 [DOC] [P1] Record the dropped fingerprinting part of backlog #15
- 11-3 [DOC] [P1] Make `corrections_note` and the `crate` fields agree about crate names
- 11-4 [DOC] [P1] Correct the loopback rate-limit exemption comment in `config.toml.example`

### P2
- 01-8 [TEST] [P2] Assert OpenAI client `cache_control.ttl` reaches the core
- 01-9 [DOC] [P2] Correct stale/inexact claims in the plan text
- 04-11 [TEST] [P2] Direct provider route never fails over for 5xx and connect errors
- 04-12 [DOC] [P2] Pool config shape and validation names in the Done text
- 04-13 [TEST] [P2] Probe negative control and process-global counter
- 04-14 [TEST] [P2] A clean stream writes its terminal `ResponseCompleted` row
- 04-15 [DECIDE] [P2] Public paths for `classification`, `error_policy`, `health` (AGENTS: one path per item)
- 03-5 [DECIDE] [P2] Admin read path still opens SQLite through `storage::UsageReader`
- 03-10 [DOC] [P2] Correct the crate-table and transport claims in the plan/Done text
- 03-11 [DOC] [P2] Remaining wording corrections in plan 03 Done text
- 03-12 [DECIDE] [P2] Two `emit_response_failed` writers (engine and server)
- 03-13 [TEST] [P2] Per-family `TimeoutLayer` actually fires
- 03-15 [TEST] [P2] Custom `Router` seam is exercised
- 03-16 [TEST] [P2] Pin the client re-encode failure delta
- 03-17 [DOC] [P2] Stale `llm-proxy-provider` comment
- 02-9 [DECIDE] [P2] `output_config` into Anthropic providers + CI coverage of same-format passthrough
- 02-10 [DECIDE] [P2] Server tools cross-format
- 02-17 [TEST] [P2] `top_k` cross-format encode (Gemini and Anthropic)
- 02-20 [FIX] [P2] Warn-only drops that remain in Responses/Gemini/Chat encoders: classify and enforce
- 02-21 [DOC] [P2] Correct stale plan text
- 05-8 [DOC] [P2] Preflight: narrow the claim, no provider token-count call
- 05-11 [TEST] [P2] Engine-level proof that a key restricted to models fails closed with no model
- 05-12 [TEST] [P2] Replace mirrored tests with property/behaviour tests
- 05-13 [FIX] [P2] Remove the double public paths in the engine crate root
- 05-14 [DOC] [P2] Align plan 05 prose with the code
- 08-7 [DECIDE] [P2] Ollama `prompt_eval_count`: keep excluding cache reads, document it
- 08-9 [TEST] [P2] Make the Gemini same-format stream test actually check the stream URL
- 08-11 [TEST] [P2] Body limits apply to the new families
- 08-12 [DOC] [P2] "Byte-for-byte" claims are value-for-value, modulo model and prompt hygiene
- 08-13 [FIX] [P2] Remove the `let _ = index;` placeholder
- 08-14 [DOC] [P2] Stale ledger doc comments list only the old dialects/routes
- 08-15 [DOC] [P2] Plan 08 text corrections
- 07-3 [DOC] [P2] State where `/metrics` is available and that it needs `[storage]` outside the admin-ui listener
- 07-11 [DECIDE] [P2] "Swap one provider at a time": narrow the claim, keep whole-registry swap
- 07-12 [DOC] [P2] Config snapshot swap and engine swap are two steps: state the invariant
- 07-15 [FIX] [P2] `serve --background` parent must honour `server.log_format`
- 07-16 [TEST] [P2] Assert `no-store` on admin 401 and unknown-path responses for both admin routers
- 07-17 [DOC] [P2] OTel export is the scalar counters only; the "live collector" is an in-process sink
- 07-18 [TEST] [P2] OTLP test must assert a counter value, not just a name
- 07-19 [DOC] [P2] Provenance docs promise an announcement the code does not make
- 07-20 [DOC] [P2] Tighten the claim that the stress test "fails if regressed to two adjacent swaps"
- 07-21 [TEST] [P2] Cover `cmd_reload` (PID file -> SIGHUP)
- 06-3 [DECIDE] [P2] String-form `system` / message `content` get no markers today
- 06-9 [TEST] [P2] Direct regression test for the shared engine cell
- 06-10 [TEST] [P2] Known limitation X1: leader disconnect after commit
- 06-11 [DOC] [P2] Data-class policy wording: TTL start and the anonymous caller
- 06-13 [FIX] [P2] Double public path for the cache types (also: plan 05)
- 06-14 [DOC] [P2] mutation.rs module doc overstates "byte-for-byte"
- 06-15 [TEST] [P2] End-to-end check on the SQLite ledger that no prompt/response bytes are stored
- 09-8 [DOC] [P2] MCP gateway admission caller is `mcp-token-<n>`, not `mcp:<token-id>`
- 09-14 [TEST] [P2] Admin posture: the full admin surface is not on the main listener; state the `admin_on_main_listener` posture
- 09-15 [DOC] [P2] External-version and evidence statements in the plan
- 11-5 [DOC] [P2] (O1) Describe audit BUG-2 accurately where GA/CO say only "unrepresentable"
- 11-6 [DOC] [P2] (O2) Mark the `server.hot_reload` rejection as superseded inline
- 11-7 [DOC] [P2] (O3) Add a baseline scoping line to `repo-intelligence.md`
- 11-8 [DOC] [P2] (O4) Cross-reference the Upstash row in the CO coverage appendix
- 11-9 [DECIDE] [P2] Self-reported "verified against live documentation/GitHub" claims
- R-8 [TEST] [P2] Source guards must name `resolve_model_route` / `ModelRoute`
- R-10 [FIX] [P2] Add an `upstream_model` tracing field on request-path logs
- R-11 [TEST] [P2] Strengthen `same_model_routes_to_different_providers`; fix a misnamed test
- R-12 [DOC] [P2] Bring `docs/completed/remodel-proxy.md` in line with the code
- R-13 [TEST] [P2] count_tokens must work without any configured adapter
- R-14 [TEST] [P2] CLI tests that assert the plan's properties
- R-15 [TEST] [P2] Discovery parser coverage: Anthropic `has_more`, Fireworks, dedup
- R-16 [TEST] [P2] Discovery transport properties: timeouts, redirects, error-body sanitisation
- R-17 [TEST] [P2] Config-level tests missing for load-time validations
- R-18 [TEST] [P2] HTTP-level tests for unsupported route, ignored provider header, and startup discovery
- R-19 [TEST] [P2] `refresh=live` gating and last-known-good preservation
- R-20 [FIX] [P2] Remove `expect()` and slicing from non-test code in the plan area (AGENTS.md)
- R-21 [DECIDE] [P2] Provider-name length limit differs between config (64) and URL path validation (128)
- 10-9 [TEST] [P2] One shared runtime, and bridges can be dropped from async context
- 10-14 [DOC] [P2] Bring READMEs and the plan in line with CI as built
- 10-15 [TEST] [P2] Same-format passthrough through `execute` is only covered by a snapshot

