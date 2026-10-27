//! Runtime provider model catalog cache and refresh coordination.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use llm_proxy_core::{
    CatalogFile, CatalogFileMetadata, ProviderConfig, StaticModelCatalogEntry, merge_catalog,
};
use llm_proxy_provider::{DiscoveryClient, ProviderError};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

#[derive(Debug, Clone)]
struct RefreshOutcome {
    completed_at: Instant,
    /// Preserved so single-flight followers receive the same error variant
    /// (and therefore the same HTTP status via `map_catalog_error`) as the
    /// leader. Only the variants `map_catalog_error` discriminates on are
    /// preserved; everything else already collapses to `RouteError::Internal`,
    /// so storing it as `InvalidConfig` is lossless.
    error: Option<PreservedError>,
}

/// Category-preserving snapshot of a [`ProviderError`] produced by a refresh.
///
/// `ProviderError` is not `Clone` (it carries a `serde_json::Error`), so the
/// single-flight outcome cache stores this reconstruction instead of the full
/// error. Only the variants [`map_catalog_error`](crate::routes) discriminates
/// on are preserved by variant; all other variants map to `RouteError::Internal`
/// regardless, so they collapse to [`ProviderError::InvalidConfig`] losslessly.
#[derive(Debug, Clone)]
enum PreservedError {
    /// Original was `ProviderError::Http`.
    Http { message: String, timeout: bool },
    /// Original was `ProviderError::Api`.
    Api { status: u16, body: String },
    /// Original was any other variant; only the `Display` string is retained.
    Other(String),
}

impl PreservedError {
    /// Snapshot the category-relevant fields of a [`ProviderError`].
    fn from_provider_error(error: &ProviderError) -> Self {
        match error {
            ProviderError::Http {
                message, timeout, ..
            } => Self::Http {
                message: message.clone(),
                timeout: *timeout,
            },
            ProviderError::Api { status, body } => Self::Api {
                status: *status,
                body: body.clone(),
            },
            other => Self::Other(other.to_string()),
        }
    }

    /// Reconstruct a [`ProviderError`] that maps to the same HTTP status as the
    /// original (the `redirect` flag is irrelevant to status mapping).
    fn to_provider_error(&self) -> ProviderError {
        match self {
            Self::Http { message, timeout } => ProviderError::Http {
                message: message.clone(),
                timeout: *timeout,
                redirect: false,
            },
            Self::Api { status, body } => ProviderError::Api {
                status: *status,
                body: body.clone(),
            },
            Self::Other(message) => ProviderError::InvalidConfig(message.clone()),
        }
    }
}

/// Cached, generation-tagged catalog membership index (audit LOW-9).
#[derive(Clone, Debug)]
struct IndexedCatalog {
    /// The `cache_generations` value this index was built from; a mismatch
    /// against the live generation marks the index stale and forces a rebuild.
    generation: u64,
    ids: Arc<HashSet<String>>,
}

/// Mutable catalog state kept separate from immutable provider routing config.
#[derive(Debug)]
pub struct ModelCatalogService {
    cache_dir: Option<PathBuf>,
    cache: RwLock<HashMap<String, CatalogFile>>,
    /// Bumped every time the discovered `cache` entry for a provider is
    /// replaced, so `id_index` can detect staleness without holding a lock
    /// across the network refresh (audit LOW-9).
    cache_generations: Mutex<HashMap<String, u64>>,
    /// Cached O(1) membership index of catalog model ids, keyed by provider
    /// name (audit LOW-9). Built from the merged catalog; tagged with the
    /// `cache` generation it reflects so a concurrent refresh forces a rebuild
    /// instead of serving a stale set.
    id_index: RwLock<HashMap<String, IndexedCatalog>>,
    refresh_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    refresh_outcomes: Mutex<HashMap<String, RefreshOutcome>>,
    discovery: DiscoveryClient,
}

