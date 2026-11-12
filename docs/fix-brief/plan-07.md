# Plan 07 — Hardening and operations: fix spec

Source: verification report `reports/plan-07.md`, plan `docs/completed/07-hardening-and-operations.md`.
All paths are relative to the repository root. Line numbers were re-checked statically against the
current tree (they drift; trust the symbol name).

## Done — leave alone
- Host/Origin/Sec-Fetch gate (Host, Origin-with-Fetch-Metadata, Sec-Fetch-Site; stands down under client-keys): `crates/llm-proxy-server/src/host_guard.rs` `enforce_host_origin`, `host_is_allowed`, `origin_is_allowed`, `sec_fetch_site_is_allowed`; tests `crates/llm-proxy-server/tests/host_guard.rs`.
- Cleartext-credentials refusal at load, loopback exemption, shared by `validate` and startup: `provider_config.rs` `validate_cleartext_endpoint`, `host_is_machine_local`; `apps/llm-proxy/src/commands/validate.rs` (only the *message content* is touched by 07-14).
- `/usage` router-level `Cache-Control: no-store` + `Pragma: no-cache` incl. 401: `crates/llm-proxy-api/src/routes.rs` `UsageApi::router`; `crates/llm-proxy-admin/src/routes.rs` router layers; test `usage_api.rs::every_admin_response_forbids_caching`.
- Self-loop literal/hostname refusal and 508 ingress: `crates/llm-proxy-core/src/self_loop.rs` `self_loop_problems`, `host_port_of`; `crates/llm-proxy-server/src/loop_guard.rs` `refuse_looped_requests`; `ProxyClient` egress stamp `crates/llm-proxy-transport/src/client.rs:175`.
- `log_format` honoured in `serve` (config before subscriber, `RUST_LOG_FORMAT` wins): `serve.rs::load_serve_state`, `state.rs::startup_log_format`, `LogFormat::effective`; binary-level test `apps/llm-proxy/tests/cli_serve.rs` module `log_format_startup`.
- Config provenance report: `apps/llm-proxy/src/provenance.rs`, printed by `validate.rs`.
- Prometheus text + token-gated `/metrics` on the `UsageApi` surface (`crates/llm-proxy-core/src/metrics.rs::to_prometheus_text`, `llm-proxy-api` `/metrics`); OTLP push `apps/llm-proxy/src/otel_metrics.rs::start_otlp_metrics`.
- In-flight `state` and `elapsed_ms`: `crates/llm-proxy-storage/src/usage_query.rs`; test `usage_api.rs::request_rows_report_state_and_in_flight_elapsed_time`.
- `doctor` command skeleton, `--probe`, exit code 1 on FAIL: `apps/llm-proxy/src/commands/doctor.rs` (fixes in 07-9, 07-10).
- `SwappingConfigSource` whole-snapshot swap, precedence doc: `crates/llm-proxy-engine/src/traits.rs`; tests `crates/llm-proxy-engine/tests/config_snapshots.rs`.
- Hot reload mechanics, restart-required prefix, SIGHUP end-to-end against the real binary (`apps/llm-proxy/tests/cli_serve.rs` module `hot_reload_signal::sighup_swaps_a_changed_providers_upstream`, which DOES distinguish old and new upstream by marker), managed-key attribution: `apps/llm-proxy/src/reload.rs`, `commands/serve.rs::spawn_reload_listener`.
- Atomic managed-key pair: `crates/llm-proxy-server/src/auth_pair.rs` `SwappingAuthPair` (module private, `pub use` at `lib.rs:24`); Redis replica runtime test `apps/llm-proxy/tests/redis_replica_managed_keys.rs`; `state.rs::refuse_key_store_without_admin_ui`.

## Work items

### 07-1 [FIX] [P0] Make `restart_required_changes` refuse `auth.admin.key_store`, `[replica]` and `[mcp]`
- **Where:** `crates/llm-proxy-core/src/provider_config.rs:2944` `restart_required_changes` (doc comment at ~2928-2943); callers `apps/llm-proxy/src/reload.rs:90` `reload_registry`. Types: `AppConfig` (line 24, fields `replica`, `mcp`), `AdminAuthConfig` (1044, field `key_store: Option<PathBuf>`), `ReplicaConfig` (64), `McpGatewayConfig` (99), `McpUpstreamConfig` (84).
- **Now:** the function compares `server.*` (rendered JSON), `auth.mode`, `auth.clients`, the admin `token` only, and `storage`. A SIGHUP that edits `[auth.admin] key_store`, the top-level `[replica]` or the top-level `[mcp]` produces an empty change list, so `reload_registry` proceeds and `state.config_source().swap(next_config)` (reload.rs:152) publishes a snapshot that disagrees with the running key store, Redis/Postgres stores and MCP listener. This is a silent half-apply.
- **Wanted:** plan Done 11: "anything under `[server]`/`[auth]`/`[storage]` is refused whole, with a stable `restart-required:` error naming the fields." `[replica]` and `[mcp]` are wired exactly like `[storage]` (stores and listeners built at startup), so they get the same treatment.
- **How:**
  1. In `restart_required_changes` after the admin-token comparison add `key_of = |c: &AppConfig| c.auth.admin.as_ref().and_then(|a| a.key_store.clone())`; if they differ push `"auth.admin.key_store"`. (Keep the existing `"auth.admin"` entry for token changes.)
  2. Add `fn replica_matches(a: &Option<ReplicaConfig>, b: &Option<ReplicaConfig>) -> bool`: both `None`, or both `Some` with equal `backend` (`ReplicaBackend` is `PartialEq`), equal `prefix`, and equal `url.expose_secret()`. Push `"replica"` when not equal. `ReplicaConfig` has no `PartialEq` and `url` is `skip_serializing`, so do not compare through `serde_json`.
  3. Add `fn mcp_matches(...)`: both `None`, or both `Some` with equal `bind`, equal `session_ttl`, tokens equal as ordered lists of `expose_secret()`, and `upstreams` equal pairwise on `name`, `url`, and `token.as_ref().map(|t| t.expose_secret())`. Both `tokens` and `McpUpstreamConfig::token` are `skip_serializing`, so a rendered-JSON comparison would MISS token rotation. Push `"mcp"` when not equal. Never render the secret values into the change string.
  4. Update the doc comment above the function to list the new fields.
  5. Update `docs/completed/07-hardening-and-operations.md` Done 11 to read "anything under `[server]`, `[auth]` (including `[auth.admin] key_store`), `[storage]`, `[replica]` or `[mcp]` is refused whole" and the module doc of `apps/llm-proxy/src/reload.rs` (line ~7-8) likewise.
