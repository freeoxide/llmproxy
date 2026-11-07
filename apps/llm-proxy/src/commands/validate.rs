use std::path::Path;

use anyhow::{Context, Result, bail};
use llm_proxy_adapters::ProviderAdapterRegistry;
use llm_proxy_core::{AppConfig, load_app_config};

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
    println!("  server_name:     {}", app_config.server.server_name);

    let providers_dir = providers_dir_for(&path);
    // Every gate startup runs (protocols, self-loop, bare /v1 targets, the
    // auth/passthrough combination) runs here too, on the same shared path.
    let adapter_registry = ProviderAdapterRegistry::builtin();
    let registry = crate::state::load_registry_with_checks(&app_config, &path, &adapter_registry)?;

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

    #[test]
    fn cmd_validate_accepts_the_shipped_example_config() {
        let example =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.toml.example");
        let raw = std::fs::read_to_string(&example)
            .unwrap()
            // The example references env vars only inside commented-out
            // lines, but config loading scans the raw text for them.
            .replace("${HOME}", "/tmp")
            .replace("${ADMIN_TOKEN}", "test-admin-token");
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, raw).unwrap();

        cmd_validate(Some(&config))
            .unwrap_or_else(|e| panic!("config.toml.example must validate as shipped: {e:#}"));
    }

    #[test]
    fn cmd_validate_rejects_an_upstream_that_resolves_to_the_own_listener() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("providers")).unwrap();
        std::fs::write(
            dir.path().join("providers/loopy.toml"),
            r#"[provider]
name = "loopy"
api_key = "k"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "http://localhost:3456/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#,
        )
        .unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"
"#,
        )
        .unwrap();

        let err = cmd_validate(Some(&config))
            .expect_err("an upstream naming this proxy's own listener is invalid");
        let message = format!("{err:#}");
        assert!(
            message.contains("loopy") && message.contains("3456"),
            "the error names the provider and the listener: {message}"
        );
        assert!(
            message.contains("forever"),
            "the error says what would happen: {message}"
        );
    }

    #[test]
    fn cmd_validate_rejects_cleartext_credentials_to_a_remote_host() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("providers")).unwrap();
        std::fs::write(
            dir.path().join("providers/leaky.toml"),
            r#"[provider]
name = "leaky"
api_key = "k"
auth_style = "bearer"

[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "http://api.example.com/v1/chat/completions"

[provider.routes]
chat_completions = "chat"
"#,
        )
        .unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"
"#,
        )
        .unwrap();

        let result = cmd_validate(Some(&config));
        let err = result.expect_err("cleartext credentials to a remote host are invalid");
        let message = format!("{err:#}");
        assert!(
            message.contains("leaky") && message.contains("api.example.com"),
            "the validate error names the provider and the host: {message}"
        );
        assert!(
            message.contains("cleartext"),
            "the validate error says what is wrong: {message}"
        );
    }

    #[test]
    fn cmd_validate_rejects_a_bare_v1_target_naming_no_provider_or_pool() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[server]
bind = "127.0.0.1:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "t"

[server.bare_v1]
enabled = true
[server.bare_v1.models]
"claude-sonnet-4-6" = "ghost-pool"
"#,
        )
        .unwrap();

        let result = cmd_validate(Some(&config));
        let err = result.expect_err("a bare /v1 model mapping to an unknown target is invalid");
        let message = format!("{err:#}");
        assert!(
            message.contains("bare_v1") && message.contains("ghost-pool"),
            "the validate error names the section and the unknown target: {message}"
        );
    }
}
