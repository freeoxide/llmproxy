//! PID file management helpers.
//!
//! Delegates to `llm_proxy_core::PidManager` and adds PID validation
//! (reject 0, overflow) plus restrictive file permissions.

use std::io::Write;

use anyhow::{Context, Result, bail};
use tracing::info;

use crate::paths::{config_dir, pid_file_path};
use crate::permissions::{set_private_dir_permissions, set_private_permissions};

/// Number of polling iterations when waiting for graceful shutdown.
pub const STOP_MAX_POLLS: u32 = 50;

/// Interval between polls when waiting for graceful shutdown.
pub const STOP_POLL_INTERVAL_MS: u64 = 200;

pub(crate) fn pid_manager() -> llm_proxy_core::PidManager {
    llm_proxy_core::PidManager::new(config_dir())
}

/// Read the PID from the PID file via the given manager.
///
/// Returns `None` if the file does not exist. Rejects PID 0 and PIDs above
/// `i32::MAX` as corrupt.
pub fn read_pid_from(mgr: &llm_proxy_core::PidManager) -> Result<Option<u32>> {
    let pid = mgr.read_pid()?;
    if let Some(p) = pid {
        if p == 0 {
            bail!("invalid PID 0 in PID file");
        }
        if p > i32::MAX as u32 {
            bail!("PID {p} exceeds i32::MAX -- cannot be a valid process ID");
        }
    }
    Ok(pid)
}

/// Read the PID from the default PID file. Returns `None` if it does not exist.
///
/// Rejects PID 0 and PIDs above `i32::MAX` as corrupt.
pub fn read_pid() -> Result<Option<u32>> {
    read_pid_from(&pid_manager())
}

/// Atomically create the PID file with exclusive ownership.
///
/// Fails if the file already exists (`create_new`); the file is written with
/// 0600 permissions.
pub fn write_pid() -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;
    set_private_dir_permissions(&dir)?;
    let path = pid_file_path();
    let pid = std::process::id();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating PID file {}", path.display()))?;
    write!(f, "{pid}").with_context(|| format!("writing PID file {}", path.display()))?;
    set_private_permissions(&path)?;
    info!(pid, "wrote PID file");
    Ok(())
}

/// Write a specific PID value (e.g. a daemon child's PID) to a new PID file.
pub fn write_pid_value(pid: u32) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;
    set_private_dir_permissions(&dir)?;
    let path = pid_file_path();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating PID file {}", path.display()))?;
    write!(f, "{pid}").with_context(|| format!("writing PID file {}", path.display()))?;
    set_private_permissions(&path)?;
    info!(pid, "wrote PID file");
    Ok(())
}

/// Remove the PID file.
pub fn remove_pid() -> Result<()> {
    Ok(pid_manager().remove_pid()?)
}

/// Check if a process with the given PID is running.
///
/// Returns `false` for PID 0 (which would signal the entire process group on
/// Unix) and on non-Unix platforms.
pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    llm_proxy_core::PidManager::is_process_running(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_rejects_zero_pid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("llm-proxy.pid");
        std::fs::write(&pid_path, "0").unwrap();

        let mgr = llm_proxy_core::PidManager::new(dir.path());
        let result = read_pid_from(&mgr);
        assert!(
            result.is_err(),
            "expected PID 0 to be rejected, got {result:?}"
        );
    }

    #[test]
    fn is_process_running_current_process() {
        let pid = std::process::id();
        assert!(is_process_running(pid));
    }

    #[test]
    fn is_process_running_nonexistent_pid() {
        assert!(!is_process_running(299_999_999));
    }

    #[test]
    fn is_process_running_rejects_pid_zero() {
        assert!(!is_process_running(0));
    }
}
