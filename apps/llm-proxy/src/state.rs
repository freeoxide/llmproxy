//! Application state construction and tracing initialization.
//!
//! Provides helpers to load TOML config into `AppState`, build `BuildInfo`,
//! check catalog enforcement, and initialize the tracing subscriber.

use std::path::Path;

use anyhow::{Context, Result};
use llm_proxy_core::{
    AppConfig, LogFormat, ProviderConfig, ProviderRegistry, merge_catalog, parse_catalog_file,
};
use llm_proxy_provider::{ProviderAdapterRegistry, ProxyClient};
use llm_proxy_server::{AppState, BuildInfo};
use tracing::info;

use crate::paths::providers_dir_for;

/// Load TOML config files and build [`AppState`] in TOML new-runtime mode.
///
/// Loads the main `AppConfig` from `path`, then scans a `providers/` directory
/// next to `path` for provider TOML files. Uses [`ProviderRegistry::load_from_dir`]
/// which handles non-regular-file filtering, deterministic sorted ordering, and
/// duplicate-name detection. Protocol validation is performed separately against
/// the builtin adapter set after loading.
///
/// Takes ownership of `adapter_registry` and `proxy_client` since they are
/// consumed by [`AppState::new`] and not reused after this call.
pub(crate) fn load_toml_state(
    path: &Path,
    adapter_registry: ProviderAdapterRegistry,
    proxy_client: ProxyClient,
    port_override: Option<u16>,
) -> Result<AppState> {
    let mut app_config: AppConfig = llm_proxy_core::load_app_config(path)
        .with_context(|| format!("loading TOML config from {}", path.display()))?;

    // Apply CLI port override.
    // In-place mutation is safe here: the config is consumed by AppState::new
    // below and is not shared with any other caller.
    if let Some(p) = port_override {
        let mut bind = app_config.server.bind;
        bind.set_port(p);
        app_config.server.bind = bind;
    }

    // Load provider files from providers/ directory next to the main config.
    let providers_dir = providers_dir_for(path);
    let registry = if providers_dir.exists() {
        ProviderRegistry::load_from_dir(&providers_dir)
            .with_context(|| format!("loading providers from {}", providers_dir.display()))?
    } else {
        ProviderRegistry::from_providers(vec![])
            .with_context(|| "building empty provider registry")?
    };

    // Validate all provider protocols against the builtin adapter set.
    let known_protocols = adapter_registry.protocol_names();
    registry
        .validate_protocols(known_protocols)
        .with_context(|| "validating provider protocols")?;

    let cache_dir = providers_dir.join(".catalog");
    for provider in registry.iter() {
        if let Some(warning) = catalog_enforcement_warning(provider, &cache_dir) {
            tracing::warn!(provider = %provider.name, "{warning}");
        }
    }

    // Build the client-auth state here (before the listener binds) so an
    // invalid verifier hash is a startup error, not a runtime fail-closed.
    let client_auth = llm_proxy_server::ClientAuth::from_config(&app_config.auth)
        .with_context(|| "building client auth from [auth] config")?;

    // Build per-provider egress clients from [provider.egress] entries here
    // (also before the listener binds) so a proxy URL the transport cannot
    // use is a startup error, never a silent fallback to direct egress.
    // Providers without an egress section get no entry and keep using the
    // shared default client, exactly as before the section existed.
    let mut egress_clients = std::collections::HashMap::new();
    for provider in registry.iter() {
        if let Some(egress) = &provider.egress {
            let client = ProxyClient::try_with_egress_proxy(&egress.proxy).with_context(|| {
                format!(
                    "building egress proxy client for provider \"{}\" from [provider.egress]",
                    provider.name
                )
            })?;
            // Log the provider and scheme, not the URL: egress proxy URLs may
            // carry userinfo credentials.
            info!(
                provider = %provider.name,
                scheme = egress.proxy.split("://").next().unwrap_or_default(),
                "routing upstream traffic through the configured egress proxy"
            );
            egress_clients.insert(provider.name.clone(), std::sync::Arc::new(client));
        }
    }

    // Build the persistent event bus from `[storage]` (also before the
    // listener binds) so an unusable ledger path is a startup error rather
    // than a silent no-op. Absent `[storage]` keeps the NoopBus default:
    // nothing is persisted, exactly as before the section existed.
    let event_bus: std::sync::Arc<dyn llm_proxy_storage::EventBus> = match &app_config.storage {
        Some(storage) => {
            let bus = llm_proxy_storage::SqliteEventBus::open(
                &storage.path,
                llm_proxy_storage::RetentionPolicy {
                    max_age: storage.max_age,
                    max_rows: storage.max_rows,
                    prune_every: storage.prune_interval,
                },
            )
            .with_context(|| format!("opening usage ledger at {}", storage.path.display()))?;
            info!(
                path = %storage.path.display(),
                "persisting usage events to the SQLite ledger"
            );
            std::sync::Arc::new(bus)
        }
        None => std::sync::Arc::new(llm_proxy_storage::NoopBus),
    };

    info!(
        config = %path.display(),
        providers = registry.len(),
        "loaded TOML config"
    );

    // Announce the admin-mount security posture loudly at startup so the
    // operator sees which surface their config selected. The
    // main-listener opt-in is a different, deliberately less-isolated
    // posture than the default loopback-only dedicated listener.
    warn_admin_posture(&app_config);

    Ok(AppState::new_with_catalog_dir(
        app_config,
        registry,
        adapter_registry,
        proxy_client,
        build_info(),
        Some(cache_dir),
    )
    .with_client_auth(std::sync::Arc::new(client_auth))
    .with_egress_clients(egress_clients)
    .with_event_bus(event_bus))
}

