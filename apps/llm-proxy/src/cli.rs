//! CLI argument definitions using clap.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// LLM proxy server.
#[derive(Parser, Debug)]
#[command(
    name = "llm-proxy",
    version,
    about = "LLM proxy server with provider-scoped routing",
    propagate_version = true
)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Commands,
}

/// Available CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the proxy server.
    Serve {
        /// Path to config file (must be TOML).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Override listen port.
        #[arg(short, long)]
        port: Option<u16>,
        /// Run as background daemon.
        #[arg(short, long)]
        background: bool,
        /// Internal: used by daemon child process.
        #[arg(long, hide = true)]
        daemonize: bool,
    },
    /// Stop a running server.
    Stop,
    /// Signal a running server to re-read its provider files (hot reload;
    /// needs server.hot_reload = true, unix only).
    Reload,
    /// Check if the server is running.
    Status,
    /// Create a default config file.
    Init,
    /// Validate a config file and print settings.
    Validate {
        /// Path to config file (must be TOML).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Live-check every configured provider: reachability, and with --probe
    /// a one-token request (which can cost money). Each failure names its fix.
    Doctor {
        /// Path to config file (must be TOML).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Also send one real one-token request per provider. Off by default:
        /// probes spend upstream quota.
        #[arg(long)]
        probe: bool,
    },
    /// List provider-scoped models.
    Models {
        /// Path to config file (must be TOML).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Filter output to a single provider.
        #[arg(long)]
        provider: Option<String>,
        /// Fetch models from the configured provider discovery endpoint.
        #[arg(long)]
        live: bool,
        /// Persist a successful live result under providers/.catalog/.
        #[arg(long, requires = "live")]
        write_catalog: bool,
        /// Return an error instead of falling back when live discovery fails.
        #[arg(long, requires = "live")]
        require_success: bool,
    },
    /// Sync model pricing from the models.dev catalogue.
    PricingSync {
        /// Path to config file (must be TOML).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Only sync this provider (its configured name, not the models.dev id).
        #[arg(long)]
        provider: Option<String>,
        /// Catalogue URL. Defaults to https://models.dev/api.json.
        #[arg(long)]
        url: Option<String>,
        /// Show what would be written without touching any file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage auto-start on login.
    Autostart {
        /// Autostart subcommand.
        #[command(subcommand)]
        action: AutostartAction,
    },
}

/// Autostart subcommand actions.
#[derive(Subcommand, Debug)]
pub enum AutostartAction {
    /// Enable auto-start.
    Enable {
        /// Path to config file.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Port to listen on.
        #[arg(short, long)]
        port: Option<u16>,
        /// Overwrite an existing launchd plist / .desktop entry instead of
        /// aborting.
        #[arg(long)]
        force: bool,
    },
    /// Disable auto-start.
    Disable,
    /// Show auto-start status.
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_flags_parse_together() {
        let cli = Cli::try_parse_from([
            "llm-proxy",
            "models",
            "--provider",
            "fireworks",
            "--live",
            "--write-catalog",
            "--require-success",
        ])
        .unwrap();
        let Commands::Models {
            provider,
            live,
            write_catalog,
            require_success,
            ..
        } = cli.command
        else {
            panic!("expected models command");
        };
        assert_eq!(provider.as_deref(), Some("fireworks"));
        assert!(live);
        assert!(write_catalog);
        assert!(require_success);
    }
}