- **Test:** extend `provider_config.rs::tests` (near `restart_required_changes_covers_auth_and_storage`, ~line 4100) with `restart_required_changes_covers_key_store_replica_and_mcp`:
  - `next.auth.admin = Some(AdminAuthConfig{ token: same, key_store: Some("keys.db".into()) })` against a current with the same token and `key_store: None` -> result contains `"auth.admin.key_store"` and NOT `"auth.admin"`.
  - `next.replica = Some(ReplicaConfig{ backend: Redis, url: "redis://a".into(), prefix: "p".into() })` vs `None` -> contains `"replica"`; then both `Some` with only `url` differing -> still `["replica"]`; identical -> empty.
  - `next.mcp = Some(McpGatewayConfig{ bind: 127.0.0.1:9100, tokens: vec!["t1".into()], upstreams: vec![], session_ttl: 1800s })` vs `None` -> `["mcp"]`; both `Some` differing ONLY in `tokens[0]` secret -> `["mcp"]` (this is the case a JSON compare would miss); identical -> empty.
  - A message check that no returned string contains `"t1"` or `"redis://"`.
  Add one reload-level test in `apps/llm-proxy/src/reload.rs::tests` (`a_key_store_edit_is_refused_as_restart_required`): start state from a config with `[auth.admin]` and no key_store (needs `client-keys` auth or `none`; copy the config helper used by `a_managed_keys_auth_diff_is_attributed_not_blamed_on_the_file`), rewrite config adding `key_store = "<tmp>/k.db"`, call `reload_registry`, assert the error starts with `restart-required:` and contains `auth.admin.key_store`, and `Arc::ptr_eq` of `state.engine()` before/after (nothing applied). If constructing a state with `[auth.admin]` + admin-ui feature gating is awkward, gate the test with the same `#[cfg(...)]` the sibling tests use.
- **Done when:** `rg -n 'auth.admin.key_store|"replica"|"mcp"' crates/llm-proxy-core/src/provider_config.rs` shows the three pushes; the new tests pass; plan Done 11 text lists the five sections.
- **Risk / notes:** `managed_keys_note` in reload.rs inspects only `auth.clients*` entries so it is unaffected. A config that merely re-states `[mcp]`/`[replica]` unchanged must still reload (identical-case assertion above). `${VAR}` interpolation happens before parsing, so comparison is on resolved values.

### 07-2 [FIX] [P1] Serve `/metrics` on the `admin-ui` dedicated admin listener
- **Where:** `crates/llm-proxy-admin/src/lib.rs` `AdminSurface` (struct at ~line 36; builder methods `keys`, `config`, `usage`), `crates/llm-proxy-admin/src/routes.rs:35` `router` (route table, layers at lines 43-58), `apps/llm-proxy/src/commands/serve.rs:~341-357` `bind_admin_listener` (`#[cfg(feature = "admin-ui")]` block). Reference implementation: `crates/llm-proxy-api/src/routes.rs:100` `prometheus_metrics` and `UsageApi::with_metrics`.
- **Now:** under `admin-ui` the dedicated listener serves `AdminSurface::router()`, whose route table (`/`, `/keys`, `/keys/{id}/revoke`, `/config`, `/usage/summary|requests|keys`, `/mcp`) has no `/metrics`. Only `UsageApi` (which needs `[storage]`) has one. An operator with the default `admin-ui` build and a Prometheus scraper gets 404.
- **Wanted:** plan Done 7: "the token-gated `/metrics` route on the admin surface."
- **How:**
  1. `AdminSurface`: add field `pub(crate) metrics: Option<Arc<llm_proxy_core::Metrics>>` (the crate already depends on `llm-proxy-core`), initialised `None` in `new`, update `Debug` (`has_metrics`), add builder `pub fn metrics(mut self, metrics: Arc<llm_proxy_core::Metrics>) -> Self` with a doc comment, and `pub(crate) fn metrics_ref`.
  2. `routes.rs::router`: after the existing routes, `let mut routes = ...; if state.surface.metrics.is_some() { routes = routes.route("/metrics", get(prometheus_metrics)); }` (mirror `UsageApi`: the route does not exist unless attached, so the surface shape is unchanged for embedders). Handler returns `200` with `content-type: text/plain; version=0.0.4; charset=utf-8` and body `metrics.get_snapshot().to_prometheus_text()`. It sits inside `require_admin` and inherits the no-store/nosniff layers.
  3. `serve.rs::bind_admin_listener` (admin-ui branch): `surface = surface.metrics(std::sync::Arc::clone(state.metrics()));` (`AppState::metrics()` already exists and is used by `admin_router`).
  4. Update the `AdminSurface::router` doc comment to list `/metrics`.
- **Test:** `crates/llm-proxy-admin/tests/admin_surface.rs`: `metrics_route_serves_prometheus_text_behind_the_admin_token`. Build `AdminSurface::new(ADMIN_TOKEN).metrics(m)` where `m = Arc::new(Metrics::new())` with `m.record_request(false); m.record_failure();`. Assert (a) no token -> 401, wrong token -> 401; (b) admin token -> 200, `content-type` equals `text/plain; version=0.0.4; charset=utf-8`, `cache-control` equals `no-store`, body contains `llm_proxy_requests_total 1` and `llm_proxy_requests_failed_total 1`. Add `without_metrics_attached_the_route_does_not_exist` using the existing `surface()` helper: authed `GET /metrics` -> 404. This fails before the change (404 in the first test).
- **Done when:** `rg -n '"/metrics"' crates/llm-proxy-admin/src/routes.rs` matches; both tests pass; `bind_admin_listener` passes `state.metrics()`.
- **Risk / notes:** `llm-proxy-admin` is only compiled with `admin-ui`; do not add `llm-proxy-admin` to server crates. `cargo shear`: no new dependency needed. Admin-on-main-listener (`admin_on_main_listener = true`) still merges `UsageApi` (needs `[storage]`), see 07-3.

