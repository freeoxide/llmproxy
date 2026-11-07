//! `llm-proxy` binary -- thin CLI dispatch.

use anyhow::Result;
use clap::Parser;
use llm_proxy_app::cli::{AutostartAction, Cli, Commands};
use llm_proxy_app::commands::*;
use llm_proxy_app::paths::default_command_paths;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve {
            config,
            port,
            background,
            daemonize,
        } => cmd_serve(config, port, background, daemonize).await,
        Commands::Stop => {
            let paths = default_command_paths();
            cmd_stop(&paths)
        }
        Commands::Status => {
            let paths = default_command_paths();
            cmd_status(&paths)
        }
        Commands::Init => cmd_init(),
        Commands::Validate { config } => cmd_validate(config.as_deref()),
        Commands::Doctor { config, probe } => cmd_doctor(config.as_deref(), probe).await,
        Commands::Models {
            config,
            provider,
            live,
            write_catalog,
            require_success,
        } => cmd_models(config, provider, live, write_catalog, require_success).await,
        Commands::PricingSync {
            config,
            provider,
            url,
            dry_run,
        } => cmd_pricing_sync(config, provider, url, dry_run).await,
        Commands::Autostart { action } => match action {
            AutostartAction::Enable {
                config,
                port,
                force,
            } => cmd_autostart_enable(config.as_deref(), port, force),
            AutostartAction::Disable => cmd_autostart_disable(),
            AutostartAction::Status => cmd_autostart_status(),
        },
    }
}
