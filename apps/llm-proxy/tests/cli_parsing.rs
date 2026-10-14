//! Integration tests for CLI argument parsing.
//!
//! Exercises `Cli::try_parse_from` to verify that clap argument definitions
//! accept and reject the expected inputs.

#[cfg(test)]
mod tests {

    use clap::Parser;
    use llm_proxy_app::cli::{AutostartAction, Cli, Commands};

    #[test]
    fn parse_serve_with_config() {
        let cli =
            Cli::try_parse_from(["llm-proxy", "serve", "--config", "/tmp/test.toml"]).unwrap();
        let Commands::Serve { config, .. } = cli.command else {
            panic!("expected serve")
        };
        assert_eq!(config.unwrap().to_str(), Some("/tmp/test.toml"));
    }

    #[test]
    fn parse_serve_with_port() {
        let cli = Cli::try_parse_from(["llm-proxy", "serve", "-p", "9090"]).unwrap();
        let Commands::Serve { port, .. } = cli.command else {
            panic!("expected serve")
        };
        assert_eq!(port, Some(9090));
    }

    #[test]
    fn parse_serve_background_flag() {
        let cli = Cli::try_parse_from(["llm-proxy", "serve", "--background"]).unwrap();
        let Commands::Serve { background, .. } = cli.command else {
            panic!("expected serve")
        };
        assert!(background);
    }

    #[test]
    fn parse_serve_daemonize_hidden_flag() {
        let cli = Cli::try_parse_from(["llm-proxy", "serve", "--daemonize"]).unwrap();
        let Commands::Serve { daemonize, .. } = cli.command else {
            panic!("expected serve")
        };
        assert!(daemonize);
    }

    #[test]
    fn parse_stop() {
        let cli = Cli::try_parse_from(["llm-proxy", "stop"]).unwrap();
        assert!(matches!(cli.command, Commands::Stop));
    }

    #[test]
    fn parse_status() {
        let cli = Cli::try_parse_from(["llm-proxy", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn parse_init() {
        let cli = Cli::try_parse_from(["llm-proxy", "init"]).unwrap();
        assert!(matches!(cli.command, Commands::Init));
    }

    #[test]
    fn parse_validate_with_config() {
        let cli = Cli::try_parse_from(["llm-proxy", "validate", "-c", "x.toml"]).unwrap();
        let Commands::Validate { config } = cli.command else {
            panic!("expected validate")
        };
        assert_eq!(config.unwrap().to_str(), Some("x.toml"));
    }

    #[test]
    fn parse_models_with_all_flags() {
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
            panic!("expected models")
        };
        assert_eq!(provider.as_deref(), Some("fireworks"));
        assert!(live);
        assert!(write_catalog);
        assert!(require_success);
    }

    #[test]
    fn parse_models_write_catalog_requires_live() {
        let result = Cli::try_parse_from(["llm-proxy", "models", "--write-catalog"]);
        result.unwrap_err();
    }

    #[test]
    fn parse_models_require_success_requires_live() {
        let result = Cli::try_parse_from(["llm-proxy", "models", "--require-success"]);
        result.unwrap_err();
    }

    #[test]
    fn parse_autostart_enable() {
        let cli = Cli::try_parse_from([
            "llm-proxy",
            "autostart",
            "enable",
            "--config",
            "c.toml",
            "-p",
            "8080",
        ])
        .unwrap();
        let Commands::Autostart { action } = cli.command else {
            panic!("expected autostart")
        };
        let AutostartAction::Enable {
            config,
            port,
            force,
        } = action
        else {
            panic!("expected enable")
        };
        assert_eq!(config.unwrap().to_str(), Some("c.toml"));
        assert_eq!(port, Some(8080));
        assert!(!force, "--force should default to false");
    }

    #[test]
    fn parse_autostart_enable_force() {
        let cli = Cli::try_parse_from(["llm-proxy", "autostart", "enable", "--force"]).unwrap();
        let Commands::Autostart { action } = cli.command else {
            panic!("expected autostart")
        };
        let AutostartAction::Enable { force, .. } = action else {
            panic!("expected enable")
        };
        assert!(force, "--force must parse to true");
    }

    #[test]
    fn parse_autostart_disable() {
        let cli = Cli::try_parse_from(["llm-proxy", "autostart", "disable"]).unwrap();
        let Commands::Autostart { action } = cli.command else {
            panic!("expected autostart")
        };
        assert!(matches!(action, AutostartAction::Disable));
    }

    #[test]
    fn parse_autostart_status() {
        let cli = Cli::try_parse_from(["llm-proxy", "autostart", "status"]).unwrap();
        let Commands::Autostart { action } = cli.command else {
            panic!("expected autostart")
        };
        assert!(matches!(action, AutostartAction::Status));
    }

    #[test]
    fn parse_no_subcommand_fails() {
        let result = Cli::try_parse_from(["llm-proxy"]);
        assert!(result.is_err(), "clap requires a subcommand");
    }

    #[test]
    fn parse_unknown_subcommand_fails() {
        let result = Cli::try_parse_from(["llm-proxy", "explode"]);
        result.unwrap_err();
    }
}
