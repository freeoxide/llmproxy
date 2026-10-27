//! Provider registry: load provider TOML files and resolve route targets to
//! adapter configurations.
//!
//! # Provider-based routing
//!
//! ```text
//! provider_name + route_kind  -> provider config + route -> adapter name
//! adapter_name                -> protocol + endpoint + headers
//! requested_model             -> model_aliases -> upstream_model
//! ```
//!
//! The registry does **not**:
//!
//! - translate protocol fields
//! - parse response or stream chunks
//! - build final protocol-specific URL paths
//! - inspect messages or content
//! - mutate sampling, tools, metadata, reasoning, cache, or stream intent
//! - infer protocol families from model names

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use secrecy::SecretString;

use crate::error::CoreError;
use crate::provider_config::{
    AuthStyle, ModelId, ModelPricing, ProviderConfig, ProviderRouteKind, StaticModelCatalogEntry,
    endpoint_without_query, load_provider_config,
};

/// Typed failures from resolving a provider-scoped route.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderRouteResolutionError {
    /// The requested provider is not registered.
    #[error("unknown provider \"{provider}\": no provider config loaded with this name")]
    UnknownProvider {
        /// Requested provider name.
        provider: String,
    },
    /// The provider has no adapter mapped for the requested route kind.
    #[error(
        "provider \"{provider}\" does not support route \"{route}\". Add a [provider.routes.{route}] entry mapping to an adapter name"
    )]
    UnsupportedRoute {
        /// Provider name.
        provider: String,
        /// Route kind configuration key.
        route: &'static str,
    },
    /// The configured route references an adapter that is absent at resolution time.
    #[error(
        "provider \"{provider}\": route references adapter \"{adapter}\" which does not exist in [provider.adapters]. Available adapters: {available}"
    )]
    MissingAdapter {
        /// Provider name.
        provider: String,
        /// Missing adapter name.
        adapter: String,
        /// Human-readable list of configured adapters.
        available: String,
    },
}

// ---------------------------------------------------------------------------
// ProviderAdapterTargetConfig
// ---------------------------------------------------------------------------

/// Fully resolved adapter target produced by the provider registry.
///
/// Contains everything the provider adapter layer needs to build an upstream
/// request: which provider, which adapter, which protocol, the raw endpoint
/// URL, authentication style, API key, optional static headers, and both the
/// client-requested and upstream model names.
///
/// `endpoint` is the raw endpoint or URL template from provider TOML. It is
/// **not** a route-built final URL. Provider adapters own endpoint URL shape,
/// including Gemini `{model}` expansion.
#[derive(Clone)]
#[non_exhaustive]
pub struct ProviderAdapterTargetConfig {
    /// Provider name (identifies the provider config).
    pub provider_name: String,
    /// Adapter name within the provider.
    pub adapter_name: String,
    /// Protocol family (e.g. `"openai_chat_completions"`, `"anthropic_messages"`).
    pub protocol: String,
    /// Raw upstream endpoint URL (may contain template placeholders like `{model}`).
    pub endpoint: String,
    /// Authentication header style.
    pub auth_style: AuthStyle,
    /// API key for the upstream provider (wrapped in [`SecretString`]).
    pub api_key: SecretString,
    /// The model name the client originally requested.
    pub requested_model: String,
    /// The model name to send to the upstream provider.
    pub upstream_model: String,
    /// Optional static headers from the adapter config (e.g. `anthropic-version`).
    ///
    /// `Arc`-shared with the source [`crate::provider_config::ProviderAdapterConfig`] so producing a
    /// resolved target per request is a refcount bump, not a `HashMap` clone
    /// (LOW-5). Downstream code that needs to mutate (e.g. defaulting the
    /// `anthropic-version` header) materializes an owned copy.
    pub headers: Arc<HashMap<String, String>>,
}

impl std::fmt::Debug for ProviderAdapterTargetConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAdapterTargetConfig")
            .field("provider_name", &self.provider_name)
            .field("adapter_name", &self.adapter_name)
            .field("protocol", &self.protocol)
            .field("endpoint", &endpoint_without_query(&self.endpoint))
            .field("auth_style", &self.auth_style)
            .field("api_key", &"[REDACTED]")
            .field("requested_model", &self.requested_model)
            .field("upstream_model", &self.upstream_model)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ProviderRegistry
