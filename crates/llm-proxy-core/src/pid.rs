//! PID file management for daemon mode.
//!
//! [`PidManager`] handles creating, reading, and removing the PID file used
//! to track a running daemon process.

use std::io::Write;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};

/// Errors returned by [`PidManager`] operations.
///
/// Typed so callers can branch on the failure mode; each variant carries the
/// filesystem path involved.
#[derive(Debug, thiserror::Error)]
pub enum PidError {
    /// A filesystem I/O error occurred while reading, writing, or removing a
    /// PID file, or while creating its parent directory.
    #[error("PID file I/O failed at {path}: {source}")]
    Io {
        /// The filesystem path involved in the failed operation.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The PID file contents could not be parsed as a `u32`.
    #[error("invalid PID contents in {path}: {source}")]
    Parse {
        /// The PID file path whose contents were unparseable.
        path: PathBuf,
        /// The underlying parse error.
        source: ParseIntError,
    },
}

/// Manages a PID file for daemon process tracking.
#[derive(Debug, Clone)]
pub struct PidManager {
    /// Directory for config and PID files.
    config_dir: PathBuf,
    /// Path to the PID file.
    pid_file: PathBuf,
}

impl PidManager {
    /// Create a new PID manager for the given config directory.
    ///
    /// The PID file will be placed at `<config_dir>/llm-proxy.pid`.
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        let config_dir = config_dir.into();
        let pid_file = config_dir.join("llm-proxy.pid");
        Self {
            config_dir,
            pid_file,
        }
    }

    /// Return the config directory path.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Return the PID file path.
    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    /// Read the PID from the PID file. Returns `None` if the file does not exist.
    pub fn read_pid(&self) -> Result<Option<u32>, PidError> {
        let content = match std::fs::read_to_string(&self.pid_file) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(PidError::Io {
                    path: self.pid_file.clone(),
                    source: e,
                });
            }
        };
        let pid: u32 = content.trim().parse().map_err(|source| PidError::Parse {
            path: self.pid_file.clone(),
            source,
        })?;
        Ok(Some(pid))
    }

    /// Write the current process PID to the PID file.
    ///
    /// Creates the config directory if it does not already exist.
    /// The PID file is written atomically via a temporary file and rename,
    /// so readers never see a partial/empty file.
    pub fn write_pid(&self) -> Result<(), PidError> {
        std::fs::create_dir_all(&self.config_dir).map_err(|source| PidError::Io {
            path: self.config_dir.clone(),
            source,
        })?;
        let pid = std::process::id();

        let tmp_path = self.pid_file.with_extension("pid.tmp");
        {
            #[cfg(unix)]
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o644)
                    .open(&tmp_path)
                    .map_err(|source| PidError::Io {
                        path: tmp_path.clone(),
                        source,
                    })?
            };
            #[cfg(not(unix))]
            let mut f = std::fs::File::create(&tmp_path).map_err(|source| PidError::Io {
                path: tmp_path.clone(),
                source,
            })?;

            write!(f, "{pid}").map_err(|source| PidError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        }
        std::fs::rename(&tmp_path, &self.pid_file).map_err(|source| PidError::Io {
            path: self.pid_file.clone(),
            source,
        })?;
        Ok(())
    }

    /// Remove the PID file.
    ///
    /// Silently succeeds if the file does not exist.
    pub fn remove_pid(&self) -> Result<(), PidError> {
        match std::fs::remove_file(&self.pid_file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PidError::Io {
                path: self.pid_file.clone(),
                source: e,
            }),
        }
    }

    /// Check if a process with the given PID is running.
    ///
    /// On Unix this uses `kill(pid, 0)` which only checks process existence
    /// without sending a signal. On non-Unix platforms this always returns
    /// `true` as a conservative fallback.
    ///
    /// # Notes
    ///
    /// - `EPERM` (permission denied) is treated as "running" because the
    ///   process exists even though we cannot signal it.
    /// - `ESRCH` (no such process) is treated as "not running".
    pub fn is_process_running(pid: u32) -> bool {
        #[cfg(unix)]
        {
            #[expect(
                unsafe_code,
                reason = "libc::kill has no safe wrapper in std; the SAFETY note below states the invariant"
            )]
            // SAFETY: `kill` is a plain syscall wrapper that dereferences no
            // pointers and touches no memory owned by this process, so it is
            // sound for every `pid`/`sig` pair. Signal 0 additionally performs
            // only the existence/permission check without delivering a signal,
            // so no process state is mutated. The only observable effect is the
            // return value and `errno`, both read immediately below.
            let ret = unsafe { libc::kill(pid as i32, 0) };
            if ret == 0 {
                true
            } else {
                let err = std::io::Error::last_os_error();
                err.raw_os_error() != Some(libc::ESRCH)
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PidError, PidManager};

    #[test]
    fn write_and_read_pid_roundtrip() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());

        mgr.write_pid().expect("write_pid");

        let pid = mgr.read_pid().expect("read_pid").expect("pid should exist");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn read_pid_missing_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());

        let result = mgr.read_pid().expect("read_pid should not error");
        assert!(result.is_none(), "expected None for missing PID file");
    }

    #[test]
    fn read_pid_invalid_content() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());

        std::fs::write(mgr.pid_file(), "not-a-number").expect("write garbage");

        match mgr.read_pid() {
            Err(PidError::Parse { path, source }) => {
                assert_eq!(
                    path,
                    *mgr.pid_file(),
                    "typed error should carry the PID file path"
                );
                let _ = source;
            }
            other => panic!("expected PidError::Parse, got {other:?}"),
        }
    }

    #[test]
    fn read_pid_io_error_is_typed() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());

        std::fs::create_dir(mgr.pid_file()).expect("create dir at pid path");

        match mgr.read_pid() {
            Err(PidError::Io { path, .. }) => {
                assert_eq!(
                    path,
                    *mgr.pid_file(),
                    "typed Io error should carry the PID file path"
                );
            }
            other => panic!("expected PidError::Io, got {other:?}"),
        }
    }

    #[test]
    fn is_process_running_current_process() {
        let pid = std::process::id();
        assert!(
            PidManager::is_process_running(pid),
            "current process should be running"
        );
    }

    #[test]
    fn is_process_running_nonexistent_pid() {
        let pid = 299_999_999;
        assert!(
            !PidManager::is_process_running(pid),
            "nonexistent PID should not be running"
        );
    }

    #[test]
    fn remove_pid_deletes_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());

        mgr.write_pid().expect("write_pid");
        assert!(mgr.pid_file().exists(), "PID file should exist after write");
        mgr.remove_pid().expect("remove_pid");
        assert!(
            !mgr.pid_file().exists(),
            "PID file should be gone after remove"
        );
    }

    #[test]
    fn remove_pid_succeeds_when_missing() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());
        mgr.remove_pid()
            .expect("remove_pid on missing file should succeed");
    }

    #[test]
    fn write_pid_is_atomic() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mgr = PidManager::new(dir.path());
        mgr.write_pid().expect("write_pid");

        let content = std::fs::read_to_string(mgr.pid_file()).expect("read PID file");
        let pid: u32 = content.trim().parse().expect("parse PID");
        assert_eq!(pid, std::process::id());
    }
}
