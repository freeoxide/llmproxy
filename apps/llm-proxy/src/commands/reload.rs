//! The `reload` command (plan 07 step 11): signal a running server (by its
//! PID file) to re-read its provider and pool files. Unix only — the signal
//! is SIGHUP.

use anyhow::{Result, bail};

use crate::paths::CommandPaths;
use crate::pid::{is_process_running, read_pid_from};

/// Run the `reload` command against the given paths.
pub fn cmd_reload(paths: &CommandPaths) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = paths;
        bail!("reload is not supported on this platform (SIGHUP does not exist)");
    }

    #[cfg(unix)]
    {
        let mgr = paths.pid_manager();
        let Some(pid) = read_pid_from(mgr)? else {
            bail!("no PID file found -- is the server running?");
        };
        if !is_process_running(pid) {
            bail!(
                "PID {pid} is not running (stale PID file at {})",
                mgr.pid_file().display()
            );
        }
        // Re-read before signaling: the same ownership discipline `stop`
        // uses, so a replaced PID file cannot make us signal the wrong
        // process.
        match read_pid_from(mgr)? {
            Some(current) if current == pid => {}
            Some(other) => {
                bail!(
                    "PID file changed (was {pid}, now {other}); refusing to signal {pid} to \
                     avoid hitting an unrelated process"
                );
            }
            None => bail!("PID file disappeared before signaling; server likely stopped"),
        }
        #[expect(
            unsafe_code,
            reason = "libc::kill has no safe wrapper in std; pid ownership is verified immediately above"
        )]
        // SAFETY: `kill` is a bare syscall wrapper: it dereferences no
        // pointers and touches no memory owned by this process, so it is
        // sound for any `pid`/`sig` pair. The re-read immediately above
        // establishes that `pid` is still the one recorded in our PID file,
        // which is what makes the signal correct (not merely sound).
        let ret = unsafe { libc::kill(pid as i32, libc::SIGHUP) };
        if ret != 0 {
            let error = std::io::Error::last_os_error();
            bail!(
                "failed to send SIGHUP to PID {pid}: {error} (os error {})",
                error.raw_os_error().unwrap_or(0)
            );
        }
        println!("sent SIGHUP to PID {pid}; the server logs the reload outcome");
        Ok(())
    }
}