// ---------------------------------------------------------------------------

/// Loads provider TOML files and resolves provider routes to
/// [`ProviderAdapterTargetConfig`]s.
///
/// The registry owns a map of provider configs keyed by provider name.
/// Resolution follows a strict lookup chain:
///
/// ```text
/// provider_name                    -> provider config
/// provider.routes[route_kind]     -> adapter name
/// provider.adapters[adapter_name] -> protocol + endpoint + headers
/// provider.model_aliases[model]   -> upstream_model (if present)
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderConfig>,
}

impl ProviderRegistry {
    /// Load all provider TOML files from a directory.
    ///
    /// Reads every `*.toml` file in `path`, parses each as a [`crate::provider_config::ProviderFile`],
    /// and indexes them by provider name. Returns an error if:
    ///
    /// - the directory cannot be read,
    /// - any TOML file cannot be parsed or fails validation,
    /// - two files declare the same provider name (duplicate).
    ///
    /// Protocol validation is performed separately via [`Self::validate_protocols`]
    /// after loading, typically by passing `ProviderAdapterRegistry::protocol_names()`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ProviderResolution`] for duplicate provider names.
    /// Returns [`CoreError::ConfigLoad`], [`CoreError::ConfigParse`], or
    /// [`CoreError::ConfigValidation`] for I/O, parse, or validation failures
    /// in individual files.
    pub fn load_from_dir(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref();
        let entries = std::fs::read_dir(path).map_err(|source| CoreError::ConfigLoad {
            path: path.to_path_buf(),
            source,
        })?;

        let mut providers: HashMap<String, ProviderConfig> = HashMap::new();
        // Track which file each provider name was first loaded from, so that
        // duplicate-name errors can report *both* file paths for quick diagnosis.
        let mut source_paths: HashMap<String, std::path::PathBuf> = HashMap::new();

        // Collect and sort entries by filename for deterministic ordering across
        // platforms (filesystem order is not guaranteed).
        let mut sorted_entries: Vec<_> =
            entries
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| CoreError::ConfigLoad {
                    path: path.to_path_buf(),
                    source,
                })?;
        sorted_entries.sort_by_key(|e| e.file_name());

