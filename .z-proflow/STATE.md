# HEADER

phase: implement
verdict: (looping)
next: spawn implementer PRX-01

# POLICY

1. Two consecutive rounds with no meaningful progress — open findings or
   failing gates not shrinking — stop; report honestly what passes, what
   fails, and a diagnosis.
2. Every routing-map area complete + all gates green + zero open findings
   → stop, report.
3. Findings strictly shrinking → continue, up to the run's round cap of 6
   per phase (an honesty valve, not a target).
Never claim clean while a gate is red.

# TASK

# llmproxy usage-truth ledger plan

Task prefix: PRX. Estimate: L (14-18 change sets). Order within the plan matters.

## Context

llmproxy already emits exactly one usage event per request; today those events go
nowhere, because there is no persistence backend. This plan makes the proxy the
usage-truth ledger: every request persisted to SQLite with token counts, cost, and
caller attribution, queryable through an authenticated admin API. Worth having with no
consumer in mind: per-request and per-provider accounting, named client keys, retention
control, and per-provider egress routing for geo-restricted upstreams. Downstream, a
separate tool (see Companion plans) reads these endpoints to answer "how much quota is
burned and what did it cost" across machines.

Baseline, verified at commit 7d91377: six crates, per-request events already defined in
crates/llm-proxy-storage/src/lib.rs (ProxyEvent, EventBus, NoopBus), events
emitted exactly once per request in crates/llm-proxy-server/src/routes/core_pipeline.rs,
the API crate an empty placeholder (crates/llm-proxy-api/src/lib.rs), no persistence
backend (the storage doc comment says SQLite "lands in a later phase"), no client
authentication, no upstream egress configuration
(crates/llm-proxy-provider/src/transport.rs builds one default reqwest client), and
CI at .github/workflows/ci.yml with no release pipeline.

## PRX-01 — client identity

Today the server identifies clients only by IP for rate limiting
(crates/llm-proxy-server/src/middleware.rs) and trusts forwarded headers only
opt-in (config trust_forwarded_headers). Add named client keys: [auth.clients] with
key ids and hashed secrets, Authorization: Bearer <client-key> accepted on proxy
routes, loopback-only default installs may set auth = "none" to preserve today's local
behavior; non-loopback binds refuse to start without client keys. RequestReceived
gains a caller field (key id, or "anonymous" under the loopback exemption).
Secrets are hashed at rest and never appear in events or logs; state.rs's documented
"none hold secrets" invariant is updated to cover the new auth state honestly.

## PRX-02 — SQLite EventBus backend

