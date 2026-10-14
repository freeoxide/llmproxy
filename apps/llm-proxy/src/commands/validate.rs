use std::path::Path;

use anyhow::{Context, Result, bail};
use llm_proxy_core::{AppConfig, ProviderRegistry, load_app_config};
use llm_proxy_provider::ProviderAdapterRegistry;

use crate::config_validation::validate_toml_extension;
use crate::paths::{providers_dir_for, resolve_config};
use crate::state::catalog_enforcement_warning;

/// Run the `validate` command.
///
/// Loads main TOML, loads provider files, validates provider adapter protocols
/// against builtins, and prints provider route table.
pub fn cmd_validate(config_path: Option<&Path>) -> Result<()> {
    let path = resolve_config(config_path);

    validate_toml_extension(&path)?;

    println!("validating config: {}", path.display());

    let app_config: AppConfig = load_app_config(&path)
        .with_context(|| format!("loading TOML config from {}", path.display()))?;

    println!();
    println!("=== Server Configuration ===");
    println!("  bind:            {}", app_config.server.bind);
    println!(
        "  request_timeout: {}s",
        app_config.server.request_timeout.as_secs()
    );
    println!("  log_level:       {}", app_config.server.log_level);
    println!("  hot_reload:      {}", app_config.server.hot_reload);
    println!("  rate_limit_rpm:  {}", app_config.server.rate_limit_rpm);
    println!(
        "  trust_forwarded_headers: {}",
        app_config.server.trust_forwarded_headers
    );
    println!(
        "  dedup_window:    {}ms",
        app_config.server.dedup_window.as_millis()
    );
    println!("  server_name:     {}", app_config.server.server_name);

    let providers_dir = providers_dir_for(&path);
    let registry = if providers_dir.exists() {
        ProviderRegistry::load_from_dir(&providers_dir)
            .with_context(|| format!("loading providers from {}", providers_dir.display()))?
    } else {
        ProviderRegistry::from_providers(vec![])
            .with_context(|| "building empty provider registry")?
    };

    let adapter_registry = ProviderAdapterRegistry::builtin();
    let known_protocols = adapter_registry.protocol_names();
    registry
        .validate_protocols(known_protocols)
        .with_context(|| "validating provider protocols")?;

    println!();
    println!("=== Providers ({} loaded) ===", registry.len());
    if registry.is_empty() {
        println!("  (none)");
    } else {
        for provider in registry.iter() {
            let route_count = provider
                .routes
                .chat_completions
                .iter()
                .chain(provider.routes.messages.iter())
                .count();
            println!(
                "  {} ({} adapters, {} routes, {} aliases)",
                provider.name,
                provider.adapters.len(),
                route_count,
                provider.model_aliases.len()
            );
        }
    }

    println!();
    println!("=== Provider Routes ===");
    if registry.is_empty() {
        println!("  (no providers loaded)");
    } else {
        let route_errors: Vec<String> = registry.iter().flat_map(print_provider_routes).collect();

        if !route_errors.is_empty() {
            bail!(
                "config has {} route resolution error(s)",
                route_errors.len()
            );
        }
    }

    let cache_dir = providers_dir.join(".catalog");
    let warnings: Vec<_> = registry
        .iter()
        .filter_map(|provider| catalog_enforcement_warning(provider, &cache_dir))
        .collect();
    if !warnings.is_empty() {
        println!();
        println!("=== Warnings ===");
        for warning in warnings {
            println!("  WARNING: {warning}");
        }
    }

    println!();
    println!("Config is valid!");
    Ok(())
}

fn print_provider_routes(provider: &llm_proxy_core::ProviderConfig) -> Vec<String> {
    println!("{}:", provider.name);

    let route_entries: Vec<(&str, &String)> = provider
        .routes
        .chat_completions
        .as_ref()
        .map(|a| ("chat_completions", a))
        .into_iter()
        .chain(provider.routes.messages.as_ref().map(|a| ("messages", a)))
        .collect();

    if route_entries.is_empty() {
        println!("  (no routes configured)");
        return Vec::new();
    }

    let mut errors = Vec::new();
    for (route_kind, adapter_name) in route_entries {
        if let Some(adapter) = provider.adapters.get(adapter_name) {
            println!("  {route_kind} -> {adapter_name}/{}", adapter.protocol);
        } else {
            let msg = format!("  {route_kind} -> ERROR: unknown adapter \"{adapter_name}\"");
            println!("{msg}");
            errors.push(msg);
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults::DEFAULT_CONFIG_TOML;

    #[test]
    fn cmd_validate_valid_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, DEFAULT_CONFIG_TOML).unwrap();

        let result = cmd_validate(Some(&config));
        result.unwrap();
    }

    #[test]
    fn cmd_validate_invalid_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "not valid toml {{{{").unwrap();

        let result = cmd_validate(Some(&config));
        assert!(result.is_err());
    }
}