        for entry in sorted_entries {
            let file_path = entry.path();

            // Skip symlinks to prevent a malicious actor with write access to
            // the providers directory from tricking the proxy into reading
            // arbitrary files. This check uses symlink_metadata which does not
            // follow symlinks, unlike entry.file_type() which does.
            if file_path.is_symlink() {
                tracing::warn!(
                    path = %file_path.display(),
                    "skipping symlink in provider directory (security: only regular files are loaded)"
                );
                continue;
            }

            // Skip non-regular files (directories, pipes, sockets, etc.).
            if let Ok(ft) = entry.file_type() {
                if !ft.is_file() {
                    tracing::debug!(
                        path = %file_path.display(),
                        "skipping non-regular file in provider directory"
                    );
                    continue;
                }
            }

            if file_path.extension().and_then(|e| e.to_str()) != Some("toml") {
                tracing::debug!(
                    path = %file_path.display(),
                    "skipping non-toml file in provider directory"
                );
                continue;
            }

            let provider = load_provider_config(&file_path, None)?;

            match providers.entry(provider.name.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let first_path = source_paths
                        .get(e.key())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(unknown)".to_owned());
                    return Err(CoreError::ProviderResolution {
                        message: format!(
                            "duplicate provider name \"{}\": already defined in {}, redefined in {}. \
                             Provider names must be unique across all config files",
                            provider.name,
                            first_path,
                            file_path.display()
                        ),
                    });
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    source_paths.insert(provider.name.clone(), file_path.clone());
                    e.insert(provider);
                }
            }
        }

        Ok(Self { providers })
    }

    /// Build a registry from an existing map of provider configs.
    ///
    /// Returns an error if any two configs share the same provider name
    /// (this should not happen if the caller built the map correctly, but
    /// we validate defensively).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ProviderResolution`] if duplicate provider names
    /// are found in the input map.
    pub fn from_providers(
        providers: impl IntoIterator<Item = ProviderConfig>,
    ) -> Result<Self, CoreError> {
        let mut map: HashMap<String, ProviderConfig> = HashMap::new();
        for provider in providers {
            if let Some(existing) = map.get(&provider.name) {
                return Err(CoreError::ProviderResolution {
                    message: format!(
                        "duplicate provider name \"{}\": defined in multiple entries",
                        existing.name
                    ),
                });
            }
            map.insert(provider.name.clone(), provider);
        }
        Ok(Self { providers: map })
    }

    /// Validate that all protocols referenced by all registered providers are
    /// in the known set.
    ///
    /// The `known_protocols` iterable should come from the compiled provider
    /// adapter registry (e.g. `ProviderAdapterRegistry::protocol_names()`),
    /// not an ad hoc string list.
    ///
    /// Accepts any iterable of items that implement `AsRef<str>`, so both
    /// `Vec<String>` and `Vec<&'static str>` (from `protocol_names()`) can be
    /// passed directly without manual conversion.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ProviderResolution`] for the first unknown protocol
    /// found, with an actionable message naming the provider, adapter, and
    /// protocol.
    pub fn validate_protocols(
        &self,
        known_protocols: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<(), CoreError> {
        // Note: This allocates a String per protocol name. The set is tiny
        // (4 built-in protocols) and this runs once at startup, so the cost
        // is negligible. A HashSet<String> is used because the input iterator
        // owns its items and we cannot borrow from them.
        let known: HashSet<String> = known_protocols
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();

        for provider in self.providers.values() {
            for (adapter_name, adapter_cfg) in &provider.adapters {
                if !known.contains(&adapter_cfg.protocol) {
                    return Err(CoreError::ProviderResolution {
                        message: format!(
                            "provider \"{}\": adapter \"{}\" uses unknown protocol \"{}\"",
                            provider.name, adapter_name, adapter_cfg.protocol
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    /// Resolve a provider route by provider name, route kind, and requested model.
    ///
    /// Lookup chain:
    ///
    /// ```text
    /// provider_name                         -> provider config
    /// provider.routes[route_kind]           -> adapter name
    /// provider.adapters[adapter_name]       -> protocol + endpoint + headers
    /// provider.model_aliases[requested_model] -> upstream_model (if present)
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderRouteResolutionError`] if:
    ///
    /// - the provider is not in the registry,
    /// - the route kind is not configured for this provider,
    /// - the adapter referenced by the route is not in the provider's adapter table.
    pub fn resolve_provider_route(
        &self,
        provider_name: &str,
        route_kind: ProviderRouteKind,
        requested_model: &str,
    ) -> Result<ProviderAdapterTargetConfig, ProviderRouteResolutionError> {
        // 1. Look up the provider.
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            ProviderRouteResolutionError::UnknownProvider {
                provider: provider_name.to_owned(),
            }
        })?;

        // 2. Look up the route to find the adapter name.
        let adapter_name = provider.routes.get(route_kind).ok_or_else(|| {
            let route_name = match route_kind {
                ProviderRouteKind::ChatCompletions => "chat_completions",
                ProviderRouteKind::Messages => "messages",
            };
            ProviderRouteResolutionError::UnsupportedRoute {
                provider: provider.name.clone(),
                route: route_name,
            }
        })?;

        // 3. Look up the adapter to get protocol, endpoint, and headers.
        let adapter_cfg = provider.adapters.get(adapter_name).ok_or_else(|| {
            let available = if provider.adapters.is_empty() {
                "(none)".to_owned()
            } else {
                let keys: Vec<&str> = provider.adapters.keys().map(|s| s.as_str()).collect();
                if keys.len() <= MAX_ADAPTERS_IN_ERROR {
                    keys.join(", ")
                } else {
                    format!(
                        "{} (+{} more)",
                        keys[..MAX_ADAPTERS_IN_ERROR].join(", "),
                        keys.len() - MAX_ADAPTERS_IN_ERROR
                    )
                }
            };
            ProviderRouteResolutionError::MissingAdapter {
                provider: provider.name.clone(),
                adapter: adapter_name.to_owned(),
                available,
            }
        })?;

        // 4. Apply provider-local model alias if present.
        let upstream_model = provider
            .model_aliases
            .get(requested_model)
            .map(|s| s.as_str())
            .unwrap_or(requested_model);

        Ok(ProviderAdapterTargetConfig {
            provider_name: provider.name.clone(),
            adapter_name: adapter_name.to_owned(),
            protocol: adapter_cfg.protocol.clone(),
            endpoint: adapter_cfg.endpoint.clone(),
            auth_style: provider.auth_style,
            api_key: provider.api_key.clone(),
            requested_model: requested_model.to_owned(),
            upstream_model: upstream_model.to_owned(),
            headers: Arc::clone(&adapter_cfg.headers),
        })
    }

    /// Returns the number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Returns `true` if no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Returns a reference to the provider config for the given name, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Look up the per-token pricing for a provider's upstream model, if any.
    ///
    /// `upstream_model` must already be alias-resolved (the upstream model id),
    /// since pricing keys are upstream ids. Returns `None` when the provider is
    /// unknown or has no pricing configured for the model. Callers that need the
    /// price across an `.await` should `.cloned()` the result (cheap: five
    /// `Decimal`s).
    #[must_use]
    pub fn pricing_for(&self, provider: &str, upstream_model: &str) -> Option<&ModelPricing> {
        self.providers
            .get(provider)?
            .pricing
            .get(&ModelId::new(upstream_model))
    }

    /// Returns an iterator over all registered provider configs.
    pub fn iter(&self) -> impl Iterator<Item = &ProviderConfig> {
        self.providers.values()
    }

    /// Returns the static catalog entries for a provider, if a catalog is configured.
    ///
    /// Returns `None` if the provider is not found. Returns `Some(&[])` if the
    /// provider exists but has no catalog configured (or an empty static list).
    #[must_use]
    pub fn catalog_models(&self, provider_name: &str) -> Option<&[StaticModelCatalogEntry]> {
        self.providers.get(provider_name).map(|p| match &p.catalog {
            Some(catalog) => catalog.models.as_slice(),
            None => &[],
        })
    }
}

impl<'a> IntoIterator for &'a ProviderRegistry {
    type Item = &'a ProviderConfig;
    type IntoIter = std::collections::hash_map::Values<'a, String, ProviderConfig>;

    fn into_iter(self) -> Self::IntoIter {
        self.providers.values()
    }
}

/// Maximum number of adapter names shown in error messages before truncation.
const MAX_ADAPTERS_IN_ERROR: usize = 5;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderAdapterConfig, ProviderCatalogConfig, ProviderRoutesConfig};

    #[test]
    fn provider_route_resolves_alias_and_redacts_secrets() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("api-secret"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1/chat/completions?key=query-secret".to_owned(),
                    headers: Arc::new(HashMap::from([(
                        "x-secret".to_owned(),
                        "header-secret".to_owned(),
                    )])),
                },
            )]),
            routes: ProviderRoutesConfig {
                chat_completions: Some("chat".to_owned()),
                messages: None,
            },
            model_aliases: HashMap::from([("short".to_owned(), "upstream".to_owned())]),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let target = registry
            .resolve_provider_route("example", ProviderRouteKind::ChatCompletions, "short")
            .expect("target");
        assert_eq!(target.requested_model, "short");
        assert_eq!(target.upstream_model, "upstream");
        let debug = format!("{target:?}");
        assert!(!debug.contains("api-secret"));
        assert!(!debug.contains("header-secret"));
        assert!(!debug.contains("query-secret"));
    }

    #[test]
    fn pricing_for_resolves_under_upstream_model_id() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1/chat/completions".to_owned(),
                    headers: Arc::new(HashMap::new()),
                },
            )]),
            routes: ProviderRoutesConfig {
                chat_completions: Some("chat".to_owned()),
                messages: None,
            },
            model_aliases: HashMap::from([(
                "alias-name".to_owned(),
                "real-upstream-model".to_owned(),
            )]),
            discovery: None,
            catalog: None,
            pricing: HashMap::from([(
                ModelId::new("real-upstream-model"),
                ModelPricing {
                    input: "0.001".parse().unwrap(),
                    ..ModelPricing::default()
                },
            )]),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        // Pricing is keyed by the UPSTREAM (alias-resolved) model id.
        let p = registry
            .pricing_for("example", "real-upstream-model")
            .expect("pricing present");
        assert_eq!(p.input.to_string(), "0.001");
        // The client-facing alias is NOT a pricing key.
        assert!(registry.pricing_for("example", "alias-name").is_none());
        // Unknown provider / model -> None.
        assert!(
            registry
                .pricing_for("other", "real-upstream-model")
                .is_none()
        );
        assert!(registry.pricing_for("example", "unpriced-model").is_none());
    }

    #[test]
    fn provider_route_returns_typed_unknown_provider_error() {
        let registry = ProviderRegistry::from_providers(Vec::new()).expect("registry");

        let error = registry
            .resolve_provider_route("missing", ProviderRouteKind::Messages, "model")
            .expect_err("unknown provider");

        assert!(matches!(
            error,
            ProviderRouteResolutionError::UnknownProvider { provider }
                if provider == "missing"
        ));
    }

    #[test]
    fn provider_route_returns_typed_unsupported_route_error() {
        let provider = ProviderConfig {
            name: "example".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers([provider]).expect("registry");

        let error = registry
            .resolve_provider_route("example", ProviderRouteKind::ChatCompletions, "model")
            .expect_err("unsupported route");

        assert!(matches!(
            error,
            ProviderRouteResolutionError::UnsupportedRoute {
                provider,
                route: "chat_completions"
            } if provider == "example"
        ));
    }

    // -- load_from_dir tests ---------------------------------------------------

    /// Helper: write a minimal valid provider TOML file into `dir`.
    fn write_provider(dir: &std::path::Path, filename: &str, name: &str) {
        let content = format!(
            r#"
[provider]
name = "{name}"
api_key = "test-key"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "https://example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#
        );
        std::fs::write(dir.join(filename), content).expect("write provider toml");
    }

    #[test]
    fn load_from_dir_loads_valid_providers() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_provider(dir.path(), "alpha.toml", "alpha");
        write_provider(dir.path(), "beta.toml", "beta");

        let registry = ProviderRegistry::load_from_dir(dir.path()).expect("load_from_dir");
        assert_eq!(registry.len(), 2);
        assert!(registry.get("alpha").is_some());
        assert!(registry.get("beta").is_some());
    }

    #[test]
    fn load_from_dir_rejects_duplicate_provider_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_provider(dir.path(), "a.toml", "dup");
        write_provider(dir.path(), "b.toml", "dup");

        let err = ProviderRegistry::load_from_dir(dir.path()).expect_err("duplicate");
        assert!(
            matches!(err, CoreError::ProviderResolution { .. }),
            "expected ProviderResolution error for duplicates, got: {err:?}"
        );
        assert!(err.to_string().contains("duplicate provider name"));
    }

    #[test]
    fn load_from_dir_skips_non_toml_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_provider(dir.path(), "valid.toml", "valid");
        std::fs::write(dir.path().join("readme.md"), "not a config").expect("write readme");

        let registry = ProviderRegistry::load_from_dir(dir.path()).expect("load_from_dir");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("valid").is_some());
    }

    #[test]
    fn load_from_dir_handles_empty_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let registry = ProviderRegistry::load_from_dir(dir.path()).expect("load_from_dir");
        assert!(registry.is_empty());
    }

    #[test]
    fn load_from_dir_skips_symlinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_provider(dir.path(), "real.toml", "real");

        // Create a symlink to the real file.
        #[cfg(unix)]
        {
            let link = dir.path().join("linked.toml");
            std::os::unix::fs::symlink(dir.path().join("real.toml"), &link)
                .expect("create symlink");
        }
        // On non-Unix, skip symlink creation (test still passes; just fewer entries).
        let registry = ProviderRegistry::load_from_dir(dir.path()).expect("load_from_dir");
        // Should only have "real" -- the symlink must be skipped.
        assert!(registry.get("real").is_some());
        assert_eq!(registry.len(), 1, "symlink should be skipped");
    }

    // -- MissingAdapter test ---------------------------------------------------

    #[test]
    fn resolve_provider_route_returns_missing_adapter_error() {
        // Build a provider with a route that points to a non-existent adapter.
        let provider = ProviderConfig {
            name: "broken".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(), // no adapters defined
            routes: ProviderRoutesConfig {
                chat_completions: Some("nonexistent_adapter".to_owned()),
                messages: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");

        let error = registry
            .resolve_provider_route("broken", ProviderRouteKind::ChatCompletions, "model")
            .expect_err("missing adapter");

        assert!(matches!(
            error,
            ProviderRouteResolutionError::MissingAdapter { provider, adapter, .. }
                if provider == "broken" && adapter == "nonexistent_adapter"
        ));
    }

    // -- from_providers duplicate detection -----------------------------------

    #[test]
    fn from_providers_rejects_duplicate_names() {
        let provider_a = ProviderConfig {
            name: "dup".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let provider_b = ProviderConfig {
            name: "dup".to_owned(),
            api_key: SecretString::from("key2"),
            ..provider_a.clone()
        };
        let result = ProviderRegistry::from_providers(vec![provider_a, provider_b]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, CoreError::ProviderResolution { .. }));
        assert!(err.to_string().contains("duplicate provider name"));
    }

    // -- len() / is_empty() / get() -------------------------------------------

    #[test]
    fn empty_registry_is_empty_and_zero_len() {
        let registry = ProviderRegistry::from_providers(Vec::new()).expect("registry");
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get("anything").is_none());
    }

    #[test]
    fn registry_with_one_provider() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
        assert!(registry.get("test").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    // -- catalog_models() -----------------------------------------------------

    #[test]
    fn catalog_models_returns_none_for_unknown_provider() {
        let registry = ProviderRegistry::from_providers(Vec::new()).expect("registry");
        assert!(registry.catalog_models("missing").is_none());
    }

    #[test]
    fn catalog_models_returns_empty_for_provider_without_catalog() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let models = registry.catalog_models("test").expect("provider exists");
        assert!(models.is_empty());
    }

    #[test]
    fn catalog_models_returns_entries_when_configured() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: Some(ProviderCatalogConfig::default()),
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let models = registry.catalog_models("test").expect("provider exists");
        // Default catalog has no models.
        assert!(models.is_empty());
    }

    // -- resolve without alias (fallback) -------------------------------------

    #[test]
    fn resolve_without_alias_returns_requested_model() {
        let provider = ProviderConfig {
            name: "test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::from([(
                "chat".to_owned(),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: "https://example.com/v1/chat/completions".to_owned(),
                    headers: Arc::new(HashMap::new()),
                },
            )]),
            routes: ProviderRoutesConfig {
                chat_completions: Some("chat".to_owned()),
                messages: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let target = registry
            .resolve_provider_route("test", ProviderRouteKind::ChatCompletions, "gpt-4o")
            .expect("resolve");
        assert_eq!(target.upstream_model, "gpt-4o");
        assert_eq!(target.requested_model, "gpt-4o");
    }

    // -- IntoIterator ---------------------------------------------------------

    #[test]
    fn into_iterator_works() {
        let provider_a = ProviderConfig {
            name: "a".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters: HashMap::new(),
            routes: ProviderRoutesConfig::default(),
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let provider_b = ProviderConfig {
            name: "b".to_owned(),
            ..provider_a.clone()
        };
        let registry =
            ProviderRegistry::from_providers(vec![provider_a, provider_b]).expect("registry");
        let names: Vec<&str> = (&registry).into_iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    // -- Adapter list truncation in MissingAdapter error ----------------------

    #[test]
    fn missing_adapter_error_truncates_long_adapter_list() {
        // Build a provider with >5 adapters and a route pointing to a non-existent one.
        let mut adapters = HashMap::new();
        for i in 0..7 {
            adapters.insert(
                format!("adapter-{i}"),
                ProviderAdapterConfig {
                    protocol: "openai_chat_completions".to_owned(),
                    endpoint: format!("https://example.com/v{i}"),
                    headers: Arc::new(HashMap::new()),
                },
            );
        }
        let provider = ProviderConfig {
            name: "truncation-test".to_owned(),
            api_key: SecretString::from("key"),
            auth_style: AuthStyle::Bearer,
            passthrough_auth: false,
            egress: None,
            adapters,
            routes: ProviderRoutesConfig {
                chat_completions: Some("nonexistent".to_owned()),
                messages: None,
            },
            model_aliases: HashMap::new(),
            discovery: None,
            catalog: None,
            pricing: Default::default(),
        };
        let registry = ProviderRegistry::from_providers(vec![provider]).expect("registry");
        let error = registry
            .resolve_provider_route(
                "truncation-test",
                ProviderRouteKind::ChatCompletions,
                "model",
            )
            .expect_err("missing adapter");

        let msg = error.to_string();
        assert!(
            msg.contains("+2 more"),
            "expected truncation message, got: {msg}"
        );
    }
}