impl ModelCatalogService {
    /// Create a catalog service. When `cache_dir` is absent, disk caching is disabled.
    #[must_use]
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        Self {
            cache_dir,
            cache: RwLock::new(HashMap::new()),
            cache_generations: Mutex::new(HashMap::new()),
            id_index: RwLock::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            refresh_outcomes: Mutex::new(HashMap::new()),
            discovery: DiscoveryClient::default(),
        }
    }

    /// Return the merged, filtered catalog, optionally refreshing it live.
    ///
    /// # Errors
    ///
    /// Returns an error when a live refresh fails without stale/static fallback,
    /// or when a cache file is unsafe or malformed.
    pub async fn catalog(
        &self,
        provider: &ProviderConfig,
        refresh_live: bool,
    ) -> Result<Vec<StaticModelCatalogEntry>, ProviderError> {
        let config_ref = provider.catalog.as_ref();
        self.load_disk_cache(provider).await?;
        let discovery_enabled = config_ref
            .is_some_and(|c| !matches!(c.mode, llm_proxy_core::ProviderCatalogMode::Static))
            && provider.discovery.is_some();
        if refresh_live && discovery_enabled && !self.cache_is_fresh(provider).await {
            if let Err(error) = self.refresh(provider).await {
                let stale = self
                    .cache
                    .read()
                    .await
                    .get(&provider.name)
                    .map(|file| file.catalog.models.clone())
                    .unwrap_or_default();
                let merged = merge_catalog(&config_ref.cloned().unwrap_or_default(), &stale);
                if merged.is_empty() {
                    return Err(error);
                }
                tracing::warn!(provider = %provider.name, error = %error, "catalog refresh failed; using stale/static catalog");
            }
        }

        let discovered = self
            .cache
            .read()
            .await
            .get(&provider.name)
            .map(|file| file.catalog.models.clone())
            .unwrap_or_default();
        Ok(merge_catalog(
            &config_ref.cloned().unwrap_or_default(),
            &discovered,
        ))
    }

    /// O(1) catalog-enforcement membership check (audit LOW-9).
    ///
    /// Replaces the per-request O(N) linear scan over the merged catalog that
    /// `resolve_target` used to perform via `catalog()` + a `Vec::iter().any`
    /// check. Backed by a cached `HashSet` of catalog model ids (built by
    /// `id_index_for`), rebuilt automatically when the provider's discovered
    /// catalog is refreshed or loaded from disk.
    ///
    /// `protocol` follows the same contract as the former linear scan: the
    /// `"gemini_generate_content"` protocol strips a leading `models/` prefix
    /// from the request and accepts either spelling of the catalog id, so a
    /// request for `"gemini-2.5-pro"` matches a catalog id of
    /// `"models/gemini-2.5-pro"` (and vice-versa); every other protocol
    /// requires an exact id match.
    ///
    /// # Errors
    ///
    /// Returns an error when the on-disk catalog cache cannot be loaded
    /// (mirrors the failure mode of [`Self::catalog`]).
    pub async fn contains_model(
        &self,
        provider: &ProviderConfig,
        upstream_model: &str,
        protocol: &str,
    ) -> Result<bool, ProviderError> {
        // Load the on-disk cache first so the index reflects the latest
        // persisted catalog. Mirrors the load prelude of `catalog()`.
        self.load_disk_cache(provider).await?;
        let ids = self.id_index_for(provider).await?;
        Ok(model_id_present(&ids, upstream_model, protocol))
    }

    /// Return the cached membership index for `provider`, rebuilding it from
    /// the merged catalog when absent or stale (audit LOW-9).
    ///
    /// Staleness is tracked via [`Self::current_generation`]: the index is
    /// tagged with the generation it was built from, and a mismatch against the
    /// live generation (advanced by every discovered-cache mutation) forces a
    /// rebuild. The rebuild re-checks the generation before storing, so a
    /// concurrent refresh can never leave a stale index behind.
    async fn id_index_for(
        &self,
        provider: &ProviderConfig,
    ) -> Result<Arc<HashSet<String>>, ProviderError> {
        let name = provider.name.as_str();
        let gen_before = self.current_generation(name);

        // Fast path: a current-generation index already exists.
        {
            let guard = self.id_index.read().await;
            if let Some(indexed) = guard.get(name) {
                if indexed.generation == gen_before {
                    return Ok(Arc::clone(&indexed.ids));
                }
            }
        }

        // Slow path: build from the merged catalog (config allow/deny/static
        // applied to the discovered models), exactly as `catalog()` returns.
        let config = provider.catalog.as_ref().cloned().unwrap_or_default();
        let discovered = self
            .cache
            .read()
            .await
            .get(name)
            .map(|file| file.catalog.models.clone())
            .unwrap_or_default();
        let merged = merge_catalog(&config, &discovered);
        let ids = Arc::new(normalized_ids(&merged));

        // Store only if the discovered cache did not change under us. If a
        // concurrent refresh advanced the generation, drop this build and let
        // the next caller rebuild — a stale index is never persisted.
        let gen_after = self.current_generation(name);
        if gen_before == gen_after {
            let mut guard = self.id_index.write().await;
            guard.insert(
                provider.name.clone(),
                IndexedCatalog {
                    generation: gen_before,
                    ids: Arc::clone(&ids),
                },
            );
        }
        Ok(ids)
    }

    /// Current generation of the discovered cache for `provider` (0 when never
    /// populated). Advanced by [`Self::bump_cache_generation`].
    fn current_generation(&self, provider: &str) -> u64 {
        self.cache_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .copied()
            .unwrap_or(0)
    }

    /// Advance the generation for `provider`, marking any cached membership
    /// index stale. Called after every mutation of the discovered cache.
    fn bump_cache_generation(&self, provider: &str) {
        let mut guard = self
            .cache_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard.entry(provider.to_owned()).or_insert(0) += 1;
    }

    async fn refresh(&self, provider: &ProviderConfig) -> Result<(), ProviderError> {
        let requested_at = Instant::now();
        let lock = {
            let mut locks = self.refresh_locks.lock().map_err(|error| {
                ProviderError::InvalidConfig(format!("catalog refresh lock is poisoned: {error}"))
            })?;
            Arc::clone(
                locks
                    .entry(provider.name.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        let _guard = lock.lock().await;
        if let Some(outcome) = self
            .refresh_outcomes
            .lock()
            .map_err(|error| {
                ProviderError::InvalidConfig(format!("catalog refresh state is poisoned: {error}"))
            })?
            .get(&provider.name)
            .filter(|outcome| outcome.completed_at >= requested_at)
            .cloned()
        {
            return outcome.error.map_or(Ok(()), |preserved| {
                // Reconstruct the typed error so single-flight followers receive
                // the same error variant -- and therefore the same HTTP status
                // via `map_catalog_error` -- as the leader (e.g. an upstream
                // timeout stays a 504, not a 500 Internal).
                Err(preserved.to_provider_error())
            });
        }

        let models = match self.discovery.discover(provider).await {
            Ok(models) => models,
            Err(error) => {
                self.record_refresh_outcome(
                    &provider.name,
                    Some(PreservedError::from_provider_error(&error)),
                )?;
                return Err(error);
            }
        };
        let file = CatalogFile {
            catalog: CatalogFileMetadata {
                provider: provider.name.clone(),
                source: "live".to_owned(),
                generated_at: now_rfc3339()?,
                models,
            },
        };
        if let Some(cache_dir) = &self.cache_dir {
            let cache_dir = cache_dir.clone();
            let file_to_write = file.clone();
            let write_result = tokio::task::spawn_blocking(move || {
                write_catalog_atomic(&cache_dir, &file_to_write)
            })
            .await
            .map_err(|error| {
                if error.is_panic() {
                    tracing::warn!(
                        "catalog cache write task panicked; this may indicate a bug in the \
                         serialization or filesystem code"
                    );
                }
                ProviderError::InvalidConfig(format!("catalog cache write task failed: {error}"))
            })?;
            if let Err(error) = write_result {
                self.record_refresh_outcome(
                    &provider.name,
                    Some(PreservedError::from_provider_error(&error)),
                )?;
                return Err(error);
            }
        }
        self.cache.write().await.insert(provider.name.clone(), file);
        // The discovered catalog changed; advance the generation so any cached
        // membership index is rebuilt on next access (audit LOW-9).
        self.bump_cache_generation(&provider.name);
        self.record_refresh_outcome(&provider.name, None)?;
        Ok(())
    }

    fn record_refresh_outcome(
        &self,
        provider: &str,
        error: Option<PreservedError>,
    ) -> Result<(), ProviderError> {
        self.refresh_outcomes
            .lock()
            .map_err(|error| {
                ProviderError::InvalidConfig(format!("catalog refresh state is poisoned: {error}"))
            })?
            .insert(
                provider.to_owned(),
                RefreshOutcome {
                    completed_at: Instant::now(),
                    error,
                },
            );
        Ok(())
    }

    async fn cache_is_fresh(&self, provider: &ProviderConfig) -> bool {
        let ttl = provider
            .catalog
            .as_ref()
            .map_or_else(std::time::Duration::default, |c| c.cache_ttl);
        // Clamp the TTL seconds to i64 range to avoid silent wrapping on
        // absurdly large values. In practice, TTL is always well within range.
        let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        self.cache
            .read()
            .await
            .get(&provider.name)
            .is_some_and(|file| {
                time::OffsetDateTime::parse(
                    &file.catalog.generated_at,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok()
                .map(|generated_at| time::OffsetDateTime::now_utc() - generated_at)
                .is_some_and(|age| age.whole_seconds() >= 0 && age.whole_seconds() < ttl_secs)
            })
    }

    async fn load_disk_cache(&self, provider: &ProviderConfig) -> Result<(), ProviderError> {
        if !is_safe_provider_slug(&provider.name) {
            return Err(ProviderError::InvalidConfig(
                "catalog provider must be a lowercase URL-safe slug".to_owned(),
            ));
        }
        let Some(cache_dir) = &self.cache_dir else {
            // No disk cache: nothing to load.
            return Ok(());
        };
        // Fast path: if this provider's catalog is already in memory, return
        // without taking the write lock at all, avoiding contention on the
        // common (already-loaded) request path.
        if self.cache.read().await.contains_key(&provider.name) {
            return Ok(());
        }
        // Cold path: stat + read + parse OUTSIDE any lock. The write lock is
        // global across all providers, so holding it across disk I/O would
        // serialize every concurrent catalog()/contains_model() call for every
        // other provider (head-of-line blocking). Two concurrent first-load
        // callers for the same provider may both read and parse the same file;
        // that is harmless -- the second insert overwrites byte-identical
        // content, and the extra bump_cache_generation only forces one cheap
        // id_index rebuild (which re-checks the generation before storing).
        let path = cache_dir.join(format!("{}.toml", provider.name));
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(invalid_cache(&error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ProviderError::InvalidConfig(format!(
                "catalog cache must be a regular non-symlink file: {}",
                path.display()
            )));
        }
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| invalid_cache(&error))?;
        let file: CatalogFile = toml::from_str(&raw).map_err(|error| {
            ProviderError::InvalidConfig(format!(
                "failed to parse catalog cache {}: {error}",
                path.display()
            ))
        })?;
        if file.catalog.provider != provider.name {
            return Err(ProviderError::InvalidConfig(format!(
                "catalog cache provider mismatch in {}",
                path.display()
            )));
        }
        // Critical section: re-check (another caller may have populated it) and
        // insert. Bump the generation after dropping the guard so it does not
        // run under the write lock.
        {
            let mut cache = self.cache.write().await;
            if cache.contains_key(&provider.name) {
                return Ok(());
            }
            cache.insert(provider.name.clone(), file);
        }
        // The discovered catalog changed; advance the generation so any cached
        // membership index is rebuilt on next access (audit LOW-9).
        self.bump_cache_generation(&provider.name);
        Ok(())
    }
}

/// Build a `HashSet` of raw catalog model ids from a merged catalog slice
/// (audit LOW-9). The gemini `models/`-prefix normalization is handled at
/// lookup time in [`model_id_present`], so this stores only the verbatim ids.
fn normalized_ids(merged: &[StaticModelCatalogEntry]) -> HashSet<String> {
    let mut ids = HashSet::with_capacity(merged.len());
    for entry in merged {
        ids.insert(entry.id.clone());
    }
    ids
}

/// O(1) membership check against a catalog id set, preserving the exact
/// semantics of the former linear `catalog_contains_model` scan (audit LOW-9):
///
/// - `"gemini_generate_content"` strips a leading `models/` from the request
///   and matches either the bare id or the `models/`-prefixed form present in
///   the catalog, so a request for `"gemini-2.5-pro"` matches a catalog id of
///   `"models/gemini-2.5-pro"` and vice-versa.
/// - Every other protocol requires an exact id match (no prefix stripping).
fn model_id_present(ids: &HashSet<String>, upstream_model: &str, protocol: &str) -> bool {
    if protocol == "gemini_generate_content" {
        let requested = upstream_model
            .strip_prefix("models/")
            .unwrap_or(upstream_model);
        // The catalog may store either the bare id or the `models/`-prefixed
        // form; accept either spelling (mirrors the former strip-prefix scan).
        return ids.contains(requested) || {
            let prefixed = format!("models/{requested}");
            ids.contains(prefixed.as_str())
        };
    }
    ids.contains(upstream_model)
}

fn invalid_cache(error: &std::io::Error) -> ProviderError {
    ProviderError::InvalidConfig(format!("catalog cache I/O failed: {error}"))
}

/// Format the current UTC time as an RFC 3339 string.
///
/// # Errors
///
/// Returns an error if formatting the timestamp fails. Propagated (rather
/// than falling back to the Unix epoch) because an epoch sentinel would make
/// the cache perpetually stale: `cache_is_fresh` computes
/// `now_utc() - generated_at`, and an epoch `generated_at` yields a huge
/// positive age, forcing a network refresh on every `catalog()` request.
/// Failing the refresh loudly is the safe direction — the caller already
/// handles refresh failures with a stale/static fallback (audit:
/// now-rfc3339-epoch-fallback).
fn now_rfc3339() -> Result<String, ProviderError> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| {
            ProviderError::InvalidConfig(format!("failed to format catalog timestamp: {error}"))
        })
}