### 07-3 [DOC] [P2] State where `/metrics` is available and that it needs `[storage]` outside the admin-ui listener
- **Where:** `docs/completed/07-hardening-and-operations.md` Done 7; doc comment of `crates/llm-proxy-server/src/lib.rs:43-45` `admin_router`.
- **Now:** Done 7 promises a `/metrics` route "on the admin surface" without conditions. `admin_router` returns `None` without `[storage]` (`let storage = ...as_ref()?;`) and without the `admin` feature; so on the non-`admin-ui` build, or with `admin_on_main_listener`, `/metrics` exists only when `[storage]` and `[auth.admin]` are both configured. (Without `admin-ui`, `bind_admin_listener` bails with "admin API enabled but no /usage router could be built" when `[storage]` is absent, so the failure is loud, not silent.)
- **Wanted:** the doc states the condition. Decision (recommended, hybrid): after 07-2 the dedicated listener under `admin-ui` serves `/metrics` without `[storage]`; the `UsageApi` surfaces keep needing `[storage]` because they share a router with the ledger views. Do not refactor `UsageApi` to be storage-optional.
- **How:** edit Done 7 to: "`Snapshot::to_prometheus_text` + a token-gated `/metrics` route: on the `admin-ui` dedicated admin listener (no `[storage]` needed), and on the `/usage` surface (dedicated or `admin_on_main_listener`) when `[storage]` is configured." Add the same sentence to the `admin_router` doc.
- **Test:** none (doc). 
- **Done when:** the Done 7 text contains "`[storage]`" and "`admin-ui`".
- **Risk / notes:** none.

### 07-4 [FIX] [P1] Stamp the hop header on discovery requests
- **Where:** `crates/llm-proxy-transport/src/discovery.rs:170` `DiscoveryClient::build_request` (called from `fetch_page`, line ~121); for contrast `crates/llm-proxy-transport/src/client.rs:175`.
- **Now:** `build_request` sets auth + Anthropic version + configured headers, but not `HOP_HEADER`. A discovery endpoint that points back at an llm-proxy is not answered 508 and can loop; the plan Done 4 states "every upstream request carries `x-llm-proxy-hop: 1`", which is false for discovery.
- **Wanted:** plan item 4: "add a hop header on egress"; Done 4: "every upstream request carries `x-llm-proxy-hop: 1`."
- **How:** in `build_request`, before the configured-headers loop, `request = request.header(llm_proxy_core::HOP_HEADER, llm_proxy_core::HOP_HEADER_VALUE);` (use the same call shape as client.rs:175; `llm_proxy_core` is already a dependency of the transport crate). Decision for `fetch_json` (pricing catalogue fetch, a public JSON URL, not a provider upstream): leave unstamped and say so in its doc comment; the Done 4 wording becomes "every provider upstream request (chat, discovery)". Edit the plan text accordingly.
- **Test:** in `discovery.rs::tests` add `discovery_requests_carry_the_hop_marker`: a handler `async fn echo_hop(headers: HeaderMap) -> Json<Value>` returning `{"data":[{"id": <value of x-llm-proxy-hop or "missing">}], "has_more": false}` (model it on `echo_anthropic_version`, line ~398); build with `discovery_provider(endpoint, ProviderDiscoveryKind::OpenAiCompatibleModels, HashMap::new())`; `DiscoveryClient::default().discover(&provider)`; assert `models[0].id == "1"`. Fails before the fix (`"missing"`).
- **Done when:** the test passes; `rg -n 'HOP_HEADER' crates/llm-proxy-transport/src/discovery.rs` matches.
- **Risk / notes:** the MCP gateway uses `ProxyClient` (`crates/llm-proxy-mcp/src/lib.rs`), so its upstream calls are already stamped. See also 07-22 (discovery also ignores `[provider.egress]`).

### 07-5 [FIX] [P1] Config-time self-loop check must understand wildcard binds
- **Where:** `crates/llm-proxy-core/src/self_loop.rs:78` `lands_on_listener`; caller `self_loop_problems` (line 100).
- **Now:** arm 1 requires `addr.ip() == listener.ip()`; arm 2 requires `listener.ip().is_loopback()`. For `bind = "0.0.0.0:P"` or `[::]:P` neither holds for an upstream at `http://127.0.0.1:P/...`, so `validate` and startup accept a self-referential upstream. The runtime 508 stops the loop after one hop but the plan's refusal is not met.
- **Wanted:** plan item 4: "Refuse an upstream that resolves to the proxy's own listener."
- **How:** add an arm before the loopback arm: when `listener.ip().is_unspecified()` and `addr.port() == listener.port()`, a hit if `addr.ip().is_loopback() || addr.ip().is_unspecified()`. (A wildcard listener accepts every loopback spelling of either family on that port; `[::]` is dual-stack on the platforms we ship, and the existing code already treats `127.0.0.1` and `::1` as the same loopback.) Update the doc comment of `lands_on_listener`. Residual (document in the fn doc, do not implement): an upstream naming one of the host's own non-loopback interface addresses on the same port is still only caught by the runtime 508.
- **Test:** `self_loop.rs::tests`: `a_wildcard_bind_catches_loopback_upstreams_on_its_port` — `listeners = ["0.0.0.0:8080".parse().unwrap()]`; providers `http://127.0.0.1:8080/v1/chat/completions`, `http://localhost:8080/...`, `http://[::1]:8080/...` each yield exactly one problem; `http://127.0.0.1:8081/...` and `http://10.1.2.3:8080/...` yield none. Repeat the first case with `"[::]:8080"`. These fail before the change.
- **Done when:** tests pass; `validate` against a config with `bind = "0.0.0.0:8080"` and a `127.0.0.1:8080` upstream errors with "own listener".
- **Risk / notes:** `0.0.0.0` binds are only legal with client-keys auth, so this is the common production shape. Make sure `other_ports_and_hosts_are_clean` still passes.

