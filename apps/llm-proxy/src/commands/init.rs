use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::defaults::{
    DEFAULT_CONFIG_TOML, DEFAULT_PROVIDER_OPENCODE_GO, DEFAULT_PROVIDER_OPENCODE_ZEN,
};
use crate::paths::{config_dir, providers_dir_for};
use crate::permissions::{create_private_file, set_private_dir_permissions};

/// Run the `init` command against the default config directory.
pub fn cmd_init() -> Result<()> {
    cmd_init_at(&config_dir())
}

pub(super) fn cmd_init_at(base_dir: &Path) -> Result<()> {
    let config_path: PathBuf = base_dir.join("config.toml");
    if config_path.exists() {
        bail!("config file already exists at {}", config_path.display());
    }

    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("creating directory {}", base_dir.display()))?;
    set_private_dir_permissions(base_dir)?;

    create_private_file(&config_path, DEFAULT_CONFIG_TOML.as_bytes())?;
    println!("created config at {}", config_path.display());

    let providers_dir = providers_dir_for(&config_path);
    std::fs::create_dir_all(&providers_dir)
        .with_context(|| format!("creating directory {}", providers_dir.display()))?;

    let go_path = providers_dir.join("opencode-go.toml");
    create_private_file(&go_path, DEFAULT_PROVIDER_OPENCODE_GO.as_bytes())?;
    println!("created provider config at {}", go_path.display());

    let zen_path = providers_dir.join("opencode-zen.toml");
    create_private_file(&zen_path, DEFAULT_PROVIDER_OPENCODE_ZEN.as_bytes())?;
    println!("created provider config at {}", zen_path.display());

    println!();
    println!("Next steps:");
    println!("  1. Set API keys via environment variables:");
    println!("     export LLM_PROXY_OPENCODE_GO_KEY=your-go-key");
    println!("     export LLM_PROXY_OPENCODE_ZEN_KEY=your-zen-key");
    println!("  2. Run `llm-proxy validate` to check your config");
    println!("  3. Run `llm-proxy serve` to start the proxy");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cmd_init_at;

    #[test]
    fn cmd_init_creates_files_with_permissions() {
        let dir = tempfile::tempdir().unwrap();
        cmd_init_at(dir.path()).expect("cmd_init_at should succeed on an empty base dir");

        let config_path = dir.path().join("config.toml");
        let providers_dir = dir.path().join("providers");
        let go_path = providers_dir.join("opencode-go.toml");
        let zen_path = providers_dir.join("opencode-zen.toml");

        assert!(config_path.exists(), "config.toml was created");
        assert!(go_path.exists(), "opencode-go.toml was created");
        assert!(zen_path.exists(), "opencode-zen.toml was created");

        let main_content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: toml::Value =
            toml::from_str(&main_content).expect("generated config.toml must be valid TOML");
        assert!(
            parsed.get("server").is_some(),
            "generated config.toml must have a [server] section"
        );

        let go_content = std::fs::read_to_string(&go_path).unwrap();
        let parsed: toml::Value = toml::from_str(&go_content).unwrap();
        assert!(parsed.get("provider").is_some());
        assert_eq!(parsed["provider"]["name"].as_str(), Some("opencode-go"));

        let zen_content = std::fs::read_to_string(&zen_path).unwrap();
        let parsed: toml::Value = toml::from_str(&zen_content).unwrap();
        assert_eq!(parsed["provider"]["name"].as_str(), Some("opencode-zen"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = config_path.metadata().unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "config.toml should be owner-only (0600)"
            );
            let mode = go_path.metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let mode = zen_path.metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn cmd_init_rejects_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&config_path, "existing").unwrap();

        let result = cmd_init_at(dir.path());
        let err = result.expect_err("cmd_init_at must error when config.toml already exists");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already exists"),
            "error should mention the config already exists, got: {msg}"
        );

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "existing",
            "cmd_init_at must not overwrite an existing config"
        );

        let go_path = dir.path().join("providers").join("opencode-go.toml");
        assert!(
            !go_path.exists(),
            "no provider files should be written on bail"
        );
    }

    #[test]
    fn cmd_init_is_idempotent_reject() {
        let dir = tempfile::tempdir().unwrap();
        cmd_init_at(dir.path()).expect("first init should succeed");
        let second = cmd_init_at(dir.path());
        assert!(
            second.is_err(),
            "a second init against a populated dir must bail, not clobber"
        );
    }
}
