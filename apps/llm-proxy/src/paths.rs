//! Path resolution helpers: config directories, file paths, provider
//! directories, and platform-specific autostart paths.

use std::path::{Path, PathBuf};

/// Config directory: `~/.config/llm-proxy/`.
///
/// Falls back to a path relative to the current working directory (with a
/// stderr warning) when `$HOME` and `$USERPROFILE` are both unset.
pub fn config_dir() -> PathBuf {
    let home = home_env::home_dir();
    if home.is_none() {
        eprintln!(
            "warning: HOME and USERPROFILE are both unset; using relative \
             config path (depends on current working directory)"
        );
    }
    home.unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("llm-proxy")
}

/// Path bundle for the `stop`/`status` commands: the PID file manager plus the
/// config file `status` reads for the `listen` address.
#[derive(Debug, Clone)]
pub struct CommandPaths {
    pid_manager: llm_proxy_core::PidManager,
    config_path: PathBuf,
}

impl CommandPaths {
    /// Build a [`CommandPaths`] from an explicit PID manager and config path.
    #[must_use]
    pub fn new(pid_manager: llm_proxy_core::PidManager, config_path: PathBuf) -> Self {
        Self {
            pid_manager,
            config_path,
        }
    }

    /// The PID file manager backing `stop`/`status`.
    #[must_use]
    pub fn pid_manager(&self) -> &llm_proxy_core::PidManager {
        &self.pid_manager
    }

    /// The config file `status` reads for the `listen` address.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }
}

/// Build [`CommandPaths`] for the default config directory and config path.
#[must_use]
pub fn default_command_paths() -> CommandPaths {
    CommandPaths::new(
        llm_proxy_core::PidManager::new(config_dir()),
        default_config_path(),
    )
}

/// Config file path: `~/.config/llm-proxy/config.toml`.
pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// PID file path: `~/.config/llm-proxy/llm-proxy.pid`.
pub fn pid_file_path() -> PathBuf {
    config_dir().join("llm-proxy.pid")
}

/// Resolve the config file path: explicit `--config`, then `$LLM_PROXY_CONFIG`,
/// then `~/.config/llm-proxy/config.toml`.
pub fn resolve_config(cli_path: Option<&Path>) -> PathBuf {
    if let Some(p) = cli_path {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("LLM_PROXY_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    default_config_path()
}

/// Returns the providers directory path: always `providers/` next to the
/// config file, or `./providers` when the config path has no parent directory.
pub fn providers_dir_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("providers")
}

/// Returns the launchd plist path on macOS.
///
/// Falls back to `./com.llm-proxy.plist` if home directory cannot be determined.
#[cfg(target_os = "macos")]
pub fn launchd_plist_path() -> PathBuf {
    home_env::home_dir()
        .map(|h| {
            h.join("Library")
                .join("LaunchAgents")
                .join("com.llm-proxy.plist")
        })
        .unwrap_or_else(|| PathBuf::from("com.llm-proxy.plist"))
}

/// Returns the XDG autostart directory on Linux.
///
/// Falls back to `./.config/autostart` if home directory cannot be determined.
#[cfg(target_os = "linux")]
pub fn linux_autostart_dir() -> PathBuf {
    home_env::home_dir()
        .map(|h| h.join(".config").join("autostart"))
        .unwrap_or_else(|| PathBuf::from(".config/autostart"))
}

pub(crate) mod home_env {
    use std::path::PathBuf;

    pub(crate) fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_uses_explicit_path() {
        let path = Path::new("/tmp/my-config.toml");
        let result = resolve_config(Some(path));
        assert_eq!(result, PathBuf::from("/tmp/my-config.toml"));
    }

    #[test]
    fn resolve_config_uses_default_when_no_args() {
        let result = resolve_config(None);
        assert!(result.to_string_lossy().ends_with("config.toml"));
    }
}