### 07-6 [FIX] [P1] Include the MCP gateway listener in the self-loop listener list
- **Where:** `apps/llm-proxy/src/state.rs:509-515` `load_registry_with_checks` (`own_listeners`).
- **Now:** `own_listeners` = main bind + (admin bind when a dedicated admin listener is served). `app_config.mcp.bind` (the MCP gateway's dedicated listener, `McpGatewayConfig::bind`) is missing, so an upstream pointed at the MCP port passes.
- **Wanted:** "resolves to the proxy's own listener(s)" — the MCP gateway is one of this instance's listeners.
- **How:** after the admin push: `if let Some(mcp) = app_config.mcp.as_ref() { own_listeners.push(mcp.bind); }`. Update the comment above (it says "main bind plus the admin bind"). Keep the single shared call so `validate`, `doctor` and reload inherit it. Optional follow-up (note only): `[mcp.upstreams].url` pointing back at the gateway is a separate loop; not covered by this plan.
- **Test:** `apps/llm-proxy/src/state.rs::tests` (or `commands/validate.rs::tests`, next to `cmd_validate_rejects_an_upstream_that_resolves_to_the_own_listener`, line ~231): `an_upstream_on_the_mcp_listener_is_refused` — config with `[mcp] bind = "127.0.0.1:<P>" tokens = ["t"]` and a provider endpoint `http://127.0.0.1:<P>/v1/messages`; `load_registry_with_checks` (or `cmd_validate`) returns an error containing "self-referential". Gate on `mcp-gateway` only if the config parser needs it (the `[mcp]` section parses regardless of the feature).
- **Done when:** the test passes.
- **Risk / notes:** none.

### 07-7 [DECIDE] [P1] Foreign `Origin` without Fetch Metadata: keep the carve-out, fix the done-when
- **Where:** `crates/llm-proxy-server/src/host_guard.rs` `request_carries_fetch_metadata`, `enforce_host_origin`; plan `docs/completed/07-hardening-and-operations.md` "Done when" bullet 1.
- **Now:** a request with `Origin: https://evil.example` and no `Sec-Fetch-*` is admitted (test `a_non_browser_request_with_an_origin_header_passes` asserts it). The "Done when" bullet says "A request with a foreign `Host` or `Origin` is refused in `mode = "none"`."
- **Options:** (a) refuse every foreign `Origin` regardless of Fetch Metadata; (b) keep the carve-out and qualify the done-when.
- **Recommended default: (b).** Evidence in the module doc: captured Claude Code traffic sends an `Origin` with no browser markers (the contract-replay fixtures), so (a) would break a real client. Every browser that can attack a loopback port sends `Sec-Fetch-*` (or is blocked by the Host check after a rebind). DNS rebinding is closed by the unconditional Host check.
- **How:** edit the plan's first Done-when bullet to: "A request with a foreign `Host`, or a browser request (one carrying `Sec-Fetch-*`) with a foreign `Origin` or a cross-site `Sec-Fetch-Site`, is refused in `mode = "none"`; a non-browser client's stray `Origin` header is admitted because the Host check already pins it to this listener." No code change. Add a sentence to the `enforce_host_origin` doc comment noting that the `Origin`-without-Fetch-Metadata admission is deliberate and covered by `a_non_browser_request_with_an_origin_header_passes`.
- **Test:** none new; existing `a_non_browser_request_with_an_origin_header_passes` and `a_browser_request_with_a_foreign_origin_is_refused` pin both sides.
- **Done when:** the done-when bullet no longer claims an unconditional Origin refusal.
- **Risk / notes:** if the owner instead picks (a), the change is: drop the `request_carries_fetch_metadata` gate, flip the test, and the captured-CLI replay tests under `mode = "none"` must be re-checked.

### 07-8 [FIX] [P1] Put `/health`, `/ready`, `/version` behind the host guard
- **Where:** `crates/llm-proxy-server/src/routes/mod.rs:74-90` `router` (`let lightweight = Router::new()...layer(ServiceBuilder::new().layer(from_fn_with_state(id_gen, inject_request_id)).layer(trace))`).
- **Now:** the lightweight routes skip `enforce_host_origin` (and the loop guard). Under `auth.mode = "none"` a DNS-rebinding page can read `/version` (name, version, target, git sha; `health.rs::version`) and `/health` (server name).
- **Wanted:** plan item 1: "Defend against DNS rebinding in `mode = "none"`." Leaking build info to a rebound page is the same hole, at lower impact.
- **How:** in `router`, add `.layer(from_fn_with_state(state.clone(), crate::host_guard::enforce_host_origin))` to the lightweight `ServiceBuilder` between `inject_request_id` and `trace`. Do NOT add `require_client_auth`: health probes must stay unauthenticated. The guard is a no-op under client-keys mode (it returns early), so load-balancer probes there are unaffected; under `mode = "none"` probes arrive on loopback with a loopback `Host`.
- **Test:** `crates/llm-proxy-server/tests/host_guard.rs`: `lightweight_routes_refuse_a_rebound_host_under_anonymous_auth` — `GET /version` with `host: rebind.attacker.example:3456` -> 403 and body `error.type == "forbidden_request_source"`; same for `/health` and `/ready`; `GET /version` with `host: 127.0.0.1:3456` -> 200; under client-keys auth (reuse the `the_guard_stands_down_under_client_keys_auth` auth fixture) `GET /health` with the rebound host -> 200. Fails before the fix (200 on the rebound host).
- **Done when:** the test passes and `rg -n 'enforce_host_origin' crates/llm-proxy-server/src/routes/mod.rs` shows two uses.
- **Risk / notes:** a Docker/k8s probe that sets `Host: <pod-ip>` against a `mode = "none"` server is already impossible (non-loopback bind is refused in none mode).

### 07-9 [FIX] [P1] `doctor` must honour `[provider.egress]` (reachability and probe)
- **Where:** `apps/llm-proxy/src/commands/doctor.rs:143` `reachability`, `reachability_checks` (line ~125), `probe_check` (line 228; builds `ProxyClient::try_with_timeouts` at ~275), `host_port` (line ~345); `ProviderConfig::egress: Option<ProviderEgressConfig{ proxy }>`; transport `ProxyClient::try_with_egress_proxy_and_timeouts` (`crates/llm-proxy-transport/src/client.rs:96`); runtime wiring `apps/llm-proxy/src/state.rs:585` `build_egress_clients`.
- **Now:** both checks connect directly. A provider reachable only through its egress proxy (socks5h, which also resolves DNS remotely) FAILS reachability although the server serves it, and the probe bypasses the proxy so it tests a path the server never takes. The final fix text even mentions `[provider.egress]` while the check never uses it.
- **Wanted:** plan item 9: "live reachability checks and one-token probes"; Done: "`doctor` reports a pass or a named fix for each configured provider" — the check must reflect what the server does.
- **How:**
  1. In `reachability`, if `provider.egress` is `Some`, check the proxy instead of the upstream: parse the proxy URL with an extended `host_port` that accepts schemes `socks5h`, `socks5` (default port 1080), `http` (80), `https` (443); resolve and TCP-connect to the PROXY address; check label `reachability <name> via egress proxy`; detail uses `host:port` only (never the URL: it may carry userinfo; `host_port` already strips userinfo). Fix text on failure: "check [provider.egress] proxy host/port and that the proxy is running". Skip the upstream DNS step in this case (socks5h resolves at the proxy).
  2. In `probe_check`, build the client with `ProxyClient::try_with_egress_proxy_and_timeouts(&egress.proxy, &config.server.upstream_timeouts)` when `provider.egress` is set (same code path as `build_egress_clients`; on build error return a FAIL row whose fix names `[provider.egress]`). Fall back to the existing client otherwise.
  3. Keep the generic final fix text.
- **Test:** `doctor.rs::tests`: `an_egress_provider_is_checked_through_its_proxy` — write a provider file with `endpoint = "http://unresolvable.invalid/v1/chat/completions"` and `[provider.egress] proxy = "http://127.0.0.1:<port of a live loopback TcpListener>"`; `run_doctor(Some(&path), false)`; the reachability row for `chat` is `Outcome::Pass` and its detail does not contain `unresolvable.invalid`. Second case: egress proxy URL with a dropped port -> `Outcome::Fail` with a fix containing `provider.egress`. For the probe: a loopback HTTP proxy test server is overkill; instead assert the unit-level helper by extracting `fn proxy_endpoint(provider) -> Option<(String,u16)>` and test it for all four schemes and a URL with userinfo (`socks5h://user:pw@h:1081` -> `("h",1081)`, no `pw` in any output). Fails before the change (the direct connect to `.invalid` FAILs).
  Note: the provider file helper `write_config` currently hard-codes a provider TOML; add an `egress: Option<&str>` parameter or a sibling helper.
- **Done when:** tests pass; `rg -n 'egress' apps/llm-proxy/src/commands/doctor.rs` shows use in `reachability` and `probe_check`.
- **Risk / notes:** `socks5h://` endpoints with `https` upstream through a proxy still verifies only TCP to the proxy; document that in the row detail ("proxy reachable; upstream not contacted"). `cmd_doctor` exit code unchanged.

### 07-10 [FIX] [P1] Give the "no one-token probe for protocol" WARN a named fix
- **Where:** `apps/llm-proxy/src/commands/doctor.rs:~268-278` `probe_check` (the `matches!(adapter.protocol.as_str(), "openai_chat_completions" | "anthropic_messages")` guard returning `fix: None`).
- **Now:** a provider whose chat/messages route is wired to another protocol (e.g. `openai_responses`, `gemini_generate_content`) gets `Outcome::Warn`, `fix: None`. Plan Done 9: "every non-PASS row names its fix."
- **Wanted:** every WARN/FAIL has `fix: Some(non-empty)`.
- **How:** set `fix: Some(format!("doctor can only probe openai_chat_completions and anthropic_messages routes; point [provider.routes] chat_completions or messages at an adapter speaking one of those, or rely on the reachability row for protocol {}", adapter.protocol))`. Consider also `Outcome::Warn` rather than Fail stays (unchanged).
- **Test:** `doctor.rs::tests`: `a_provider_with_an_unprobeable_protocol_still_names_a_fix` — provider file whose `chat_completions` route uses an adapter with `protocol = "openai_responses"` and `endpoint = <live spawn_upstream()>`; `run_doctor(path, true)`; find the row `probe one-token`; assert `Outcome::Warn` and `fix.as_deref().is_some_and(|f| f.contains("openai_chat_completions"))`. Also extend `every_row_is_a_pass_or_carries_a_named_fix` to loop over BOTH the 404 provider and this one so the invariant is checked across every branch. Fails before (fix is `None`).
- **Done when:** `rg -n 'fix: None' apps/llm-proxy/src/commands/doctor.rs` returns only PASS rows (the `Pass` constructors); the test passes.
- **Risk / notes:** the route-lookup test helper `write_config` writes two adapters with fixed protocols; add a protocol parameter.

### 07-11 [DECIDE] [P2] "Swap one provider at a time": narrow the claim, keep whole-registry swap
- **Where:** plan item 11 text ("Swap one provider at a time without touching other connections"); `apps/llm-proxy/src/reload.rs` module doc (lines 1-20, "swap them in, one provider at a time") and `reload_registry` (builds a whole new `ProviderRegistry` + `Engine`, swaps both at 152-153); `crates/llm-proxy-server/src/state.rs:436` shared `proxy_client`.
- **Now:** reload replaces the whole registry and engine. Unchanged providers keep their upstream connections only because every non-egress provider shares one `ProxyClient`; every `[provider.egress]` provider gets a freshly built client (new pool) on each reload (`build_egress_clients` is called for the full registry).
- **Options:** (a) implement real per-provider swap; (b) narrow the wording and, as a cheap improvement, reuse egress clients for unchanged providers.
- **Recommended default: (b).** The Done-when ("a reload that changes one provider leaves in-flight requests on other providers running") is met and tested; per-provider swap means partially mutable registry/admission state for no user-visible gain.
- **How:** (1) Reword reload.rs module doc and plan item 11 to: "Swap the provider and pool set atomically (a new engine over a new registry); in-flight requests finish on the engine they hold; unchanged providers keep their pooled upstream connections." (2) Optional, if cheap: in `reload_registry`, build the egress map by cloning the previous `Arc<ProxyClient>` for providers whose `egress.proxy` string and `upstream_timeouts` are unchanged (`state.egress_clients` is `pub(crate)`; expose a getter `AppState::egress_clients()` returning `&HashMap<String, Arc<ProxyClient>>`), and only building new ones for changed/added providers. Skip (2) if the getter would widen the public API unreasonably.
- **Test:** only if (2) is done: in `reload.rs::tests`, `an_unchanged_egress_provider_keeps_its_client`: reload twice without touching the egress provider's file and assert `Arc::ptr_eq` on the egress client obtained via the new getter. 
- **Done when:** the plan and module doc no longer say "one provider at a time".
- **Risk / notes:** egress clients capture timeouts at build; since `[server]` changes are restart-required (07-1 keeps that), reuse is safe.

### 07-12 [DOC] [P2] Config snapshot swap and engine swap are two steps: state the invariant
- **Where:** `apps/llm-proxy/src/reload.rs:152-153` (`state.config_source().swap(next_config)` then `state.swap_engine(engine)`); doc comment above ("The snapshot and engine slot then advance together").
- **Now:** a request landing between the two statements runs the old engine with the new snapshot (or the reverse order is equally non-atomic). Once 07-1 lands, the restart-required gate has already proven `next` equals `current` in every compared section, so the only differences are fields no part of the engine reads from the snapshot; the window is benign by construction, but the comment claims "together".
- **Wanted:** an accurate invariant, not a new lock.
- **How:** reword the comment: "The snapshot swap and the engine swap are two statements. This is safe because the restart-required gate above guarantees the snapshots are equal in everything the engine reads per stage (`[server]`, `[auth]`, `[storage]`, `[replica]`, `[mcp]`); a request observing the pair mid-swap sees an indistinguishable config." Keep the order (snapshot first, engine second). Do not add a lock.
- **Test:** none (comment). The invariant is tested transitively by 07-1.
- **Done when:** the comment no longer says "advance together".
- **Risk / notes:** if a future change lets reload alter any snapshot field the engine reads, this invariant must be revisited (swap both under one lock or give the engine its own `StaticConfigSource`).

### 07-13 [TEST] [P1] Make the reload done-when unit test distinguish the old and the new upstream
- **Where:** `apps/llm-proxy/src/reload.rs` tests: `spawn_echo_upstream` (~line 337), `a_provider_swap_leaves_in_flight_requests_on_other_providers_running` (~415).
- **Now:** every echo upstream returns the same body (`input_tokens: 3`), so `assert_eq!(response.core.usage.input_tokens, 3)` after reload cannot tell the old `quick` upstream from the new one. The test claims "the changed provider serves its new upstream afterwards."
- **Wanted:** plan Done-when 3 plus the test's own docstring: the changed provider serves its new upstream.
- **How:** give `spawn_echo_upstream` a `tokens: u32` parameter interpolated into `"usage":{"input_tokens":<tokens>,...}`. Use `slow = 3`, old `quick = 5`, new `quick = 7`. (Alternatively echo a distinguishing `"id"`; `input_tokens` is already read off `core.usage` by the assertions.) Update every other caller of `spawn_echo_upstream` (grep the file) to pass 3.
- **Test:** same test: before the reload assert `quick` yields `input_tokens == 5` (sanity: proves old upstream is distinguishable); after reload assert `== 7`; the in-flight `slow` request still yields `3`. Fails if the reload does not actually switch the upstream.
- **Done when:** the test asserts three distinct values.
- **Risk / notes:** the real-binary test `hot_reload_signal` already distinguishes by marker; this closes the in-process gap.

### 07-14 [FIX] [P1] Config errors must not echo userinfo or query strings from endpoints
- **Where:** `crates/llm-proxy-core/src/provider_config.rs`: `ConfigValidationError::CleartextCredentials` (variant at ~1935, `#[error(... endpoint "{endpoint}" ...)]`), constructor in `validate_cleartext_endpoint` (line 2608, `endpoint: endpoint.to_owned()`); same hazard in `ConfigValidationError::InvalidEndpointScheme` (variant ~1913, prints `{endpoint}`) and any other variant that prints `{endpoint}` (grep `endpoint \\"{endpoint}\\"`); `endpoint_without_query` (line 1472) strips the query but NOT userinfo, and is used in `crates/llm-proxy-core/src/self_loop.rs` (message) and `apps/llm-proxy/src/commands/doctor.rs`.
- **Now:** `http://user:pass@host/v1` or `https://host/v1?key=SECRET` (a Gemini-style API key in the query) is printed verbatim by `validate`/startup errors. A cleartext error is exactly when the URL likely carries credentials. The project already has `redacted_proxy_url` (line ~1271) for egress; endpoints have no equivalent.
- **Wanted:** AGENTS.md / plan item 2 spirit: credentials never appear in output ("Secrets never appear").
- **How:** add `pub fn endpoint_for_display(endpoint: &str) -> String` in provider_config.rs: strip `userinfo@` from the authority and drop `?query` and `#fragment` (reuse the `split_once("://")` / `rsplit_once('@')` logic of `redacted_proxy_url`; make `endpoint_without_query` call it or leave it and migrate callers). Use it where the endpoint is stored in the error: `endpoint: endpoint_for_display(endpoint)` in `validate_cleartext_endpoint` and the `InvalidEndpointScheme` construction sites; switch `self_loop.rs`'s `endpoint_without_query(endpoint.url)` and `doctor.rs` uses to it. Do not change `Debug` impls of provider structs (separate concern).
- **Test:** `provider_config.rs::tests`: `cleartext_error_does_not_echo_userinfo_or_query` — adapter endpoint `http://alice:s3cret@api.example.com/v1/chat?key=TOPSECRET`; `validate_provider_config` -> `CleartextCredentials`; the Display string contains `api.example.com` and NOT `s3cret`, `alice`, or `TOPSECRET`. Same for an invalid-scheme endpoint `ftp://bob:pw@h/x?k=v`. Add a `self_loop.rs` test with `http://u:p@127.0.0.1:8080/x` asserting the reported problem lacks `u:p`. Fails before.
- **Done when:** tests pass; `rg -n 'endpoint_without_query' crates apps` has no remaining caller that prints to users without also stripping userinfo.
- **Risk / notes:** `endpoint_without_query` is `pub`; keep it (it may be used by library consumers) or deprecate in favour of the new fn. Existing tests that assert the message contains the full endpoint (grep `message.contains("http`) must be updated to the redacted form.

### 07-15 [FIX] [P2] `serve --background` parent must honour `server.log_format`
- **Where:** `apps/llm-proxy/src/commands/serve.rs:54-57` `spawn_in_background` (`init_tracing(&LogFormat::from_env())`).
- **Now:** the parent process logs spawn failures in the env-only format; the daemon child honours config. Plan item 5: "Honour it, with `RUST_LOG_FORMAT` still winning."
- **Wanted:** both processes resolve the format the same way.
- **How:** in `spawn_in_background`, resolve the path (the caller `cmd_serve` already computed `path`; pass it instead of re-resolving), then `let format = llm_proxy_core::load_app_config(&path).map(|c| crate::state::startup_log_format(&c)).unwrap_or_else(|_| LogFormat::from_env());` and `init_tracing(&format)`. A config that fails to load must not abort the parent with a different error than the daemon will report; the child reports the real load error.
- **Test:** `apps/llm-proxy/tests/cli_serve.rs` module `log_format_startup`: `background_parent_failure_logs_use_the_configured_format` — run `llm-proxy serve --background --config <cfg>` where the provider dir or config makes the spawn fail deterministically... only if a deterministic spawn failure exists; otherwise cover by extracting `fn resolve_parent_log_format(path: &Path) -> LogFormat` and unit-testing it: config `log_format = "json"` -> `LogFormat::Json`; same with `RUST_LOG_FORMAT=plain` -> `Plain` (use the env guard in `crates/llm-proxy-core/src/test_support.rs` pattern or `LogFormat::effective` directly); missing file -> falls back to `from_env()`.
- **Done when:** `rg -n 'from_env' apps/llm-proxy/src/commands/serve.rs` shows it only as the fallback.
- **Risk / notes:** loading config in the parent duplicates a parse; harmless.

### 07-16 [TEST] [P2] Assert `no-store` on admin 401 and unknown-path responses for both admin routers
- **Where:** `crates/llm-proxy-admin/tests/admin_surface.rs` (`responses_are_never_cacheable` only checks authed `GET /keys`); `crates/llm-proxy-api/tests/usage_api.rs` (`every_admin_response_forbids_caching` covers 200 and 401 on the three real routes but not unknown paths).
- **Now:** the layers wrap `require_admin` and (via `Router::layer`) the fallback, so 401 and 404 should carry `Cache-Control: no-store` and `Pragma: no-cache`, but no test pins the admin crate's 401, nor either router's unknown-path response.
- **Wanted:** plan item 3/Done 3: "all admin API responses" (incl. errors).
- **How/Test:**
  - `admin_surface.rs`: `error_responses_are_never_cacheable` — for `(token, path)` in `[(None,"/keys"), (Some("wrong"),"/keys"), (None,"/nope"), (Some(ADMIN_TOKEN),"/nope")]` send GET; assert `cache-control == "no-store"` and `pragma == "no-cache"` on each; record statuses (expected 401, 401, 401, 404; if axum answers an unauthenticated unknown path differently, assert the header regardless and the observed stable status). Fails if a fallback ever escapes the layers.
  - `usage_api.rs`: same four cases against `app_for(&ledger.path)`; assert headers (and 404 for the authed unknown path).
- **Done when:** both tests pass.
- **Risk / notes:** none.

### 07-17 [DOC] [P2] OTel export is the scalar counters only; the "live collector" is an in-process sink
- **Where:** `docs/completed/07-hardening-and-operations.md` Done 7 ("opt-in `[server.metrics.otlp]` pushes the same counters as OTLP/HTTP JSON (tested against a live local collector)"); `apps/llm-proxy/src/otel_metrics.rs` module doc and `start_otlp_metrics` (8 observable counters; Prometheus additionally exposes the latency summary and `llm_proxy_model_requests_total{provider,model}`).
- **Now:** "the same counters" is true for the 8 scalar counters; per-model counts and latency quantiles are Prometheus-only. The test `counters_arrive_at_the_collector_as_otlp_json` uses an in-process axum sink, not an OpenTelemetry Collector.
- **Wanted:** the Done text matches reality. Decision (recommended): do NOT add per-model/latency OTel instruments in this plan; the plan item says "Export the existing counters."
- **How:** reword Done 7 to: "opt-in `[server.metrics.otlp]` pushes the eight scalar counters as OTLP/HTTP JSON (per-model counts and latency quantiles are available on the Prometheus endpoint only); verified against an in-process OTLP/HTTP sink in `otel_metrics.rs`." Add the same scope sentence to the `otel_metrics.rs` module doc.
- **Test:** none (doc); value assertion is 07-18.
- **Done when:** the plan no longer says "live local collector".
- **Risk / notes:** if the owner wants parity, add `llm_proxy_model_requests_total` as an observable counter with `provider`/`model` attributes by iterating `snapshot.model_counts`; out of scope here.

### 07-18 [TEST] [P2] OTLP test must assert a counter value, not just a name
- **Where:** `apps/llm-proxy/src/otel_metrics.rs` tests, `counters_arrive_at_the_collector_as_otlp_json` (~line 166-220).
- **Now:** the loop only finds the bytes `resource` and `llm_proxy` and then asserts the body `contains("llm_proxy_requests_total")`; a payload with zero values would pass. It also slices `&body[..body.len().min(600)]` (string slicing in a test, tolerated).
- **Wanted:** observable behaviour: the pushed counter carries the live value.
- **How:** after receiving a body, `serde_json::from_slice::<Value>` it; walk `resourceMetrics[].scopeMetrics[].metrics[]` for `name == "llm_proxy_requests_total"`; read `sum.dataPoints[0].asInt` (OTLP/JSON encodes int64 as a string, accept string or number). With two `record_request` calls (one streamed) the value must be `2`; also assert `llm_proxy_requests_streamed_total == 1` and `llm_proxy_requests_failed_total == 1`. Keep the 10 s deadline loop; retry until a payload with value 2 arrives (the first export may predate the records).
- **Done when:** the test fails if the callback returns 0 for `RequestsReceived`.
- **Risk / notes:** field names follow OTLP JSON (camelCase); if the SDK emits `asDouble` for monotonic sums, accept it.

### 07-19 [DOC] [P2] Provenance docs promise an announcement the code does not make
- **Where:** `apps/llm-proxy/src/provenance.rs` module doc lines 1-9 ("the CLI layer (`--port`) is applied by the serve command before this report could see it and is announced in the header instead"); `apps/llm-proxy/src/commands/validate.rs:41-57` (prints only `PRECEDENCE`). Also the report covers operator settings only, not provider settings.
- **Now:** `validate` has no `--port` flag and the header does not mention the CLI layer; the doc over-promises.
- **Wanted:** doc matches code.
- **How:** reword the module doc to: "the CLI layer (`--port` on `serve`) is applied by the serve command and does not appear in this report; `validate` has no CLI overrides." State that provider files are out of scope (they carry no layering). No code change.
- **Test:** none.
- **Done when:** `rg -n 'announced in the header' apps/llm-proxy/src/provenance.rs` returns nothing.
- **Risk / notes:** none.

### 07-20 [DOC] [P2] Tighten the claim that the stress test "fails if regressed to two adjacent swaps"
- **Where:** `crates/llm-proxy-server/src/auth_pair.rs` test doc above `swaps_under_fire_never_serve_a_mixed_pair` (~line 224) and plan Done 12 / commit message wording.
- **Now:** `SwappingAuthPair` exposes only single-guard methods (`replace`, `replace_admission`, `replace_client_auth`), so the regression the test claims to catch (two adjacent half-swaps) cannot be written against this type; the test is a sampling check that reads never see a torn pair, which is valuable but not mutation-proven.
- **Wanted:** the claim is accurate.
- **How:** reword the test comment: "Guards against a torn read: the type offers no half-swap, so this checks every observed snapshot is one installed generation; a regression to separate slots would be a type-level change this test would catch only by sampling." No code change.
- **Test:** none.
- **Done when:** the comment no longer asserts it "fails against that regression" unconditionally.
- **Risk / notes:** none.

### 07-21 [TEST] [P2] Cover `cmd_reload` (PID file -> SIGHUP)
- **Where:** `apps/llm-proxy/src/commands/reload.rs:11` `cmd_reload`; new test file `apps/llm-proxy/tests/cli_reload.rs` modelled on `apps/llm-proxy/tests/cli_stop.rs` (`tempdir_paths`, `PidManager::new(dir)`).
- **Now:** SIGHUP delivery to a real server is covered end-to-end, but `cmd_reload` itself (no PID file, stale PID, PID-file ownership re-read, signal) has no test (`rg cmd_reload apps/llm-proxy/tests` -> nothing).
- **Wanted:** plan item 11: "`llm-proxy reload` trigger."
- **How/Test (unix-only, `#[cfg(unix)]`):**
  - `reload_without_a_pid_file_errors`: `cmd_reload(&paths)` is `Err` with message containing "no PID file".
  - `reload_with_a_stale_pid_errors`: write `299999999` to `paths.pid_manager().pid_file()`; error contains "stale PID file".
  - `reload_signals_the_recorded_process`: spawn `Command::new("sleep").arg("30")`, write its pid to the PID file, call `cmd_reload`, `child.wait()`, assert `std::os::unix::process::ExitStatusExt::signal(&status) == Some(libc::SIGHUP)` (default SIGHUP action terminates `sleep`). Use `libc` only if already a dev-dependency of the `llm-proxy-app` crate; otherwise compare to the literal `1`.
- **Done when:** the three tests pass.
- **Risk / notes:** make sure the test never signals a pid it did not spawn.

### 07-22 [FIX] [P1] Model discovery ignores `[provider.egress]` (found while verifying 07-4/07-9)
- **Where:** `crates/llm-proxy-transport/src/discovery.rs` `DiscoveryClient` (`try_new` builds one `reqwest::Client` with no proxy; `discover`, `fetch_page`, `build_request`); caller `crates/llm-proxy-engine/src/catalog.rs` (`ModelCatalogService` holds one `DiscoveryClient::default()`), `apps/llm-proxy/src/commands/models.rs:122`.
- **Now:** `rg egress crates/llm-proxy-transport/src/discovery.rs` finds only `egress: None` in tests. For a provider with `[provider.egress]`, chat traffic goes through the proxy but model discovery (which carries the provider's API key) leaves the host directly, defeating the operator's egress policy and failing where only the proxy has network access.
- **Wanted:** egress proxy applies to all provider upstream traffic (the same intent as `build_egress_clients`).
- **How:** in `discover`, if `provider.egress` is `Some(e)`, use a client built with `reqwest::Proxy::all(&e.proxy)` plus the same timeouts and `same_origin_redirect_policy()` (factor a private `fn build_client(proxy: Option<&str>) -> Result<Client, ProviderError>`; reuse `validate_egress_proxy_url` from `client.rs` by making it `pub(crate)`). Build per `discover` call (discovery is infrequent) or memoise per proxy URL in a `Mutex<HashMap>` if profiling demands; do not hold the guard across `.await`. Providers without egress keep `self.http`.
- **Test:** `discovery.rs::tests`: `discovery_goes_through_the_provider_egress_proxy` — mirror `client.rs::egress_proxy_client_sends_requests_through_the_proxy` (line ~1910): start an HTTP forward-proxy stub (axum fallback that records the absolute request URI and answers `{"data":[{"id":"m"}],"has_more":false}`); a provider whose discovery endpoint is `http://upstream.invalid/models` with `egress: Some(ProviderEgressConfig{proxy: <stub base>})`; `discover` succeeds and the stub saw a request for `upstream.invalid`. Fails before (DNS failure for `.invalid`).
- **Done when:** the test passes.
- **Risk / notes:** also tag for the owner of plan 04/05 routing docs (`(also: plan 04)`). `fetch_json` (pricing catalogue) is not provider traffic and stays direct.

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: RV-6, RV-7, RV-8, RV-9, RV-14, RV-15, RV-16; `traits` and every other engine double path are 05-13.
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings
- "`config_validation.rs` named in plan item 2 but the check lives in `provider_config.rs`" (G15) — the plan text contains no such filename (`rg config_validation docs/completed/07-hardening-and-operations.md` is empty); the report's premise is wrong.
- "`log_format` serve wiring is untested" (row 15) — `apps/llm-proxy/tests/cli_serve.rs` module `log_format_startup` runs the real binary with `server.log_format = "json"` and checks the first stdout line is JSON, and that `RUST_LOG_FORMAT=plain` wins. Only the `--background` parent is a real gap (07-15).
- "SIGHUP trigger is untested" (row 33, SIGHUP half) — `cli_serve.rs::hot_reload_signal::sighup_swaps_a_changed_providers_upstream` sends a real `kill -HUP` to the binary and asserts the new upstream marker. The `cmd_reload` half is genuinely uncovered (07-21).
- (Reversed) The `expect()` calls in `ProxyClient::new` and in `discovery.rs:152` / `:353` were rejected here as allowed by `#[expect]`. They are not allowed: `AGENTS.md` bans `expect()` in non-test code, and `#[expect]` only governs how a lint is silenced. They are now RV-6, RV-7 and RV-8 in [rule-violations.md](rule-violations.md).
- (Reversed) The engine's `pub mod traits` plus root `pub use` was rejected here as a crate-wide pattern for a separate refactor. That refactor is now 05-13, which makes the modules private and keeps the root re-exports, so do not fix `traits` on its own. Tests that import `llm_proxy_engine::traits::` (e.g. `crates/llm-proxy-engine/tests/open_provider_set.rs:23`) move to root paths in 05-13.
- Row 17 ("verified against the real binary"), row 36 (mutation sensitivity of the pair stress test; reworded in 07-20), row 37 Postgres arm — NOT-STATICALLY-VERIFIABLE; need a run, no source gap.
- Row 4/G18 as "lightweight routes are a hole in client-keys mode" — under client-keys the guard stands down by design; only the `mode = "none"` exposure is real (07-8).