/// Emit the startup warnings naming the admin `/usage` mount posture.
///
/// `admin_on_main_listener = true` deliberately exposes the admin surface on
/// the main listener, so it is called out by name. A dedicated admin
/// listener on a non-loopback address is likewise called out (the default is
/// loopback-only); the operator may have pointed it at a tunnel interface,
/// but the warning makes that choice visible in every startup log.
pub fn warn_admin_posture(config: &AppConfig) {
    use llm_proxy_core::AdminMountPosture;
    match config.admin_posture() {
        Some(AdminMountPosture::MainListener) => {
            tracing::warn!(
                posture = AdminMountPosture::MainListener.name(),
                main_bind = %config.server.bind,
                "ADMIN SECURITY POSTURE: /usage is also served on the main listener, \
                 protected only by the admin token — every network that reaches the \
                 main listener reaches the admin API. Set server.admin_on_main_listener \
                 = false to return to the loopback-only dedicated admin listener."
            );
        }
        Some(AdminMountPosture::DedicatedListener) => {
            if let Some(bind) = config.server.effective_admin_bind()
                && !bind.ip().is_loopback()
            {
                tracing::warn!(
                    posture = AdminMountPosture::DedicatedListener.name(),
                    admin_bind = %bind,
                    "ADMIN SECURITY POSTURE: the dedicated admin listener binds a \
                     non-loopback address; any peer that reaches it can attempt the \
                     admin token. Point server.admin_bind back at loopback unless a \
                     tunnel requires otherwise."
                );
            }
        }
        None => {}
    }
}

/// Check if a provider with catalog enforcement enabled has no usable models.
///
/// Returns a warning string if the provider enforces its catalog but has no
/// static models and no cached live discovery results.
pub fn catalog_enforcement_warning(provider: &ProviderConfig, cache_dir: &Path) -> Option<String> {
    let catalog = provider.catalog.as_ref()?;
    if !catalog.enforce || !merge_catalog(catalog, &[]).is_empty() {
        return None;
    }

    let cache_path = cache_dir.join(format!("{}.toml", provider.name));
    let cached_models = std::fs::symlink_metadata(&cache_path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .and_then(|_| std::fs::read_to_string(&cache_path).ok())
        .and_then(|raw| parse_catalog_file(&raw).ok())
        .filter(|cache| cache.catalog.provider == provider.name)
        .map(|cache| cache.catalog.models)
        .unwrap_or_default();

    if !merge_catalog(catalog, &cached_models).is_empty() {
        return None;
    }

    Some(format!(
        "provider \"{}\" has catalog enforcement enabled but no allowed models; requests will be rejected until static models are configured or live discovery writes a cache",
        provider.name
    ))
}

/// Build version/target metadata embedded at compile time.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        target: env!("VERGEN_CARGO_TARGET_TRIPLE"),
        git_sha: env!("VERGEN_GIT_SHA"),
    }
}

