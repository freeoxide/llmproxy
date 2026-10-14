use anyhow::Result;
use llm_proxy_core::load_app_config;

use crate::paths::CommandPaths;
use crate::pid::{is_process_running, read_pid_from};

/// Run the `status` command against the given paths.
///
/// The displayed `listen` address is read from `paths.config_path()` and may
/// not match the actual bind address.
pub fn cmd_status(paths: &CommandPaths) -> Result<()> {
    let mgr = paths.pid_manager();
    match read_pid_from(mgr)? {
        Some(pid) => {
            if is_process_running(pid) {
                println!("server is running (PID {pid})");
                let config_path = paths.config_path();
                if config_path.exists() {
                    if let Ok(cfg) = load_app_config(config_path) {
                        println!("  listen: {}", cfg.server.bind);
                    } else {
                        println!("  listen: (could not parse config)");
                    }
                } else {
                    println!("  listen: (config not found: {})", config_path.display());
                }
                println!("  config: {}", config_path.display());
            } else {
                println!("server not running (stale PID {pid})");
            }
            Ok(())
        }
        None => {
            println!("server not running (no PID file)");
            Ok(())
        }
    }
}
