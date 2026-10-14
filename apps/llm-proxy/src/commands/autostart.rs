use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::paths::config_dir;
#[cfg(target_os = "macos")]
use crate::paths::launchd_plist_path;
#[cfg(target_os = "linux")]
use crate::paths::linux_autostart_dir;
use crate::permissions::set_private_permissions;
#[cfg(target_os = "linux")]
use crate::platform::format_desktop_entry;
#[cfg(target_os = "macos")]
use crate::platform::format_plist;

/// Run the `autostart enable` command.
///
/// Aborts when the launchd plist (macOS) or `.desktop` entry (Linux) already
/// exists, unless `force` is set.
pub fn cmd_autostart_enable(
    config_path: Option<&Path>,
    port: Option<u16>,
    force: bool,
) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;

    let exe = std::env::current_exe().with_context(|| "resolving current executable")?;
    let mut args = vec![exe.to_string_lossy().to_string(), "serve".to_owned()];

    if let Some(p) = config_path {
        args.push("--config".to_owned());
        args.push(p.to_string_lossy().to_string());
    }
    if let Some(p) = port {
        args.push("--port".to_owned());
        args.push(p.to_string());
    }

    #[cfg(target_os = "macos")]
    {
        let plist_content = format_plist(&args);
        let plist_path = launchd_plist_path();

        let plist_dir = plist_path
            .parent()
            .with_context(|| format!("invalid plist path: {}", plist_path.display()))?;
        std::fs::create_dir_all(plist_dir)
            .with_context(|| format!("creating {}", plist_dir.display()))?;

        require_overwrite_ok(&plist_path, "launchd plist", force)?;

        std::fs::write(&plist_path, plist_content)
            .with_context(|| format!("writing {}", plist_path.display()))?;
        set_private_permissions(&plist_path)?;

        println!("created launchd plist at {}", plist_path.display());
        println!("run: launchctl load {}", plist_path.display());
    }

    #[cfg(target_os = "linux")]
    {
        let desktop_content = format_desktop_entry(&args);
        let autostart_dir = linux_autostart_dir();
        std::fs::create_dir_all(&autostart_dir)
            .with_context(|| format!("creating {}", autostart_dir.display()))?;
        let desktop_path = autostart_dir.join("llm-proxy.desktop");
        require_overwrite_ok(&desktop_path, "desktop entry", force)?;
        std::fs::write(&desktop_path, desktop_content)
            .with_context(|| format!("writing {}", desktop_path.display()))?;
        set_private_permissions(&desktop_path)?;
        println!("created desktop entry at {}", desktop_path.display());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("auto-start is not supported on this platform");
        println!("add this command to your shell startup script:");
        println!("  {}", args.join(" "));
    }

    Ok(())
}

/// Run the `autostart disable` command.
pub fn cmd_autostart_disable() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = launchd_plist_path();

        if plist_path.exists() {
            let unload_result = std::process::Command::new("launchctl")
                .args(["unload", &plist_path.to_string_lossy()])
                .output();
            match unload_result {
                Ok(output) if !output.status.success() => {
                    eprintln!(
                        "warning: launchctl unload failed (exit {:?}): {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Err(e) => {
                    eprintln!("warning: failed to run launchctl unload: {e}");
                }
                _ => {}
            }
            std::fs::remove_file(&plist_path)
                .with_context(|| format!("removing {}", plist_path.display()))?;
            println!("removed launchd plist");
        } else {
            println!("no launchd plist found");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let autostart_dir = linux_autostart_dir();
        let desktop_path = autostart_dir.join("llm-proxy.desktop");
        if desktop_path.exists() {
            std::fs::remove_file(&desktop_path)
                .with_context(|| format!("removing {}", desktop_path.display()))?;
            println!("removed desktop entry");
        } else {
            println!("no desktop entry found");
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("auto-start is not supported on this platform");
    }

    Ok(())
}

/// Run the `autostart status` command.
pub fn cmd_autostart_status() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = launchd_plist_path();

        if plist_path.exists() {
            println!("auto-start is enabled ({})", plist_path.display());
        } else {
            println!("auto-start is disabled (no launchd plist found)");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let autostart_dir = linux_autostart_dir();
        let desktop_path = autostart_dir.join("llm-proxy.desktop");
        if desktop_path.exists() {
            println!("auto-start is enabled ({})", desktop_path.display());
        } else {
            println!("auto-start is disabled (no desktop entry found)");
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("auto-start is not supported on this platform");
    }

    Ok(())
}

fn require_overwrite_ok(path: &std::path::Path, kind: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{kind} already exists at {}; re-run with --force to overwrite",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_overwrite_ok_blocks_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("unit");
        std::fs::write(&p, "existing").unwrap();

        let err = require_overwrite_ok(&p, "launchd plist", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("already exists"),
            "expected 'already exists': {msg}"
        );
        assert!(msg.contains("--force"), "expected --force hint: {msg}");
        assert!(msg.contains("launchd plist"), "expected kind label: {msg}");
    }

    #[test]
    fn require_overwrite_ok_allows_existing_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("unit");
        std::fs::write(&p, "existing").unwrap();

        require_overwrite_ok(&p, "launchd plist", true).expect("--force permits overwrite");
    }

    #[test]
    fn require_overwrite_ok_allows_missing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("absent");

        require_overwrite_ok(&p, "desktop entry", false).expect("missing file is always allowed");
    }
}