/// Initialize the tracing subscriber.
///
/// Uses `RUST_LOG` env var with fallback to "info". The output format is
/// selected by `log_format`: [`LogFormat::Plain`] (default) for human-readable
/// logs, [`LogFormat::Json`] for structured JSON (one object per line). The
/// format is read from the `RUST_LOG_FORMAT` env var at init via
/// [`LogFormat::from_env`], before the TOML config is loaded (tracing is needed
/// during config loading itself). To control log verbosity, set `RUST_LOG`.
pub fn init_tracing(log_format: &LogFormat) {
    // Delegates to [`build_log_layer`] so the format-selection logic
    // (`.json()` vs plain) is exercised by `build_log_layer`'s own tests and
    // is not duplicated here. The public signature is unchanged.
    //
    // The filter is attached *to the boxed layer* (via `Layer::with_filter`)
    // rather than to the registry first. `build_log_layer` returns a type-erased
    // `Box<dyn Layer<Registry>>`; chaining `.with(filter).with(boxed_layer)`
    // would require the erased layer to implement `Layer<Layered<EnvFilter,
    // Registry>>`, which the `Box<dyn Layer<S>>` blanket impl does not provide
    // (its `S` is fixed to `Registry`). Filtering the layer instead yields a
    // concrete `Filtered<..>` type that implements `Layer<Registry>` and
    // composes cleanly with `registry().with(...)`.
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = build_env_filter();
    let layer = build_log_layer(log_format, std::io::stdout).with_filter(filter);
    tracing_subscriber::registry().with(layer).init();
}

/// Build the [`EnvFilter`] for tracing.
///
/// Falls back to `"info"` when `RUST_LOG` is unset. `"info"` is a known-valid
/// filter string, so `parse_lossy` is safe here (it never panics).
fn build_env_filter() -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::builder().parse_lossy("info"))
}

