//! Integration tests for the `stop` command, driven against a tempdir-backed
//! [`CommandPaths`]: no PID file, stale-PID cleanup, graceful SIGTERM, and the
//! SIGKILL escalation.

#[cfg(test)]
mod tests {

    use std::path::Path;
    use std::time::Duration;

    use llm_proxy_app::commands::{cmd_stop, cmd_stop_with_timing};
    use llm_proxy_app::paths::CommandPaths;
    use llm_proxy_core::PidManager;

    fn tempdir_paths(dir: &Path) -> CommandPaths {
        CommandPaths::new(PidManager::new(dir), dir.join("config.toml"))
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::process::Command;

        #[test]
        fn stop_no_pid_file_is_ok() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            cmd_stop(&paths).expect("no PID file -> Ok");
            assert!(
                !paths.pid_manager().pid_file().exists(),
                "no PID file should have been created or left behind"
            );
        }

        #[test]
        fn stop_stale_pid_is_cleaned_up() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            std::fs::write(paths.pid_manager().pid_file(), "299999999").unwrap();

            cmd_stop(&paths).expect("stale PID -> Ok + cleanup");
            assert!(
                !paths.pid_manager().pid_file().exists(),
                "stale PID file must be removed on the cleanup path"
            );
        }

        #[test]
        fn stop_graceful_sigterm_kills_live_child() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            std::fs::write(paths.pid_manager().pid_file(), child.id().to_string()).unwrap();

            cmd_stop_with_timing(&paths, 20, Duration::from_millis(5))
                .expect("graceful stop -> Ok");

            let status = child.wait().unwrap();
            assert!(
                status.code().is_none(),
                "child should have been terminated by a signal, not exited normally"
            );
            assert!(
                !paths.pid_manager().pid_file().exists(),
                "PID file must be removed after graceful stop"
            );
        }

        #[test]
        fn stop_sigkill_escalation_when_term_ignored() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());

            let mut child = Command::new("sh")
                .arg("-c")
                .arg("trap '' TERM; sleep 30 & wait")
                .spawn()
                .unwrap();
            std::fs::write(paths.pid_manager().pid_file(), child.id().to_string()).unwrap();

            cmd_stop_with_timing(&paths, 2, Duration::from_millis(5)).expect("escalation -> Ok");

            let status = child.wait().unwrap();
            assert!(
                status.code().is_none(),
                "child should have been SIGKILLed after ignoring SIGTERM"
            );
            assert!(
                !paths.pid_manager().pid_file().exists(),
                "PID file must be removed after force stop"
            );
        }
    }

    #[cfg(not(unix))]
    mod non_unix {
        use super::*;

        #[test]
        fn stop_unsupported_off_unix() {
            let dir = tempfile::tempdir().unwrap();
            let paths = tempdir_paths(dir.path());
            let err = cmd_stop(&paths).expect_err("stop bails off-Unix");
            assert!(
                err.to_string().contains("not supported"),
                "expected an unsupported-platform error, got: {err}"
            );
        }
    }
}
