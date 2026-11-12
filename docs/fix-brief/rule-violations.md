# Rule violations: fix spec

`AGENTS.md` violations in non-test code, found across all plan areas. Each one is listed
here once, with one instruction. The plan files point here instead of repeating them.
All paths are relative to the repository root. Line numbers were checked against master
@ `7b1a54a`.

`#[expect(clippy::expect_used, reason = …)]` does not make an `expect()` acceptable.
`AGENTS.md` bans `expect()` in non-test code outright; the `#[expect]` rule only governs
how a lint is silenced. Every item below removes the call, and with it the `#[expect]`
attribute that covered it. An unfulfilled `#[expect]` fails clippy under `-D warnings`,
so delete each attribute in the same edit.

Shared rules for this file:

- Run the three gates after each item. Run `cargo shear` after RV-3, RV-9, RV-11 and
  RV-12: `regex` may become unused in `llm-proxy-adapters`, `llm-proxy-core` or
  `llm-proxy-engine`.
- These are behaviour-preserving refactors. Existing tests must pass unchanged. Where an
  item names a test, add it before the refactor and confirm it passes on the old code (it
  pins behaviour; red-green does not apply).
- Closing check after the last RV item: for every file listed below,
  `rg -n '\.expect\(|\.unwrap\(\)|panic!' <file>` shows hits only after that file's first
  `#[cfg(test)]`.

## Work items

### RV-1 [FIX] [P2] Slice pattern instead of `content[0]` in the Anthropic client decoder
- **Where:** `crates/llm-proxy-protocol/src/client/anthropic.rs:435-437`
  (`if content.len() == 1 { if let CoreContent::Text { ref text, .. } = content[0]`).
- **How:** `if let [CoreContent::Text { text, .. }] = content.as_slice() { … }`.
- **Done when:** no `content[0]` in the file's non-test code.

### RV-2 [FIX] [P2] `json_object` builds a `serde_json::Map` directly
- **Where:** `crates/llm-proxy-adapters/src/anthropic.rs:31` `json_object`
  (`.expect("json! macro with {{...}} always produces a JSON object")`).
- **How:** callers build `let mut block = serde_json::Map::new(); block.insert(..);` and
  wrap with `serde_json::Value::Object(block)`. Delete `json_object` if nothing else uses it.
- **Done when:** the `expect` and its `#[expect]` are gone.

### RV-3 [FIX] [P2] Replace the tool-use-id sanitiser regex
- **Where:** `crates/llm-proxy-adapters/src/anthropic.rs:40` `INVALID_TOOL_USE_ID_CHAR`
  (`Regex::new(r"[^A-Za-z0-9_]").expect("valid regex")`).
- **How:** at the use site, `id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>()`.
  Delete the static.
- **Test:** `sanitised_tool_use_id_keeps_ascii_word_chars_and_replaces_the_rest`, with
  inputs `"call-1.x"`, `"é"` and `"ok_1"` and their expected outputs.

### RV-4 [FIX] [P2] Drop the `write!(..).expect(..)` in the id escaper
- **Where:** `crates/llm-proxy-adapters/src/anthropic.rs:60`.
- **How:** `result.push_str(&format!("_0x{byte:02x}_"));`, then remove the `#[expect]` and
  `use std::fmt::Write` if unused.

### RV-5 [FIX] [P2] No `Value` indexing for `include_usage`
- **Where:** `crates/llm-proxy-adapters/src/openai_chat.rs:497`
  (`include_usage["include_usage"].clone()`).
- **How:** build the `stream_options` object from `serde_json::Value::Bool(true)`.
  `Value` indexing cannot panic, but it is the pattern the rule names.

### RV-6 [FIX] [P2] Delete `ProxyClient::new()` and `impl Default for ProxyClient`
- **Where:** `crates/llm-proxy-transport/src/client.rs:53-67`. Non-test callers (5):
  `crates/llm-proxy-engine/src/lib.rs:367` (`EngineBuilder::new`),
  `crates/llm-proxy-sdk/src/lib.rs:180`, `crates/llm-proxy-bridge/src/lib.rs:472`,
  `apps/llm-proxy/src/commands/serve.rs:391`, `apps/llm-proxy/src/commands/doctor.rs:285`.
  All other callers are test code.