/// Build the tracing-subscriber fmt layer for `log_format`, writing to the sink
/// produced by `make_writer`.
///
/// Extracted from [`init_tracing`] so the format selection
/// (plain vs `.json()`) is independently testable: a test passes a buffer-backed
/// [`MakeWriter`] here, installs the composed subscriber as the thread-local
/// default, emits a [`tracing::event!`], and asserts on the captured bytes —
/// without needing to call the global, process-wide `init()` (which can only
/// run once and would race with other tests).
///
/// Returns a boxed, type-erased layer so callers (and tests) do not have to
/// name the concrete `Layer` generic over the writer type.
fn build_log_layer<W>(
    log_format: &LogFormat,
    make_writer: W,
) -> Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    use tracing_subscriber::{fmt, layer::Layer};
    match log_format {
        LogFormat::Plain => Box::new(
            fmt::layer()
                .with_writer(make_writer)
                .with_ansi(false)
                .boxed(),
        ),
        LogFormat::Json => Box::new(fmt::layer().json().with_writer(make_writer).boxed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_config(raw: &str) -> ProviderConfig {
        toml::from_str::<llm_proxy_core::ProviderFile>(raw)
            .unwrap()
            .provider
    }

    #[test]
    fn enforcement_warning_respects_discovered_mode() {
        let provider = provider_config(
            r#"[provider]
name = "test-provider"
api_key = "test-key"
auth_style = "bearer"

[provider.catalog]
mode = "discovered"
enforce = true

[[provider.catalog.models]]
id = "static-model"
"#,
        );
        let directory = tempfile::tempdir().unwrap();

        let warning = catalog_enforcement_warning(&provider, directory.path());

        assert!(warning.is_some());
    }

    #[test]
    fn enforcement_warning_accepts_usable_cache() {
        let provider = provider_config(
            r#"[provider]
name = "test-provider"
api_key = "test-key"
auth_style = "bearer"

[provider.catalog]
mode = "discovered"
enforce = true
"#,
        );
        let directory = tempfile::tempdir().unwrap();
        let cache = llm_proxy_core::CatalogFile {
            catalog: llm_proxy_core::CatalogFileMetadata {
                provider: "test-provider".to_owned(),
                source: "live".to_owned(),
                generated_at: "2026-06-10T00:00:00Z".to_owned(),
                models: vec![llm_proxy_core::StaticModelCatalogEntry {
                    id: "cached-model".to_owned(),
                    display_name: None,
                    supports: Vec::new(),
                    context_length: None,
                }],
            },
        };
        std::fs::write(
            directory.path().join("test-provider.toml"),
            toml::to_string(&cache).unwrap(),
        )
        .unwrap();

        let warning = catalog_enforcement_warning(&provider, directory.path());

        assert!(warning.is_none());
    }

    // -----------------------------------------------------------------------
    // Log format tests (drive the extracted `build_log_layer` helper)
    // -----------------------------------------------------------------------
    //
    // These tests install the composed subscriber as the thread-local default
    // via `Subscriber::set_default` (a scoped guard), NOT via the global
    // `init()`. `init()` can only run once per process and would race with
    // other tests; `set_default` scopes the subscriber to this thread only, so
    // a buffer `MakeWriter` captures exactly the events emitted here. This is
    // the standard `tracing-subscriber` pattern for isolated format tests.

    /// Emit a known `tracing::event!` while `subscriber` is the thread-local
    /// default, returning the captured bytes.
    fn capture_event_for(log_format: &LogFormat) -> Vec<u8> {
        use std::sync::{Arc, Mutex};
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        // `BufferWriter` is a tiny newtype that implements
        // `tracing_subscriber::fmt::MakeWriter` by locking the shared buffer
        // and returning a `MutexGuard`-backed writer. The layer clones the
        // `BufferWriter` (cheap `Arc` clone) internally; we keep the original
        // `Arc` to read the captured bytes after the event is emitted.
        let layer = build_log_layer(log_format, BufferWriter(Arc::clone(&buf)));
        // `SubscriberExt::with` must be in scope to compose the layer onto the
        // registry. `set_default` (not the one-shot global `init`) scopes the
        // subscriber to this thread only, so a buffer `MakeWriter` captures
        // exactly the events emitted here and tests do not race with each other.
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(layer);
        // Scope the subscriber to this thread only. `set_default` (from the
        // `tracing` crate) takes the subscriber by value and returns a guard
        // that restores the prior subscriber on drop. This avoids the global
        // `init()` (one-shot, process-wide, would race with other tests).
        //
        // The fmt layer (and the subscriber+guard that own it) clones the
        // `BufferWriter`/`Arc` internally and may outlive the `event!` call, so
        // this helper does NOT depend on `Arc::try_unwrap` to recover the
        // buffer (that would race the layer's drop and panic spuriously). The
        // captured bytes are read through a shared `lock()` on the live `Arc`
        // instead, which is correct regardless of how many clones exist.
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            tracing::event!(
                target: "log_format_test",
                tracing::Level::INFO,
                message = "log-format-probe",
                probe = "plain-or-json",
            );
        }
        buf.lock().expect("buffer lock not poisoned").clone()
    }

    /// `MakeWriter` implementation backed by a shared, lockable byte buffer.
    ///
    /// `tracing_subscriber::fmt::MakeWriter` is not implemented for
    /// `Arc<Mutex<Vec<u8>>>` directly (the `Arc<W>` blanket impl requires
    /// `W: MakeWriter`, and `Mutex<Vec<u8>>` is not a writer), so this newtype
    /// provides the impl explicitly. Each `make_writer` call returns a
    /// `MutexGuard<'static>`-shaped writer that appends bytes to the shared
    /// buffer; the `MutexGuard` is `Send` and `Sync` and lives as long as the
    /// borrowed `Arc` (which the layer clones and holds for `'static`).
    #[derive(Clone)]
    struct BufferWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriterGuard<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            BufferWriterGuard(self.0.lock().expect("buffer lock not poisoned"))
        }
    }

    /// Write handle for [`BufferWriter`], backed by a `MutexGuard` over the
    /// shared buffer. Implements `Write` by appending directly.
    struct BufferWriterGuard<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

    impl<'a> std::io::Write for BufferWriterGuard<'a> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Minimal structural check that `line` is a single JSON object (starts with
    /// `{`, ends with `}`). A full `serde_json` parse is intentionally avoided
    /// to keep this test off a `serde_json` dev-dependency in the app crate; the
    /// brace check plus key-content assertions below are sufficient to
    /// distinguish the JSON formatter from the human-readable plain formatter.
    fn looks_like_json_object(line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.starts_with('{') && trimmed.ends_with('}')
    }

    #[test]
    fn json_log_format_yields_a_json_object_line() {
        let captured = String::from_utf8(capture_event_for(&LogFormat::Json))
            .expect("captured bytes are UTF-8");
        assert!(
            !captured.is_empty(),
            "a tracing::event! must produce output under LogFormat::Json"
        );
        // Each non-empty line must be a single JSON object (one per event).
        for line in captured.lines() {
            if line.trim().is_empty() {
                continue;
            }
            assert!(
                looks_like_json_object(line),
                "LogFormat::Json line must be a JSON object, got: {line}"
            );
        }
        // The JSON formatter emits `level` and the `fields.message` payload as
        // structured keys. Assert both are present to prove our `event!`
        // reached the JSON formatter and the fields were serialized (not
        // dropped by a future edit to `build_log_layer`).
        assert!(
            captured.contains("\"level\"") && captured.contains("INFO"),
            "JSON log must carry the structured `level` field, got: {captured}"
        );
        assert!(
            captured.contains("log-format-probe"),
            "JSON log must carry the emitted message value, got: {captured}"
        );
    }

    #[test]
    fn plain_log_format_does_not_emit_a_json_object() {
        let captured = String::from_utf8(capture_event_for(&LogFormat::Plain))
            .expect("captured bytes are UTF-8");
        assert!(
            !captured.is_empty(),
            "a tracing::event! must produce output under LogFormat::Plain"
        );
        // Take the first non-empty line. Under Plain it is human-readable text
        // like `2026-... INFO log_format_test: ...` and must NOT be a JSON
        // object — if it were, the format selection in `build_log_layer` would
        // be broken (Plain behaving like Json).
        let first = captured
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("at least one non-empty log line");
        assert!(
            !looks_like_json_object(first),
            "Plain log line must NOT be a JSON object, got: {first}"
        );
        // Sanity: the probe message still reached the formatter.
        assert!(
            captured.contains("log-format-probe"),
            "Plain log must still carry the emitted message, got: {captured}"
        );
    }

    // -----------------------------------------------------------------------
    // [storage] wiring into load_toml_state
    // -----------------------------------------------------------------------

    /// Write a minimal valid main config, optionally with a [storage] section.
    fn write_main_config(
        dir: &std::path::Path,
        storage_section: Option<&str>,
    ) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        let storage = storage_section.unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                r#"
[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"
{storage}"#
            ),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn load_toml_state_wires_sqlite_ledger_when_storage_configured() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("usage.db");
        let storage = format!(
            "
[storage]
path = \"{}\"
",
            db_path.display()
        );
        let path = write_main_config(dir.path(), Some(&storage));

        let state = load_toml_state(
            &path,
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::try_new().unwrap(),
            None,
        )
        .expect("state with [storage]");

        let debug = format!("{state:?}");
        assert!(
            debug.contains("SqliteEventBus"),
            "[storage] must wire the SQLite ledger, got: {debug}"
        );

        // The ledger accepts and persists an event end to end: emit through
        // the wired bus, close (drain), then the database file must exist
        // with content (the writer checkpoints its WAL on close).
        let event =
            llm_proxy_storage::ProxyEvent::RequestReceived(llm_proxy_storage::RequestReceived {
                request_id: "wiring-1".to_owned(),
                timestamp: time::OffsetDateTime::now_utc(),
                caller: "laptop".to_owned(),
                provider: "prov-a".to_owned(),
                route_kind: "messages".to_owned(),
                client_protocol: "anthropic".to_owned(),
                model: llm_proxy_protocol::core::ModelRef {
                    requested: "claude-3".to_owned(),
                    upstream: None,
                },
                streaming: false,
                body_hash: "digest".to_owned(),
            });
        let bus = std::sync::Arc::clone(state.event_bus());
        bus.emit(&event);
        let closer = std::sync::Arc::clone(&bus);
        tokio::task::spawn_blocking(move || llm_proxy_storage::EventBus::close(&*closer))
            .await
            .expect("close");

        let db = std::fs::read(&db_path).expect("database file exists after close");
        assert!(
            !db.is_empty(),
            "database file must hold the committed schema and rows"
        );
    }

    #[tokio::test]
    async fn load_toml_state_keeps_noop_bus_without_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_main_config(dir.path(), None);

        let state = load_toml_state(
            &path,
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::try_new().unwrap(),
            None,
        )
        .expect("state without [storage]");

        let debug = format!("{state:?}");
        assert!(
            debug.contains("NoopBus"),
            "absent [storage] must keep the no-op bus (today's behavior), got: {debug}"
        );
        // And close() on the default bus is a no-op that must not hang.
        llm_proxy_storage::EventBus::close(state.event_bus().as_ref());
    }

    // -----------------------------------------------------------------------
    // [provider.egress] wiring into load_toml_state
    // -----------------------------------------------------------------------

    /// Write a providers/ dir next to the main config with one provider file,
    /// optionally carrying an egress section.
    fn write_provider_file(dir: &std::path::Path, egress: Option<&str>) {
        let egress_section = egress.unwrap_or_default();
        std::fs::create_dir_all(dir.join("providers")).unwrap();
        std::fs::write(
            dir.join("providers/egress-test.toml"),
            format!(
                r#"
[provider]
name = "egress-test"
api_key = "test-key"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
{egress_section}"#
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn load_toml_state_wires_egress_client_only_for_configured_providers() {
        let dir = tempfile::tempdir().unwrap();
        // The proxy URL carries userinfo: it must reach the transport as proxy
        // authentication but must never be printed with the credentials.
        write_provider_file(
            dir.path(),
            Some(
                "[provider.egress]\nproxy = \"socks5h://egress-user:egress-pass@127.0.0.1:1080\"\n",
            ),
        );
        // A second provider with no egress section must keep the shared
        // default client.
        std::fs::write(
            dir.path().join("providers/direct.toml"),
            r#"
[provider]
name = "direct"
api_key = "test-key"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#,
        )
        .unwrap();
        let path = write_main_config(dir.path(), None);

        let state = load_toml_state(
            &path,
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::try_new().unwrap(),
            None,
        )
        .expect("state builds with an egress provider");

        // Observable through the state Debug: the egress map holds exactly the
        // provider that configured egress, and the proxy credentials never
        // print (the credential-free host:port may).
        let debug = format!("{state:?}");
        let egress_map = debug
            .split("egress_clients: ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap_or_default();
        assert!(
            egress_map.contains("egress-test"),
            "the egress provider must have a wired client: {debug}"
        );
        assert!(
            !egress_map.contains("direct"),
            "providers without egress must not appear in the egress map: {debug}"
        );
        assert!(
            !debug.contains("egress-user") && !debug.contains("egress-pass"),
            "proxy credentials must never print in the state Debug: {debug}"
        );
    }

    #[tokio::test]
    async fn load_toml_state_rejects_egress_url_the_transport_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        // Malformed IPv6 host: passes config validation (scheme http, host
        // non-empty) but the transport cannot build a proxy from it, so the
        // serve path must refuse to start rather than fall back to direct.
        write_provider_file(
            dir.path(),
            Some("[provider.egress]\nproxy = \"http://[::1:8080\"\n"),
        );
        let path = write_main_config(dir.path(), None);

        let err = load_toml_state(
            &path,
            llm_proxy_provider::ProviderAdapterRegistry::builtin(),
            llm_proxy_provider::ProxyClient::try_new().unwrap(),
            None,
        )
        .expect_err("unusable egress proxy must be a startup error");

        let message = format!("{err:#}");
        assert!(
            message.contains("egress"),
            "the error must point at the egress wiring: {message}"
        );
        assert!(
            message.contains("egress-test"),
            "the error must name the provider: {message}"
        );
    }

    // -----------------------------------------------------------------------
    // Admin posture startup warnings
    // -----------------------------------------------------------------------

    fn admin_app_config(
        on_main: bool,
        admin_bind: Option<std::net::SocketAddr>,
    ) -> llm_proxy_core::AppConfig {
        use llm_proxy_core::{AdminAuthConfig, ServerConfig, StorageConfig};
        llm_proxy_core::AppConfig {
            server: ServerConfig {
                bind: "127.0.0.1:3456".parse().unwrap(),
                request_timeout: std::time::Duration::from_secs(300),
                shutdown_timeout: std::time::Duration::from_secs(30),
                log_level: "info".to_owned(),
                hot_reload: false,
                allowed_origins: None,
                server_name: "posture-test".to_owned(),
                rate_limit_rpm: 100,
                trust_forwarded_headers: false,
                dedup_window: std::time::Duration::ZERO,
                log_format: Default::default(),
                admin_bind,
                admin_on_main_listener: on_main,
            },
            auth: llm_proxy_core::AuthConfig {
                mode: llm_proxy_core::AuthMode::None,
                clients: std::collections::HashMap::new(),
                admin: Some(AdminAuthConfig {
                    token: secrecy::SecretString::from("admin-secret"),
                }),
            },
            storage: Some(StorageConfig {
                path: "usage.db".into(),
                max_age: None,
                max_rows: None,
                prune_interval: std::time::Duration::from_secs(3600),
            }),
        }
    }

    /// Capture the warnings [`warn_admin_posture`] emits for `config`.
    fn captured_posture_warnings(config: &llm_proxy_core::AppConfig) -> Vec<u8> {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let layer = build_log_layer(&LogFormat::Json, BufferWriter(std::sync::Arc::clone(&buf)));
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(layer);
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            warn_admin_posture(config);
        }
        buf.lock().expect("buffer lock not poisoned").clone()
    }

    #[test]
    fn main_listener_optin_is_warned_about_loudly_by_name() {
        let config = admin_app_config(true, None);
        let captured = String::from_utf8(captured_posture_warnings(&config))
            .expect("captured bytes are UTF-8");
        assert!(
            captured.contains("admin-on-main-listener"),
            "the posture must be named for grepping: {captured}"
        );
        assert!(
            captured.contains("ADMIN SECURITY POSTURE"),
            "the warning must be loud and identifiable: {captured}"
        );
    }

    #[test]
    fn dedicated_loopback_posture_stays_quiet() {
        let config = admin_app_config(false, None);
        let captured = String::from_utf8(captured_posture_warnings(&config))
            .expect("captured bytes are UTF-8");
        assert!(
            !captured.contains("ADMIN SECURITY POSTURE"),
            "the default loopback posture needs no warning: {captured}"
        );
    }

    #[test]
    fn dedicated_non_loopback_bind_is_warned_about() {
        let config = admin_app_config(false, Some("10.0.0.5:9900".parse().unwrap()));
        let captured = String::from_utf8(captured_posture_warnings(&config))
            .expect("captured bytes are UTF-8");
        assert!(
            captured.contains("non-loopback"),
            "a non-loopback admin listener must be called out: {captured}"
        );
    }
}
