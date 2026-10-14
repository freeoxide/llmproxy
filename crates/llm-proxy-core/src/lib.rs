//! Core types shared across all `llm-proxy` crates: configuration, routing,
//! the provider registry, metrics, token counting, PID management, and
//! errors.

#![deny(missing_docs)]

/// Provider model catalog merge, filter, and cache-file types.
pub mod catalog;
/// Shared `${ENV_VAR}` interpolation for config files.
pub mod env_interpolate;
/// Crate-level error type.
pub mod error;
/// Runtime metrics (counters, latency ring-buffer, per-provider-model counts).
pub mod metrics;
/// PID file management for daemon mode.
pub mod pid;
/// TOML provider configuration types, parsing, and validation.
pub mod provider_config;
/// Provider registry: load provider TOML files and resolve route targets.
pub mod provider_registry;
/// Token counting utilities.
pub mod token;

/// Shared test utilities (env-var guards, crate-level test mutex).
#[cfg(test)]
pub mod test_support;

pub use catalog::{
    CatalogFile, CatalogFileMetadata, merge_catalog, model_allowed, parse_catalog_file,
};
pub use error::CoreError;
pub use metrics::{Metrics, Snapshot};
pub use pid::{PidError, PidManager};
pub use provider_config::{
    ANONYMOUS_CALLER, AdminAuthConfig, AdminMountPosture, AppConfig, AuthConfig, AuthMode,
    AuthStyle, ConfigValidationError, LogFormat, ModelId, ModelPricing, ProviderAdapterConfig,
    ProviderCatalogConfig, ProviderCatalogMode, ProviderConfig, ProviderDiscoveryConfig,
    ProviderDiscoveryKind, ProviderEgressConfig, ProviderFile, ProviderRouteKind,
    ProviderRoutesConfig, ServerConfig, StaticModelCatalogEntry, StorageConfig, load_app_config,
    load_provider_config, validate_provider_config,
};
pub use provider_registry::{
    ProviderAdapterTargetConfig, ProviderRegistry, ProviderRouteResolutionError,
};
pub use token::{Counter, MessageContent};