/// Persist a catalog using a temporary file and atomic rename.
///
/// # Errors
///
/// Returns an error for unsafe paths, serialization failures, or filesystem errors.
pub fn write_catalog_atomic(cache_dir: &Path, file: &CatalogFile) -> Result<(), ProviderError> {
    if !is_safe_provider_slug(&file.catalog.provider) {
        return Err(ProviderError::InvalidConfig(
            "catalog provider must be a lowercase URL-safe slug".to_owned(),
        ));
    }
    std::fs::create_dir_all(cache_dir).map_err(|error| invalid_cache(&error))?;
    let metadata = std::fs::symlink_metadata(cache_dir).map_err(|error| invalid_cache(&error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProviderError::InvalidConfig(format!(
            "catalog cache directory must be a non-symlink directory: {}",
            cache_dir.display()
        )));
    }
    let destination = cache_dir.join(format!("{}.toml", file.catalog.provider));
    if destination.exists() {
        let metadata =
            std::fs::symlink_metadata(&destination).map_err(|error| invalid_cache(&error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ProviderError::InvalidConfig(format!(
                "catalog destination is not a regular file: {}",
                destination.display()
            )));
        }
    }
    let temporary = cache_dir.join(format!(
        ".{}.{}.toml.tmp",
        file.catalog.provider,
        uuid::Uuid::new_v4()
    ));
    let body = toml::to_string_pretty(file).map_err(|error| {
        ProviderError::InvalidConfig(format!("failed to serialize catalog cache: {error}"))
    })?;
    let write_result = (|| {
        use std::io::Write;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| invalid_cache(&error))?;
        output
            .write_all(body.as_bytes())
            .map_err(|error| invalid_cache(&error))?;
        output.sync_all().map_err(|error| invalid_cache(&error))?;
        std::fs::rename(&temporary, &destination).map_err(|error| invalid_cache(&error))
    })();
    if write_result.is_err()
        && let Err(error) = std::fs::remove_file(&temporary)
    {
        tracing::warn!(path = %temporary.display(), %error, "failed to remove temporary catalog cache file");
    }
    write_result?;
    Ok(())
}