Implement EventBus (crates/llm-proxy-storage/src/lib.rs, trait EventBus) over SQLite
(rusqlite with the bundled feature, spelling and maintenance verified on crates.io per
llmproxy's AGENTS.md dependency rule). Schema: one requests table keyed by request_id
with received/completed timestamps, provider, model (requested and upstream), route
kind, client protocol, streaming flag, caller, token usage columns (input/output/
cache-read/cache-creation from the existing Usage type), cost (from the existing Cost
type), stop reason, error kind and http status for failures, latency, and body_hash
(already the dedup-safe digest, never the body). Indexes on (timestamp), (provider,
timestamp), (caller, timestamp). The writer is a dedicated task behind a bounded
channel, honoring the emit contract "must not block the calling task"; a full channel
drops the event and increments a counter, never backpressures a response. Retention:
configurable max age/rows, pruned on a timer. WAL mode; the database path is config
under [storage].

## PRX-03 — /usage admin endpoints

Fill the placeholder llm-proxy-api crate with the query surface, wired into the
server's router next to the existing lightweight routes
(crates/llm-proxy-server/src/routes/mod.rs):

- GET /usage/summary?window=…&group_by=provider|model|caller — tokens, requests,
  cost, error counts for a time window.
- GET /usage/requests?since=…&until=…&provider=…&limit=… — bounded paged raw rows,
  no request bodies, ever.
- GET /usage/keys — caller list with last-seen (admin only).

Admin authentication and mounting: a separate [auth.admin] token, required for every
/usage route. The default mount is a second, loopback-or-tunnel-only listener
([server.admin_bind], default 127.0.0.1 on a distinct port), so the admin API is not
part of the public surface unless the operator deliberately puts the tunnel or a
reverse proxy in front of it. An explicit admin_on_main_listener = true opts into also
serving /usage on the main listener behind the same token, for setups where a second
port is not available; that opt-in is a different, named security posture and the
config validator says so. Responses are read-only; there is no write path. The
quota-stats-compatible summary for CodexBar is optional, not required.

## PRX-04 — geo-exit upstream support

Two layers, both needed when the proxy must reach geo-restricted upstreams:

1. Per-provider egress: [provider.egress] proxy = "socks5h://host:port" (or an http
   proxy URL) in provider TOML; the transport builds a per-provider reqwest client with
   .proxy(...) instead of today's single default client
   (crates/llm-proxy-provider/src/transport.rs). No proxy means today's behavior.
2. Bind-out selection stays out of scope: exit geography comes from where the tunnel
   endpoint the forward proxy listens on runs, not from llmproxy source-address tricks.

The tunnel layer and the deploy flow that configures both ends live in companion
repos; this plan only honors the per-provider proxy URL.

## PRX-05 — release artifacts

There is CI (.github/workflows/ci.yml) but no release pipeline (no cargo-dist
config). Add cargo-dist producing musl Linux binaries plus a shell installer.
Companion deploy tooling consumes these artifacts; without them, deploy means building
on the target machine, which the plan does not want as the only path.

## PRX-06 — secret-abuse test family

Add a dedicated abuse family in llmproxy's own suite covering: client and admin
tokens never appearing in events, logs, error responses, or the SQLite file beyond
their hashed-at-rest rows; /usage responses containing no request bodies; admin routes
refusing unauthenticated and wrong-token calls on both mounts; caller attribution
surviving the loopback exemption without leaking the key id of a non-anonymous caller
to other callers.

## Tests

Gates: just fmt, just lint, just test per AGENTS.md. Event-exactly-once assertions
extended to caller attribution; SQLite writer under channel saturation drops and
counts rather than blocking; /usage endpoints refuse unauthenticated calls on both the
admin listener and the opt-in main-listener mount; retention pruning; provider egress
config validation; the PRX-06 abuse family.

## Exit gate

- llmproxy persists every request with caller attribution and serves /usage endpoints
  behind admin auth.
- Per-provider egress proxy config validated and honored by the transport.
- No client or admin token ever reaches an event, log, error response, or the SQLite
  file beyond its hashed-at-rest row; the PRX-06 family proves it.

## Out of scope

- Bind-out/source-address selection.
- Any write path on /usage; responses stay read-only.
- The quota-stats-compatible summary: optional, not required.

## Companion plans

The consumer-side work lives in the superai repository's subplans 15 and 16. This
document's canonical home is the llmproxy repository; the copy in the superai repo
under docs/plans/export/ is an export.

# BODY

## survey — init (main thread)

### routing map

| id | title | paths | skills | gates | depends-on |
| --- | --- | --- | --- | --- | --- |
| PRX-01 | Client identity — named keys, hashed secrets, caller attribution | crates/llm-proxy-storage, crates/llm-proxy-server (middleware.rs, state.rs, config wiring), config.toml.example | /home/thragg/.zcode/skills/axum-web-framework/SKILL.md; /home/thragg/.zcode/skills/rust-best-practices/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: none |
| PRX-02 | SQLite EventBus backend — the persistent usage ledger | crates/llm-proxy-storage, storage config wiring, config.toml.example | /home/thragg/.zcode/skills/rust-async-patterns/SKILL.md; /home/thragg/.zcode/skills/rust-fast-architecture/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: PRX-01 |
| PRX-03 | /usage admin endpoints — read-only query surface behind admin auth | crates/llm-proxy-api, crates/llm-proxy-server (routes, listeners), apps/llm-proxy, config.toml.example | /home/thragg/.zcode/skills/axum-web-framework/SKILL.md; /home/thragg/.zcode/skills/rust-best-practices/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: PRX-02 |
| PRX-04 | Per-provider egress proxy for geo-restricted upstreams | crates/llm-proxy-provider (transport.rs), crates/llm-proxy-core (provider_config.rs), provider TOML examples | /home/thragg/.zcode/skills/rust-async-patterns/SKILL.md; /home/thragg/.zcode/skills/rust-best-practices/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: none |
| PRX-05 | Release artifacts — cargo-dist musl binaries + shell installer | dist config at workspace root, .github/workflows (new release workflow only) | /home/thragg/.zcode/skills/rust-best-practices/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: none |
| PRX-06 | Secret-abuse test family — tokens never leak anywhere | test modules across crates/llm-proxy-storage, llm-proxy-api, llm-proxy-server | /home/thragg/.zcode/skills/rust-testing/SKILL.md | gates: the workspace gate set (fmt --check, clippy --locked -D warnings, lint-inheritance, test --all-features, cargo shear) | depends-on: PRX-01, PRX-02, PRX-03, PRX-04 |

### project rules

Extracted this session from AGENTS.md (read in full), clippy.toml (read in full), and
docs/SETUP-RUST-LINTS.agent.md (read in full).

Workspace shape (AGENTS.md:3-7): Rust workspace, edition 2024, MSRV 1.85, toolchain
pinned 1.96.1 in rust-toolchain.toml; binary apps/llm-proxy plus six crates
(crates/llm-proxy-{core,protocol,provider,storage,api,server}). `just` recipes
(`just fmt`, `just lint`, `just test`, `just build`, `just run`) wrap the cargo
commands.

Gates, run after every change (AGENTS.md:11-21):

    cargo fmt --all
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features

Unused dependencies are checked by `cargo shear` in CI, not the rustc
`unused_crate_dependencies` lint (useless in a workspace where each tests/*.rs is its
own target). Run `cargo shear` before adding or removing a dependency.

Hard rules (AGENTS.md:25-43, verbatim in substance):

- Never silence a lint with `#[allow(...)]`. Use `#[expect(lint, reason = "…")]` at the
  narrowest scope, or fix the code. Crate-level `#![allow(...)]` is forbidden.
- No `unwrap()`, `expect()`, `panic!`, `todo!`, or `unimplemented!` in non-test code.
  Propagate errors with `?`. Use `thiserror` for libraries, `anyhow` for applications.
- No indexing (`v[i]`) or string slicing (`&s[..n]`) in non-test code — they panic.
  Use `.get()`, and respect UTF-8 char boundaries when truncating strings.
- Never write `let _ = …` on a `Result` or a `Future`. Handle or log it.
- Every `unsafe` block needs a `// SAFETY:` comment stating the invariant.
- Never hold a `MutexGuard` across an `.await`.
- Prefer `&str` over `String` and `&[T]` over `Vec<T>` in function parameters.
- Do not add a dependency without checking it exists on crates.io, is spelled correctly,
  and is actively maintained. Verify before importing.
- Tests must assert observable behaviour. A test that restates a constant
  (`assert_eq!(RETRIES, 3)`) or mirrors the implementation's branches is worthless —
  test a property instead.
- Public items should be reachable through exactly one path. Do not add re-exports to
  paper over a refactor.
- Do not put design narratives, "why we chose X over Y" essays, or self-report tables
  into user-facing documentation.

clippy.toml (clippy.toml:1-14): test escapes default false, set explicitly —
allow-unwrap-in-tests = true, allow-expect-in-tests = true,
allow-indexing-slicing-in-tests = true, allow-panic-in-tests = true,
allow-dbg-in-tests = false. Thresholds: excessive-nesting-threshold = 5 (a no-op
without it; default 0), too-many-lines-threshold = 100,
too-many-arguments-threshold = 6, msrv = "1.85".

Lint discipline (docs/SETUP-RUST-LINTS.agent.md):

- Non-negotiables (lines 11-19): never `#[allow]` to make a lint pass —
  `#[expect(lint, reason = "…")]` with real justification or fix the code (`#[expect]`
  warns when the lint stops firing, so suppressions cannot rot). Never crate-level
  `#![allow(...)]` — if one seems needed, stop and report. Never mass-suppress. Never
  enable `clippy::restriction` as a group (mutually contradictory lints; cherry-pick
  only). Never enable `clippy::cognitive_complexity` (upstream says don't; use
  excessive_nesting and too_many_lines). Do not invent lint names — every lint must be
  verified to exist. Do not modify application logic for lint config beyond mechanical
  fixes; if a lint reveals a real bug, report it, do not silently redesign.
- Lint inheritance (lines 207-222): cargo does not inherit workspace lints
  automatically; every member Cargo.toml must carry `[lints] workspace = true`, and CI
  verifies none is missing.
- Triage (lines 293-301): mechanical fix exists → fix it; `clippy --fix` handles it →
  run it and review the diff; correct but behaviour-changing → leave it, report as
  "needs human decision", never suppress; genuine false positive → `#[expect]` at the
  narrowest possible scope; reveals an actual bug → stop and report prominently.
- Fix recipes (lines 310-327): `x.unwrap()` in a Result fn → `x?`; unwrap on Option →
  `let Some(x) = x else { return … };`; `v[i]` → `v.get(i).ok_or(…)?`; `&s[..n]` →
  `s.chars().take(n).collect::<String>()`; `let _ = fut;` → `fut.await` or
  `tokio::spawn(fut)`; guard across await → scope it in `{ … }` or use
  `tokio::sync::Mutex`; `fn f(s: String)` → `fn f(s: &str)`.
- Escalate to the human, never decide alone (lines 498-507): baseline warning count
  over 300; a lint that reveals a real bug; any believed need for crate-level
  `#![allow]`; a dependency name that is a near-miss of a popular crate (supply-chain
  red flag — report immediately, do not build); any request to weaken
  `allow_attributes`/`allow_attributes_without_reason`.

## implementer — PRX-01 — round 1

Client identity: named client keys with argon2id-hashed secrets, Bearer auth on
proxy routes, caller attribution on RequestReceived, key-id rate-limit keying,
loopback-only no-auth enforced at config load.

### Changed paths

- `crates/llm-proxy-core/src/provider_config.rs` — new `AuthConfig`
  (`[auth]`: `mode = "none"|"client-keys"`, `clients: HashMap<String,
  SecretString>` with `skip_serializing`), `AuthMode`, `ANONYMOUS_CALLER`
  const; `AppConfig.auth` (serde default, backward compatible); validation in
  `load_app_config` via new `validate_auth_config` (none+clients rejected,
  client-keys+empty rejected, none+non-loopback bind rejected before any
  listener, empty/reserved key ids rejected). 8 new tests.
- `crates/llm-proxy-core/src/lib.rs` — export `AuthConfig`, `AuthMode`,
  `ANONYMOUS_CALLER`.
- `crates/llm-proxy-storage/src/lib.rs` — `RequestReceived.caller: String`
  (key id or `"anonymous"`); 3 test constructions updated.
- `crates/llm-proxy-server/src/middleware.rs` — `ClientAuth` (argon2id
  verifiers parsed/validated at construction, verify on `spawn_blocking`,
  SHA-256 digest cache with constant-time compare so steady-state requests
  skip the KDF; manual Debug prints key ids only), `ClientAuthError`,
  `extract_bearer_token`, `CallerId`, `RequestMeta` extractor (bundles
  request id + caller + connect info; replaces 3 extractors per handler),
  unit tests incl. cheap-parameter argon2 hashing.
- `crates/llm-proxy-server/src/routes/mod.rs` — `require_client_auth` layer
  on the api router (after `inject_request_id`, before body limit): stamps
  `CallerId`, 401 via the standard `Unauthorized` envelope (no secret
  material, x-request-id attached, `ResponseFailed` emitted like other layer
  rejections), failed attempts charge the peer's IP bucket to bound the
  argon2 spend of garbage-token floods.
- `crates/llm-proxy-server/src/routes/core_pipeline.rs` —
  `RequestContext.caller`; `prepare_request(state, &RequestMeta, headers,
  body, path)`; rate limiter keyed by key id when authenticated (loopback
  bypass now applies only to anonymous callers), by client IP otherwise; both
  RequestReceived emit sites carry caller; 5 new/updated prepare_request
  tests.
- `crates/llm-proxy-server/src/routes/{messages,chat,token_count}.rs` —
  handlers take `RequestMeta`; emit sites carry `ctx.caller`.
- `crates/llm-proxy-server/src/state.rs` — `AppState.client_auth:
  Arc<ClientAuth>` (invalid config fails CLOSED: enforced, zero verifiers),
  `with_client_auth` builder, Debug chain + maintenance note updated
  ("hashed argon2id verifiers only, never plaintext secrets"), 2 new tests +
  literals fixed.
- `crates/llm-proxy-server/src/lib.rs` — export `CallerId`, `ClientAuth`,
  `ClientAuthError`.
- `apps/llm-proxy/src/state.rs` — `load_toml_state` builds `ClientAuth` from
  `[auth]` before state/router/listener: invalid verifier = startup error.
- `apps/llm-proxy/tests/cli_validate.rs` — end-to-end: non-loopback bind
  without keys rejected through `cmd_validate`; client-keys + non-loopback
  accepted (uses a real generated PHC string).
- `crates/llm-proxy-server/tests/*`, `src/routes/models.rs` — `AppConfig`
  literals gain `auth: Default::default()`; tests/core_pipeline.rs adds 5
  router-level auth tests (401 missing/wrong bearer with no token echo, valid
  bearer attributes caller on the event, anonymous attribution, /health open
  under enforcement).
- `Cargo.toml` + `crates/llm-proxy-server/Cargo.toml` — argon2 0.6 (verified
  on crates.io: RustCrypto, 0.6.0 published 2026-08-27, 61M+ downloads).
- `config.toml.example` — `[auth]` section with mode/clients docs and a PHC
  generation hint.

### Self-checks run (this round)

- `cargo test -p llm-proxy-server` — 177+39+60+40+13 passed, 0 failed.
- `cargo test -p llm-proxy-core` — 159+6 passed. `-p llm-proxy-storage` — 5 passed.
- `cargo test -p llm-proxy --test cli_validate` — 5 passed.
- `cargo test --workspace --all-features --no-fail-fast` — 1232 passed; ONLY
  failures are the 2 pre-existing `apps/llm-proxy/src/platform.rs` tests
  (desktop-entry snapshot + shell-escape), untouched by this area (file not
  in my diff; `.snap.new` predates this round).
- `cargo fmt --all` + `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets --all-features --locked` — my
  crates emit only warnings that pre-date this area (verified against HEAD:
  `handle_core_once`/`handle_core_stream` already had 7 params; token_count
  147→140 lines still over; chat_completions test fns already 126/112 lines).
  My area added zero new clippy warnings (the 7-arg handlers I briefly
  created were refactored away via `RequestMeta`). Full
  `... -- -D warnings` is still red on baseline (llm-proxy-protocol 4 errors
  in anthropic.rs/openai_chat.rs, plus pre-existing provider/app warnings) —
  not claimed green; deciding gate run belongs to the script.
- `cargo shear` — NOT RUN: cargo-shear is not installed in this environment
  and not installable offline. Manual equivalent: the one added dependency
  (argon2) is imported and used 28 times in llm-proxy-server src.

### Interface notes for dependent areas

- `RequestReceived` now carries `caller: String` (key id, or `"anonymous"`
  under the loopback exemption). PRX-02 must persist it; its planned `(caller,
  timestamp)` index keys on this field. Events never carry secrets — key ids
  only.
- Exports: `llm_proxy_core::{AuthConfig, AuthMode, ANONYMOUS_CALLER}`;
  `llm_proxy_server::{CallerId, ClientAuth, ClientAuthError}`.
  `AppState::with_client_auth(Arc<ClientAuth>)` is the test/builder hook;
  production builds it in `load_toml_state`.
- Config surface: `[auth] mode` + `[auth.clients] <id> = "<argon2id PHC>"`.
  Core validates mode/loopback/id rules at `load_app_config`; the server
  validates PHC parseability at `ClientAuth::from_config` (startup error in
  `load_toml_state`; infallible `AppState::new` fails CLOSED on bad config).
- Rate limiter: authenticated → keyed by key id, no loopback bypass;
  anonymous → keyed by IP, loopback bypass preserved (today's behavior).
  Failed auth attempts consume the peer's IP bucket (flood bound, uniform 401).
- Auth 401s emit exactly one `ResponseFailed` (no paired RequestReceived —
  same as 404/405/408/413 layer rejections). PRX-06 can assert on this.
- PRX-03's `[auth.admin]` token is separate and not built here.
- Test helpers for PRX-06: `client_auth_for(key_id, token)` in
  tests/core_pipeline.rs and `cheap_phc` in middleware.rs tests (cheap
  argon2 params, ~instant; cost params travel in the PHC string so
  production-cost hashes verify identically).

### Punted

- No `llm-proxy hash-key` CLI subcommand: not in the brief; the example
  config points at the `argon2` CLI instead. Key management may belong with
  PRX-03's admin surface.
- Bearer-style `passthrough_auth` providers + `mode = "client-keys"` interact
  badly: the Authorization header then carries the proxy client key, which
  passthrough would forward upstream. Needs a credential-channeling design
  decision (e.g. x-api-key for one of the two); config validation does NOT
  reject the combination. Flagging for the judge/PRX-06.
- `defaults.rs` DEFAULT_CONFIG_TOML unchanged (default stays none/loopback;
  `[auth]` is serde-defaulted so old configs parse untouched).
- Digest cache is in-memory only (cleared on restart; first request per key
  pays one argon2). No TTL/revocation beyond process restart.
- cargo shear not run (tool absent here) — see self-checks.

## implementer — PRX-01 — round 2

Gate-fix round. All remaining red was pre-existing baseline (none of it caused
by round 1): 2 platform tests in apps/llm-proxy and 19 clippy warnings across
protocol/provider/server/app. Fixed all of it; both workspace gates now pass
locally with the exact gate commands.

### Fixes

- `apps/llm-proxy/src/platform.rs`:
  - `format_desktop_entry_basic` snapshot: the stored `.snap` file did not
    exist (only a `.snap.new` was on disk, predating this run). Accepted the
    snapshot by committing the `.snap.new` content as the `.snap` — its
    `Exec=/usr/bin/llm-proxy serve` matches what the code correctly produces
    for special-char-free args per the XDG Exec rules.
  - `desktop_exec_escape_shell_metacharacters` was internally contradictory:
    `assert!(!escaped.contains('$'))` cannot hold together with
    `assert!(escaped.contains("\\$"))` (the escaped form contains the
    metacharacter by definition) — no implementation could pass it. Rewrote to
    the intended property: every `$`/backtick occurrence is backslash-escaped
    (strip the escaped forms, then assert no bare metacharacter remains).
  - cfg-gated the macOS-only `use crate::paths::config_dir` and the
    `regex_escape` test helper — the source of both app-lib clippy warnings
    on Linux builds (they are used only by `#[cfg(target_os = "macos")]`
    code).
- `crates/llm-proxy-server` (my area) — mechanical fixes where clean:
  - New `CoreDispatch<'a> { provider_name, route_kind, client_protocol }`
    bundle; `handle_core_once`/`handle_core_stream` now take
    `(state, ctx, dispatch, core, inbound_auth)` (7→5 params) — kills both
    `too_many_arguments` warnings; call sites in messages.rs/chat.rs updated.
  - Extracted `extract_countable_text(&CoreRequest) -> (String,
    Vec<MessageContent>)` out of `count_tokens_inner` (140→~75 lines) — kills
    its `too_many_lines` warning. Pure code movement.
  - `#[expect(clippy::too_many_lines, reason = "…")]` on `handle_core_once`,
    `handle_core_stream` (stage-order audit trail: each stage's metric +
    ResponseFailed emission stays next to the failing call),
    `build_sse_output_stream` (one streaming state machine; splitting the
    select arms would scatter the state they mutate), and the two barely-over
    chat_completions scenario tests (105/102 lines; single-narrative e2e
    scenarios).
- `crates/llm-proxy-protocol` — `#[expect(clippy::too_many_lines, reason)]`
  on the four wire-translation functions (anthropic `encode_event` 132;
  openai_chat `decode_request` 190, `encode_response` 113, `encode_event`
  171): each is one match arm per CoreEvent variant / field-by-field wire
  mapping kept in one place for audit against the API schema.
- `crates/llm-proxy-provider` — same treatment on the four adapter wire
  functions (responses `decode_frame`/`encode_request`, openai_chat
  `encode_request`, gemini `encode_request`) and the three fixture-walk test
  helpers (each walks one fixture's expected wire output; splitting scatters
  the assertions).

Rationale for expect-vs-split: AGENTS.md sanctions `#[expect(lint, reason)]`
as the alternative to fixing; every site carries a specific reason, and the
`#[expect]` anti-rot property (warns when the lint stops firing) keeps them
honest under `-D warnings`. Where a clean mechanical fix existed
(`too_many_arguments` bundling, `count_tokens_inner` text extraction) I did
the refactor instead.

### Gate runs (this round, exact commands)

- `cargo clippy --workspace --all-targets --all-features --locked -- -D
  warnings` → exit 0, zero warnings/errors (19 baseline warnings gone; every
  `#[expect]` fires, or the unfulfilled-expectation lint would have failed
  the gate).
- `cargo test --workspace --all-features` → exit 0; 1234 passed, 0 failed
  (28 suites, all ok).
- `cargo fmt --all --check` → clean.
- The script's own gate run remains the deciding one; the above is what I ran
  locally.

### Interface notes

- `handle_core_once`/`handle_core_stream` signatures changed to take
  `CoreDispatch` — pub(crate), internal to the server crate; no external
  surface changed from round 1.

### Punted

- cargo shear still not run (binary absent from this environment; no
  dependency changes this round).

## committer — PRX-01

- Commit: `499b22a8209686574420023c7ee1491a87ac0ae3` on branch
  `prx/usage-ledger` (branched from master @ 7d91377).
- Message: "Add named client keys with caller attribution".
- Staged paths (32): .gitignore; .z-proflow/STATE.md; Cargo.lock; Cargo.toml;
  apps/llm-proxy/src/platform.rs;
  apps/llm-proxy/src/snapshots/llm_proxy_app__platform__tests__desktop_entry_basic.snap;
  apps/llm-proxy/src/state.rs; apps/llm-proxy/tests/cli_validate.rs;
  config.toml.example; crates/llm-proxy-core/src/lib.rs;
  crates/llm-proxy-core/src/provider_config.rs;
  crates/llm-proxy-protocol/src/client/anthropic.rs;
  crates/llm-proxy-protocol/src/client/openai_chat.rs;
  crates/llm-proxy-provider/src/adapter/gemini.rs;
  crates/llm-proxy-provider/src/adapter/openai_chat.rs;
  crates/llm-proxy-provider/src/adapter/responses.rs;
  crates/llm-proxy-provider/tests/fixture_tests.rs;
  crates/llm-proxy-server/Cargo.toml; crates/llm-proxy-server/src/lib.rs;
  crates/llm-proxy-server/src/middleware.rs;
  crates/llm-proxy-server/src/routes/chat.rs;
  crates/llm-proxy-server/src/routes/core_pipeline.rs;
  crates/llm-proxy-server/src/routes/messages.rs;
  crates/llm-proxy-server/src/routes/mod.rs;
  crates/llm-proxy-server/src/routes/models.rs;
  crates/llm-proxy-server/src/routes/token_count.rs;
  crates/llm-proxy-server/src/state.rs;
  crates/llm-proxy-server/tests/chat_completions.rs;
  crates/llm-proxy-server/tests/core_pipeline.rs;
  crates/llm-proxy-server/tests/integration.rs;
  crates/llm-proxy-server/tests/openrouter_e2e.rs;
  crates/llm-proxy-storage/src/lib.rs.
- The ask's explicit list was staged in full; the out-of-list files above
  (core config/storage, server middleware/state/lib/routes-mod/models,
  server + app tests, Cargo manifests + lock, config example) were staged as
  area-relevant per the PRX-01 routing map and the implementer rounds in this
  file — the listed route/protocol files reference types defined only in
  them (RequestMeta/CallerId, RequestReceived.caller, AppConfig.auth), so the
  commit would not compile without them. `.zcode/` left untracked (session
  tooling, not area code).
- Housekeeping: `.z-proflow/` appended to .gitignore in the same commit;
  STATE.md itself force-added (`git add -f`, explicit path) so it stays
  tracked while the rest of the directory is ignored.
- No hooks configured (.git/hooks samples only, no core.hooksPath), so none
  ran; no --no-verify used. Gates were not re-run by the committer — the ask
  states PRX-01 is green on the full workspace gate set; that run belongs to
  the script.

## judge — PRX-01 — round 1

Audited commit 499b22a8209686574420023c7ee1491a87ac0ae3 read-only against
AGENTS.md, the STATE.md POLICY/TASK/project-rules, the [workspace.lints] wall
plus clippy.toml, and the PRX-01 brief.

### Gates run (exact commands, this round)

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  — exit 0, zero warnings.
- `cargo test --workspace --all-features` — exit 0, every suite ok.
- `cargo shear` — "no issues found" (cargo-shear 1.14.0 installed this round
  with `cargo +stable install cargo-shear --locked`; the pinned 1.96.1
  toolchain cannot build it — its ra_ap_* deps require rustc 1.98).
- Dependency rule: `cargo search argon2` + `cargo info argon2` — argon2 0.6.0
  exists, RustCrypto/password-hashes, rust-version 1.85 (matches workspace
  MSRV), MIT OR Apache-2.0. Rule satisfied.
- docs/protocol.md does NOT document the RequestReceived shape (its line 71
  "caller metadata" refers to CoreRequest and predates this area), so the
  brief's conditional doc update does not apply.
- Loopback refusal ordering verified: `load_toml_state` (serve.rs:62) runs
  `load_app_config` → `validate_auth_config` and `ClientAuth::from_config`
  before `bind_listener` (serve.rs:103) — refusal precedes any bind.

### Findings

1. MAJOR (fixRequired) — crates/llm-proxy-server/src/routes/mod.rs:276-279
   and 303-316: the layer's doc comment claims "once [the IP bucket] is
   empty, further failures skip the (CPU-costly) argon2 verification
   entirely, so a flood of garbage tokens cannot pin the blocking pool" (and
   the inline comment claims "flood guard first (bounds the argon2 spend)"),
   but authentication at mod.rs:290-293 runs the full argon2 verification on
   EVERY attempt before the limiter is consulted, and the limiter result
   never gates anything (it only charges the bucket and records a metric).
   The claimed CPU-exhaustion bound does not exist: a garbage-token flood
   runs N argon2 hashes per request (N = configured verifiers) on the
   blocking pool indefinitely, unauthenticated. Fix this run: check the
   limiter BEFORE verification and reject without verifying when exhausted
   (uniform 401 either way), or rewrite both comments to state the truth
   (failures merely charge the bucket).
2. MAJOR (defer to user) — crates/llm-proxy-server/src/routes/
   core_pipeline.rs:452-462 with extract_bearer_token (routes/mod.rs:290):
   `passthrough_auth = true` providers resolve their upstream api_key from
   the inbound `Authorization: Bearer` header, which under `mode =
   "client-keys"` is the proxy client key — so that combination silently
   transmits the proxy client secret to the operator-configured upstream
   endpoint as the provider credential, and config validation does not
   reject it. The implementer flagged this honestly as punted; it needs a
   deliberate design decision (reject the combination at startup, or channel
   proxy auth via a distinct header) and must land by PRX-06's exit gate at
   the latest. Not a PRX-01 brief requirement — user's call on the design.
3. NIT (defer to user) — crates/llm-proxy-server/src/middleware.rs:365:
   `OptionalConnectInfo` lost its last users in this commit (all handlers
   moved to `RequestMeta`) and is now dead public API on the crate surface;
   delete it.

### Also checked, no finding

- Non-test panic paths: none added (the two pre-existing
  `Regex::new(...).expect()` at core_pipeline.rs:308/2308 date to c2437c8,
  2026-06-09 — baseline, not this area).
- No indexing/slicing, no `let _ =` on Result/Future, no MutexGuard across
  await (locks in `cached_key_id`/`verify_uncached` are scoped after/without
  awaits), no `#[allow]` (all new `#[expect]`s carry reasons at narrow
  scope), no crate-level allows.
- 401 body is the static "missing or invalid client auth token" envelope
  (error_response.rs:473-477); no secret material echoed; tests assert the
  no-echo property.
- Secrets: verifiers PHC-hashed at rest, SecretString-wrapped, redacted in
  Debug for both AuthConfig and ClientAuth; events/logs carry key ids only.
- Re-exports single-path (`llm_proxy_core::{AuthConfig,AuthMode,
  ANONYMOUS_CALLER}`, `llm_proxy_server::{CallerId,ClientAuth,
  ClientAuthError}`).
- Digest cache bounded (only verified tokens cached; ≤ one entry per key id).
- config.toml.example documents [auth] without design narratives; default
  DEFAULT_CONFIG_TOML untouched (loopback default still parses under
  serde-defaulted [auth]).

Verdict: findings — 1 major fix-required (doc/code mismatch on the auth
flood guard), 1 major deferred (passthrough × client-keys secret channeling),
1 nit (dead extractor). All workspace gates green as run above.

## implementer — PRX-01 — round 3

Audit-fix round. The audit (major) found that `require_client_auth`'s doc
claimed failed attempts "skip the argon2 verification once the IP bucket is
empty", but the round-1 code ran the full argon2 verification on EVERY
attempt before consulting the limiter, and the limiter result never gated
verification — it only charged the bucket and recorded a metric. Both the
comment and the security property were false. Fixed by implementing the
pre-verification gate (the audit's preferred option), not by weakening the
comment.

### Fix

- `crates/llm-proxy-server/src/middleware.rs` — new
  `RateLimiter::would_allow(client_ip) -> bool`: a non-consuming peek at the
  peer's refilled balance (unknown peer → true, disabled limiter → true;
  same refill math as `try_consume`, no mutation of `last_refill`).
- `crates/llm-proxy-server/src/routes/mod.rs` — `require_client_auth`
  restructured: the peer IP is extracted up front; when a bearer token is
  present, `would_allow` gates BEFORE `authenticate` runs — a peer with an
  exhausted failure budget gets the uniform 401 without paying argon2
  (records `rate_limited`). The failure path still charges the bucket via
  `is_allowed` so repeated failures drain it and trip the gate; successful
  authentications are never charged (their limits live under the key id in
  `prepare_request`). Doc comment rewritten to describe exactly this flow,
  including the explicit caveat that `rate_limit_rpm = 0` (limiter disabled)
  means no bound applies.

### Tests (observable behavior, would fail on the round-1 code)

- `routes::auth_gate_tests::exhausted_failure_budget_gates_verification_before_argon2`:
  rate_limit_rpm = 1; failure #1 (wrong token) → 401 with `rate_limited == 0`
  (it consumed the last token); failure #2 carries the VALID token → 401 and
  `rate_limited >= 1`. On the round-1 code #2 would have authenticated and
  the probe handler would have answered 200, so this test pins the gate
  ordering.
- `routes::auth_gate_tests::valid_token_passes_when_the_peer_has_budget`:
  control — a valid token on a fresh budget still reaches the handler (200).
- `middleware::tests::would_allow_*` (3 tests): drained peer → false,
  other peers unaffected; unknown peer / disabled limiter → true; repeated
  peeks neither drain nor refill (subsequent consume still denied).

### Gate runs (this round, exact commands)

- `cargo clippy --workspace --all-targets --all-features --locked -- -D
  warnings` → exit 0, zero warnings/errors.
- `cargo test --workspace --all-features` → exit 0; 1239 passed, 0 failed
  (28 suites, all ok).
- `cargo fmt --all --check` → clean.
- The script's own gate run remains the deciding one.

### Interface notes

- `RateLimiter::would_allow` is a new public method on `llm_proxy_server::
  RateLimiter` (pure read; `#[must_use]`). No other signature changes.
- Semantics note for PRX-06: a peer that exhausts its failure budget is
  rejected pre-verification even with a valid token until the bucket refills
  (rate_limit_rpm/60 per second); the response is a uniform 401 on every
  rejection path, so the gate is not peer-observable.

### Punted

- None this round beyond the standing cargo-shear note (binary still absent
  from this environment; no dependency changes this round).