- **Decision:** delete both. Plans 01, 02, 03, 07 and R-20 gave conflicting instructions.
  This item replaces all of them.
- **How:** do it together with 04-7, which moves the engine, SDK and bridge to
  `ProxyClient::try_with_timeouts(&config.server.upstream_timeouts)`. In the engine,
  `EngineBuilder` holds `Option<Arc<ProxyClient>>` and builds the client in `build()`,
  mapping the error to `EngineError::Internal`. `serve.rs` and `doctor.rs` use the `try_*`
  constructor with `anyhow` context. Test callers become `ProxyClient::try_new().expect("client")`.
  `expect` is allowed in tests, and this is a mechanical replacement.
- **Done when:** `rg -n 'ProxyClient::new\(\)|impl Default for ProxyClient' crates apps`
  returns nothing.

### RV-7 [FIX] [P2] Delete `impl Default for DiscoveryClient`
- **Where:** `crates/llm-proxy-transport/src/discovery.rs:353`. Non-test users:
  `crates/llm-proxy-engine/src/catalog.rs:91` (`ModelCatalogService::new`) and
  `apps/llm-proxy/src/commands/models.rs:122` (`get_or_insert_with(DiscoveryClient::default)`).
- **How:** `ModelCatalogService::new` takes the `DiscoveryClient` as a parameter. Its caller
  already returns a `Result`, so it builds the client with `DiscoveryClient::try_new()?`.
  `models.rs` uses `DiscoveryClient::try_new()?`. 07-22 also edits `discovery.rs` (a
  per-call egress client inside `discover`), so do the two one after the other.
  Tests use `DiscoveryClient::try_new().expect("client")`.
- **Done when:** `rg -n 'DiscoveryClient::default|impl Default for DiscoveryClient' crates apps` is empty.

### RV-8 [FIX] [P2] Restructure the discovery retry loop
- **Where:** `crates/llm-proxy-transport/src/discovery.rs:152` `fetch_page`
  (`Err(last_error.expect("retry loop ran at least once"))`).
- **How:** `let mut attempt = 0; loop { match self.send_once(..).await { Ok(page) => return Ok(page), Err(e) if e.is_transient() && attempt < MAX_TRANSIENT_RETRIES => { warn; attempt += 1; sleep } Err(e) => return Err(e) } }`.
- **Test:** the existing transient-retry tests keep passing. If none asserts the attempt
  count, add `transient_failures_retry_exactly_max_times` using the loopback fake with a
  counter.

### RV-9 [FIX] [P2] One provider-name validator, no regex
- **Where:** `crates/llm-proxy-core/src/provider_config.rs:2219-2226` (`PROVIDER_NAME_SLUG`,
  `{1,64}`), `crates/llm-proxy-engine/src/lib.rs:84-90` (`PROVIDER_NAME_REGEX`, used by
  `validate_provider_name`), `crates/llm-proxy-engine/src/catalog.rs` `is_safe_provider_slug`.
- **How:** in `llm-proxy-core`, next to the constant from R-21, add
  `pub fn is_valid_provider_name(name: &str) -> bool { (1..=MAX_PROVIDER_NAME_LEN).contains(&name.len()) && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')) }`.
  Use it in all three places and delete both regex statics. Do this together with R-21.
- **Test:** the table test from R-17 (provider names), which covers empty, 64, 65, an
  uppercase letter, `.` and a non-ASCII letter, run through both the config loader and the
  engine path validator.

### RV-10 [FIX] [P2] Infallible empty registry for `EngineBuilder::new`
- **Where:** `crates/llm-proxy-engine/src/lib.rs:355-362`
  (`.expect("an empty provider registry is always valid")`).
- **How:** add `ProviderRegistry::empty() -> Self` (two empty maps, infallible) in
  `crates/llm-proxy-core/src/provider_registry.rs`. Use it, and drop the `# Panics` doc section.

### RV-11 [FIX] [P2] URL redaction without a regex
- **Where:** `crates/llm-proxy-engine/src/error.rs:296-302` `URL_REDACT_REGEX`.
- **How:** a scanner that finds each `http://` or `https://` and redacts through the next
  whitespace. Iterate with `find` / `split_once`. No byte-index slicing.
