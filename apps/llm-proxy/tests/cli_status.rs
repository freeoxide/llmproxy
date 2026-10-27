//! Integration tests for the `status` command, driven against a tempdir-backed
//! [`CommandPaths`].

#[cfg(test)]
mod tests {

    use std::path::Path;

    use llm_proxy_app::commands::cmd_status;
    use llm_proxy_app::defaults::DEFAULT_CONFIG_TOML;
    use llm_proxy_app::paths::CommandPaths;
    use llm_proxy_core::PidManager;

    fn tempdir_paths(dir: &Path) -> CommandPaths {
        CommandPaths::new(PidManager::new(dir), dir.join("config.toml"))
    }

    #[test]
    fn status_no_pid_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let paths = tempdir_paths(dir.path());
        cmd_status(&paths).expect("no PID file -> Ok");
    }

    #[test]
    fn status_stale_pid_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let paths = tempdir_paths(dir.path());
        std::fs::write(paths.pid_manager().pid_file(), "299999999").unwrap();
        cmd_status(&paths).expect("stale PID -> Ok");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::process::Command;

        #[test]
        fn status_running_process_is_ok() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            std::fs::write(paths.pid_manager().pid_file(), child.id().to_string()).unwrap();

            cmd_status(&paths).expect("running process -> Ok");

            child.kill().expect("kill sleep child");
            child.wait().expect("reap sleep child");
        }

        #[test]
        fn status_with_present_config_is_ok() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            std::fs::write(paths.config_path(), DEFAULT_CONFIG_TOML.as_bytes()).unwrap();
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            std::fs::write(paths.pid_manager().pid_file(), child.id().to_string()).unwrap();

            cmd_status(&paths).expect("running + config -> Ok");

            child.kill().expect("kill sleep child");
            child.wait().expect("reap sleep child");
        }
    }
}
