use std::time::Duration;

use anyhow::{Result, bail};

use crate::paths::CommandPaths;
use crate::pid::{STOP_MAX_POLLS, STOP_POLL_INTERVAL_MS, is_process_running, read_pid_from};

#[cfg(unix)]
#[derive(Debug)]
enum PidOwnership {
    StillOurs,
    Changed(u32),
    Gone,
}

#[cfg(unix)]
fn classify_pid_ownership(mgr: &llm_proxy_core::PidManager, expected: u32) -> PidOwnership {
    match read_pid_from(mgr) {
        Ok(Some(current)) if current == expected => PidOwnership::StillOurs,
        Ok(Some(other)) => PidOwnership::Changed(other),
        Ok(None) => PidOwnership::Gone,
        Err(_) => PidOwnership::Gone,
    }
}

/// Run the `stop` command against the given paths using the production poll
/// cadence ([`STOP_MAX_POLLS`] × [`STOP_POLL_INTERVAL_MS`]).
pub fn cmd_stop(paths: &CommandPaths) -> Result<()> {
    cmd_stop_with_timing(
        paths,
        STOP_MAX_POLLS,
        Duration::from_millis(STOP_POLL_INTERVAL_MS),
    )
}

/// Run the `stop` command with an explicit graceful-shutdown poll cadence.
///
/// `max_polls` × `poll_interval` bounds how long the command waits for the
/// process to exit after SIGTERM before escalating to SIGKILL.
pub fn cmd_stop_with_timing(
    paths: &CommandPaths,
    max_polls: u32,
    poll_interval: Duration,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (max_polls, poll_interval);
        bail!("error: stop is not supported on this platform");
    }

    let mgr = paths.pid_manager();
    match read_pid_from(mgr)? {
        Some(pid) => {
            if !is_process_running(pid) {
                println!("server not running (stale PID {pid})");
                mgr.remove_pid()?;
                return Ok(());
            }

            #[cfg(unix)]
            {
                match classify_pid_ownership(mgr, pid) {
                    PidOwnership::StillOurs => {}
                    PidOwnership::Changed(other) => {
                        bail!(
                            "PID file changed while stopping (was {pid}, now {other}); \
                             refusing to signal {pid} to avoid hitting an unrelated process"
                        );
                    }
                    PidOwnership::Gone => {
                        println!(
                            "PID file disappeared before signaling; server likely already \
                             stopped (was PID {pid})"
                        );
                        return Ok(());
                    }
                }

                #[expect(
                    unsafe_code,
                    reason = "libc::kill has no safe wrapper in std; pid ownership is verified immediately above"
                )]
                // SAFETY: `kill` is a bare syscall wrapper: it dereferences no
                // pointers and touches no memory owned by this process, so it is
                // sound for any `pid`/`sig` pair. The `classify_pid_ownership`
                // check immediately above establishes that `pid` is still the
                // one recorded in our PID file, which is what makes the
                // graceful signal correct (not merely sound). The result and
                // `errno` are consumed on the next line.
                let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                if ret != 0 {
                    let err = std::io::Error::last_os_error();
                    bail!(
                        "failed to send SIGTERM to PID {pid}: {} (os error {})",
                        err,
                        err.raw_os_error().unwrap_or(0)
                    );
                }
                println!("sent SIGTERM to PID {pid}");
            }

            for _ in 0..max_polls {
                if !is_process_running(pid) {
                    mgr.remove_pid()?;
                    println!("server stopped");
                    return Ok(());
                }
                std::thread::sleep(poll_interval);
            }

            println!(
                "WARNING: PID {pid} did not exit within {} seconds, escalating to SIGKILL",
                max_polls as u64 * poll_interval.as_millis() as u64 / 1000
            );

            #[cfg(unix)]
            {
                match classify_pid_ownership(mgr, pid) {
                    PidOwnership::StillOurs => {}
                    PidOwnership::Changed(other) => {
                        bail!(
                            "PID file changed during graceful-shutdown wait \
                             (was {pid}, now {other}); refusing to SIGKILL {pid} to \
                             avoid hitting an unrelated process"
                        );
                    }
                    PidOwnership::Gone => {
                        println!("server stopped during graceful-shutdown wait (PID {pid} gone)");
                        return Ok(());
                    }
                }
                #[expect(
                    unsafe_code,
                    reason = "libc::kill has no safe wrapper in std; pid ownership is verified immediately above"
                )]
                // SAFETY: `kill` is a bare syscall wrapper: it dereferences no
                // pointers and touches no memory owned by this process, so it is
                // sound for any `pid`/`sig` pair. The `classify_pid_ownership`
                // check immediately above establishes that `pid` is still the
                // one recorded in our PID file, which is what makes the
                // forced signal correct (not merely sound). The result and
                // `errno` are consumed on the next line.
                let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                if ret != 0 {
                    let err = std::io::Error::last_os_error();
                    bail!(
                        "failed to send SIGKILL to PID {pid}: {} (os error {})",
                        err,
                        err.raw_os_error().unwrap_or(0)
                    );
                }
            }
            mgr.remove_pid()?;
            println!("server force-stopped (PID {pid})");
            Ok(())
        }
        None => {
            println!("no PID file found -- server not running");
            Ok(())
        }
    }
}