- **Test:** keep the existing redaction tests. Add `two_urls_in_one_message_are_both_redacted`
  and `a_url_at_the_very_end_is_redacted`.

### RV-12 [FIX] [P2] Duration parsing without a regex
- **Where:** `crates/llm-proxy-engine/src/classification.rs:157-159` `DURATION_PAIR`.
- **How:** owned by 04-4, which rewrites rate-limit reset parsing. Replace the regex with a
  hand-written scanner over `(number)(ns|us|µs|ms|s|m|h)` pairs. The unit `µs` is
  multi-byte, so match on `str` prefixes, not bytes.
- **Done when:** 04-4 is done and the static is gone.

### RV-13 [FIX] [P2] No slicing in the missing-adapter error
- **Where:** `crates/llm-proxy-core/src/provider_registry.rs:466-470`
  (`keys[..MAX_ADAPTERS_IN_ERROR].join(", ")`, `keys.len() - MAX_ADAPTERS_IN_ERROR`).
- **How:** sort `keys` (it comes from a `HashMap`, so its order is random), then
  `keys.iter().take(MAX_ADAPTERS_IN_ERROR).copied().collect::<Vec<_>>().join(", ")` and
  `keys.len().saturating_sub(MAX_ADAPTERS_IN_ERROR)`.
- **Test:** `adapter_error_lists_at_most_n_adapters_sorted`: a provider with
  `MAX_ADAPTERS_IN_ERROR + 3` adapters and a route that names a missing one. The listed
  names are the first N in sorted order, and the message ends with `(+3 more)`.

### RV-14 [FIX] [P2] No string slicing in `scheme_region`
- **Where:** `crates/llm-proxy-core/src/provider_config.rs:2631` (`Some(idx) => &endpoint[..idx]`).
- **How:** `endpoint.split_once("://").map_or("", |(scheme, _)| scheme)`.

### RV-15 [FIX] [P2] No indexing in `percentile`
- **Where:** `crates/llm-proxy-core/src/metrics.rs:383-401` (`sorted[idx]`, under an `#[expect]`).
- **How:** `sorted.get(idx).or_else(|| sorted.last()).copied().unwrap_or(Duration::ZERO)`.
  Keep the early return for empty input.

### RV-16 [FIX] [P2] `self_loop` reachable through one path
- **Where:** `crates/llm-proxy-core/src/lib.rs:20` (`pub mod self_loop;`) and the root
  re-export at `:52`.
- **How:** make the module private (`mod self_loop;`) and keep the root re-export. Nothing
  outside the file uses `llm_proxy_core::self_loop::`. This matches 05-13, which picks root
  re-exports as the one path.

### RV-17 [FIX] [P2] No indexing in the RESP test server
- **Where:** `crates/llm-proxy-store-redis/src/test_server.rs:170` (`byte[0]`). The module
  is compiled under the `test-server` feature, not `cfg(test)`, so the rule applies.
- **How:** `let [value] = byte;` and compare `value`. Code added to this file by 09-1, 09-2
  and 09-3 follows the same rule.

## Owned elsewhere
- `crates/llm-proxy-protocol/src/client/responses.rs:985` `let _ = index;`: 08-13.
- Double public paths in the engine crate root: 05-13. It also closes 04-15 and 06-13.

## Not violations
- `crates/llm-proxy-engine/src/cache.rs:189,226,449,462` `let _ = …send_replace(..)`: it
  returns the previous value, not a `Result` or `Future`.
- `apps/llm-proxy/src/replica.rs:41,55` `let _ = (url, registry, state, pair);` binds plain
  values, not a `Result` or `Future`. Leave it. `drop(..)` of references would trip clippy's
  `drop_ref`.
- `crates/llm-proxy-engine/src/traits.rs:234-472` `let _ = param;` in default trait methods:
  unused parameters, not results.
- `crates/llm-proxy-server/src/routes/core_pipeline.rs:398`
  `#[expect(clippy::large_enum_variant, reason = …)]` uses the allowed form for a lint that
  is not an `AGENTS.md` ban.