/// Maximum allowed length for a provider slug.
const MAX_PROVIDER_SLUG_LEN: usize = 128;

fn is_safe_provider_slug(provider: &str) -> bool {
    !provider.is_empty()
        && provider.len() <= MAX_PROVIDER_SLUG_LEN
        && provider.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

impl Default for ModelCatalogService {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use llm_proxy_core::{
        AuthStyle, ProviderCatalogConfig, ProviderCatalogMode, ProviderDiscoveryConfig,
        ProviderDiscoveryKind, ProviderRoutesConfig,
    };

    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("llm-proxy-catalog-test-{}", uuid::Uuid::new_v4()))
    }

    fn provider(endpoint: String) -> ProviderConfig {
        ProviderConfig {
            name: "test-provider".to_owned(),
            api_key: secrecy::SecretString::from("test-key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: Some(ProviderDiscoveryConfig {
                kind: ProviderDiscoveryKind::OpenAiCompatibleModels,
                endpoint,
                headers: HashMap::new(),
                max_pages: 100,
                max_models: 20_000,
                max_response_bytes: 4 * 1024 * 1024,
            }),
            catalog: Some(ProviderCatalogConfig {
                mode: ProviderCatalogMode::Discovered,
                enforce: false,
                cache_ttl: Duration::from_secs(60),
                allow: vec!["*".to_owned()],
                deny: Vec::new(),
                models: Vec::new(),
            }),
            pricing: Default::default(),
        }
    }

    fn catalog_file(generated_at: &str) -> CatalogFile {
        CatalogFile {
            catalog: CatalogFileMetadata {
                provider: "test-provider".to_owned(),
                source: "live".to_owned(),
                generated_at: generated_at.to_owned(),
                models: vec![StaticModelCatalogEntry {
                    id: "cached-model".to_owned(),
                    display_name: None,
                    supports: Vec::new(),
                    context_length: None,
                }],
            },
        }
    }

    async fn failing_models(State(count): State<Arc<AtomicUsize>>) -> StatusCode {
        count.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(75)).await;
        StatusCode::INTERNAL_SERVER_ERROR
    }

    async fn failing_server() -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/models", get(failing_models))
            .with_state(Arc::clone(&count));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/models"), count)
    }

    #[test]
    fn atomic_catalog_write_round_trips() {
        let directory = temp_dir();
        let file = catalog_file("2026-06-10T00:00:00Z");
        write_catalog_atomic(&directory, &file).unwrap();
        let raw = std::fs::read_to_string(directory.join("test-provider.toml")).unwrap();
        let decoded: CatalogFile = toml::from_str(&raw).unwrap();
        assert_eq!(decoded.catalog.models[0].id, "cached-model");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_catalog_write_rejects_unsafe_provider_name() {
        let directory = temp_dir();
        let mut file = catalog_file("2026-06-10T00:00:00Z");
        file.catalog.provider = "../outside".to_owned();
        assert!(write_catalog_atomic(&directory, &file).is_err());
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn concurrent_failed_refreshes_are_single_flight() {
        let (endpoint, count) = failing_server().await;
        let provider = Arc::new(provider(endpoint));
        let service = Arc::new(ModelCatalogService::new(None));
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let provider = Arc::clone(&provider);
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                service.catalog(&provider, true).await
            }));
        }
        for task in tasks {
            task.await
                .unwrap()
                .expect_err("every single-flight follower should observe the refresh failure");
        }
        // GAP-LOW-4: discovery retries transient (5xx/transport) failures up to
        // `MAX_TRANSIENT_RETRIES` times after the first attempt, so a
        // persistently-failing endpoint is probed `1 + MAX_TRANSIENT_RETRIES`
        // times. Single-flight still holds — only one refresh actually reaches
        // `discover`; the others reuse the cached failure outcome — so the
        // count is exactly the per-call probe budget, not 8× it.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1 + DiscoveryClient::MAX_TRANSIENT_RETRIES as usize
        );
    }

    #[tokio::test]
    async fn failed_refresh_uses_stale_disk_catalog() {
        let (endpoint, count) = failing_server().await;
        let directory = temp_dir();
        write_catalog_atomic(&directory, &catalog_file("2020-01-01T00:00:00Z")).unwrap();
        let service = ModelCatalogService::new(Some(directory.clone()));
        let models = service.catalog(&provider(endpoint), true).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "cached-model");
        // GAP-LOW-4: discovery retries transient (5xx/transport) failures up to
        // `MAX_TRANSIENT_RETRIES` times after the first attempt, so a
        // persistently-failing endpoint is probed `1 + MAX_TRANSIENT_RETRIES`
        // times. Single-flight still holds — only one refresh actually reaches
        // `discover`; the others reuse the cached failure outcome — so the
        // count is exactly the per-call probe budget, not 8× it.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1 + DiscoveryClient::MAX_TRANSIENT_RETRIES as usize
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn discovered_mode_does_not_treat_static_models_as_refresh_fallback() {
        let (endpoint, count) = failing_server().await;
        let mut provider = provider(endpoint);
        provider.catalog.as_mut().unwrap().models = vec![StaticModelCatalogEntry {
            id: "static-only".to_owned(),
            display_name: None,
            supports: Vec::new(),
            context_length: None,
        }];
        let service = ModelCatalogService::new(None);

        let result = service.catalog(&provider, true).await;

        result.expect_err("catalog refresh should fail when the endpoint persistently errors");
        // GAP-LOW-4: discovery retries transient (5xx/transport) failures up to
        // `MAX_TRANSIENT_RETRIES` times after the first attempt, so a
        // persistently-failing endpoint is probed `1 + MAX_TRANSIENT_RETRIES`
        // times. Single-flight still holds — only one refresh actually reaches
        // `discover`; the others reuse the cached failure outcome — so the
        // count is exactly the per-call probe budget, not 8× it.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1 + DiscoveryClient::MAX_TRANSIENT_RETRIES as usize
        );
    }

    // -- load_disk_cache validation edge cases ---------------------------------

    fn minimal_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_owned(),
            api_key: secrecy::SecretString::from(""),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        }
    }

    #[tokio::test]
    async fn load_disk_cache_rejects_symlink() {
        let directory = temp_dir();
        std::fs::create_dir_all(&directory).unwrap();

        // Write a real cache file, then replace it with a symlink.
        let real = directory.join("real.toml");
        std::fs::write(&real, "not used").unwrap();
        let link = directory.join("symlink-provider.toml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = minimal_provider("symlink-provider");
        let result = service.load_disk_cache(&provider).await;

        assert!(result.is_err(), "symlinked cache files must be rejected");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("non-symlink"),
            "error should mention symlink requirement, got: {msg}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn load_disk_cache_rejects_non_regular_file() {
        let directory = temp_dir();
        std::fs::create_dir_all(&directory).unwrap();

        // Create a FIFO (named pipe) which is neither a regular file nor a symlink.
        let fifo = directory.join("fifo-provider.toml");
        #[cfg(unix)]
        {
            use std::process::Command;
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo must be available");
        }

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = minimal_provider("fifo-provider");
        let result = service.load_disk_cache(&provider).await;

        assert!(result.is_err(), "non-regular files must be rejected");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("non-symlink") || msg.contains("regular"),
            "error should mention file type requirement, got: {msg}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn load_disk_cache_rejects_provider_name_mismatch() {
        let directory = temp_dir();
        std::fs::create_dir_all(&directory).unwrap();

        // Write a cache file whose internal provider name differs from the filename.
        let mut file = catalog_file("2026-06-10T00:00:00Z");
        file.catalog.provider = "other-provider".to_owned();
        let raw = toml::to_string_pretty(&file).unwrap();
        std::fs::write(directory.join("test-provider.toml"), &raw).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = minimal_provider("test-provider");
        let result = service.load_disk_cache(&provider).await;

        assert!(result.is_err(), "provider name mismatch must be rejected");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("mismatch"),
            "error should mention mismatch, got: {msg}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn load_disk_cache_rejects_malformed_toml() {
        let directory = temp_dir();
        std::fs::create_dir_all(&directory).unwrap();

        std::fs::write(
            directory.join("bad-toml.toml"),
            "this is not {{{ valid toml",
        )
        .unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = minimal_provider("bad-toml");
        let result = service.load_disk_cache(&provider).await;

        assert!(result.is_err(), "malformed TOML must be rejected");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("parse"),
            "error should mention parsing, got: {msg}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    // -- is_safe_provider_slug tests -------------------------------------------

    #[test]
    fn safe_slug_accepts_valid_names() {
        assert!(is_safe_provider_slug("openai"));
        assert!(is_safe_provider_slug("my-provider"));
        assert!(is_safe_provider_slug("provider_123"));
        assert!(is_safe_provider_slug("a"));
    }

    #[test]
    fn safe_slug_rejects_empty() {
        assert!(!is_safe_provider_slug(""));
    }

    #[test]
    fn safe_slug_rejects_uppercase() {
        assert!(!is_safe_provider_slug("OpenAI"));
    }

    #[test]
    fn safe_slug_rejects_path_traversal() {
        assert!(!is_safe_provider_slug("../outside"));
        assert!(!is_safe_provider_slug("a/b"));
    }

    #[test]
    fn safe_slug_rejects_exceeds_max_length() {
        let long_name = "a".repeat(129);
        assert!(
            !is_safe_provider_slug(&long_name),
            "129-char name should be rejected"
        );
        let at_limit = "a".repeat(128);
        assert!(
            is_safe_provider_slug(&at_limit),
            "128-char name should be accepted"
        );
    }

    // -- cache_is_fresh logic tests --------------------------------------------

    #[tokio::test]
    async fn cache_is_fresh_returns_true_within_ttl() {
        let directory = temp_dir();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        write_catalog_atomic(&directory, &catalog_file(&now)).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = provider("http://127.0.0.1:0/models".to_owned());
        service.load_disk_cache(&provider).await.unwrap();
        assert!(
            service.cache_is_fresh(&provider).await,
            "cache with current timestamp should be fresh within TTL"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cache_is_fresh_returns_false_for_expired() {
        let directory = temp_dir();
        // generated_at far in the past (well beyond any reasonable TTL)
        write_catalog_atomic(&directory, &catalog_file("2020-01-01T00:00:00Z")).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = provider("http://127.0.0.1:0/models".to_owned());
        service.load_disk_cache(&provider).await.unwrap();
        assert!(
            !service.cache_is_fresh(&provider).await,
            "cache with 2020 timestamp should be expired"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cache_is_fresh_returns_false_when_empty() {
        let service = ModelCatalogService::new(None);
        let provider = provider("http://127.0.0.1:0/models".to_owned());
        assert!(
            !service.cache_is_fresh(&provider).await,
            "cache with no entries should not be fresh"
        );
    }

    // -- cache_is_fresh boundary tests (findings 4, 10) ------------------------

    #[tokio::test]
    async fn cache_is_fresh_with_zero_ttl_is_always_stale() {
        let directory = temp_dir();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        write_catalog_atomic(&directory, &catalog_file(&now)).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let mut provider = provider("http://127.0.0.1:0/models".to_owned());
        provider.catalog.as_mut().unwrap().cache_ttl = Duration::ZERO;
        service.load_disk_cache(&provider).await.unwrap();
        assert!(
            !service.cache_is_fresh(&provider).await,
            "cache with zero TTL should always be stale"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cache_is_fresh_with_future_timestamp_is_stale() {
        let directory = temp_dir();
        // A timestamp far in the future (clock skew)
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let future_str = future
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        write_catalog_atomic(&directory, &catalog_file(&future_str)).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = provider("http://127.0.0.1:0/models".to_owned());
        service.load_disk_cache(&provider).await.unwrap();
        assert!(
            !service.cache_is_fresh(&provider).await,
            "cache with future timestamp should be stale (age < 0)"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cache_is_fresh_with_malformed_timestamp_is_stale() {
        let directory = temp_dir();
        write_catalog_atomic(&directory, &catalog_file("not-a-valid-timestamp")).unwrap();

        let service = ModelCatalogService::new(Some(directory.clone()));
        let provider = provider("http://127.0.0.1:0/models".to_owned());
        service.load_disk_cache(&provider).await.unwrap();
        assert!(
            !service.cache_is_fresh(&provider).await,
            "cache with malformed timestamp should be stale"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    // -- Concurrent successful refresh test (finding 13) -----------------------

    async fn succeeding_server(model_ids: Vec<&str>) -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let models_json = model_ids
            .iter()
            .map(|id| format!("{{\"id\":\"{id}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!("{{\"object\":\"list\",\"data\":[{models_json}]}}");
        let body_clone = body.clone();
        let count_clone = Arc::clone(&count);
        let app = Router::new().route(
            "/models",
            get(move || {
                let b = body_clone.clone();
                let c = Arc::clone(&count_clone);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        b,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/models"), count)
    }

    #[tokio::test]
    async fn concurrent_successful_refreshes_are_single_flight() {
        let (endpoint, count) = succeeding_server(vec!["model-a", "model-b"]).await;
        let provider = Arc::new(provider(endpoint));
        let service = Arc::new(ModelCatalogService::new(None));
        let barrier = Arc::new(tokio::sync::Barrier::new(6));
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let provider = Arc::clone(&provider);
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                service.catalog(&provider, true).await
            }));
        }
        let mut all_ok = true;
        for task in tasks {
            let result = task.await.unwrap();
            if let Ok(models) = &result {
                assert_eq!(models.len(), 2, "should have 2 models");
            } else {
                all_ok = false;
            }
        }
        assert!(all_ok, "all concurrent catalog() calls should succeed");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "only one HTTP request should be made (single-flight)"
        );
    }

    // -- contains_model O(1) index (audit LOW-9) -------------------------------

    /// A provider whose catalog is purely `Static` (no discovery), so the
    /// membership index is built lazily from config and never invalidated.
    fn static_catalog_provider(name: &str, models: Vec<StaticModelCatalogEntry>) -> ProviderConfig {
        ProviderConfig {
            name: name.to_owned(),
            api_key: secrecy::SecretString::from(""),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: Some(ProviderCatalogConfig {
                mode: ProviderCatalogMode::Static,
                enforce: true,
                cache_ttl: Duration::from_secs(60),
                allow: vec!["*".to_owned()],
                deny: Vec::new(),
                models,
            }),
            pricing: Default::default(),
        }
    }

    fn model_entry(id: &str) -> StaticModelCatalogEntry {
        StaticModelCatalogEntry {
            id: id.to_owned(),
            display_name: None,
            supports: Vec::new(),
            context_length: None,
        }
    }

    #[tokio::test]
    async fn contains_model_static_catalog_no_cache() {
        let provider = static_catalog_provider("static-only", vec![model_entry("static-a")]);
        let service = ModelCatalogService::new(None);
        assert!(
            service
                .contains_model(&provider, "static-a", "openai_chat")
                .await
                .unwrap(),
            "configured static model must be allowed"
        );
        assert!(
            !service
                .contains_model(&provider, "static-b", "openai_chat")
                .await
                .unwrap(),
            "absent model must be rejected"
        );
    }

    #[tokio::test]
    async fn contains_model_gemini_prefix_normalization() {
        // Catalog stores the `models/`-prefixed form; gemini requests may omit
        // the prefix, non-gemini requests must not strip it.
        let provider =
            static_catalog_provider("gemini-prov", vec![model_entry("models/gemini-2.5-pro")]);
        let service = ModelCatalogService::new(None);
        assert!(
            service
                .contains_model(&provider, "gemini-2.5-pro", "gemini_generate_content")
                .await
                .unwrap(),
            "gemini: bare request must match prefixed catalog id"
        );
        assert!(
            service
                .contains_model(
                    &provider,
                    "models/gemini-2.5-pro",
                    "gemini_generate_content"
                )
                .await
                .unwrap(),
            "gemini: prefixed request must match prefixed catalog id"
        );
        assert!(
            !service
                .contains_model(&provider, "gemini-2.0-flash", "gemini_generate_content")
                .await
                .unwrap(),
            "gemini: unrelated model must be rejected"
        );
        // Non-gemini protocol must NOT strip the prefix — exact match only.
        assert!(
            service
                .contains_model(&provider, "models/gemini-2.5-pro", "openai_chat")
                .await
                .unwrap(),
            "non-gemini: exact prefixed id must match"
        );
        assert!(
            !service
                .contains_model(&provider, "gemini-2.5-pro", "openai_chat")
                .await
                .unwrap(),
            "non-gemini: bare request must NOT match prefixed catalog id"
        );
    }

    #[tokio::test]
    async fn contains_model_index_invalidated_after_refresh() {
        // Seed the disk cache with the default "cached-model" so the first
        // membership check builds an index from it.
        let directory = temp_dir();
        write_catalog_atomic(&directory, &catalog_file("2026-06-10T00:00:00Z")).unwrap();
        let (endpoint, _count) = succeeding_server(vec!["new-model"]).await;
        let mut provider = provider(endpoint);
        // Zero TTL forces the next live catalog() call to refresh, which
        // replaces the discovered cache and bumps the generation.
        provider.catalog.as_mut().unwrap().cache_ttl = Duration::ZERO;
        let service = ModelCatalogService::new(Some(directory.clone()));

        // First check loads the disk cache ("cached-model") and caches the index.
        assert!(
            service
                .contains_model(&provider, "cached-model", "openai_chat")
                .await
                .unwrap(),
            "seeded model must be present before refresh"
        );

        // A live refresh replaces the discovered catalog with "new-model" and
        // bumps the generation, forcing the index to rebuild.
        service.catalog(&provider, true).await.unwrap();

        assert!(
            !service
                .contains_model(&provider, "cached-model", "openai_chat")
                .await
                .unwrap(),
            "stale model must be gone after refresh (index invalidated)"
        );
        assert!(
            service
                .contains_model(&provider, "new-model", "openai_chat")
                .await
                .unwrap(),
            "refreshed model must be present"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }
}
