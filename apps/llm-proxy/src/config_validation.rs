//! Config file extension validation.
//!
//! Provides helpers to verify that config file paths use the `.toml` extension.

use std::path::Path;

use anyhow::{Result, bail};

/// Check if a path points to a TOML config file.
pub fn is_toml_config(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// Validate that the config file path has a supported extension.
///
/// Rejects non-TOML paths with an unsupported-extension error.
pub fn validate_toml_extension(path: &Path) -> Result<()> {
    if !is_toml_config(path) {
        bail!(
            "unsupported config file extension: {:?} (expected .toml)",
            path.extension()
                .map(|e| e.to_string_lossy())
                .unwrap_or_else(|| std::borrow::Cow::Borrowed("(none)"))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_validate_rejects_non_toml_extension() {
        let result = validate_toml_extension(Path::new("config.json"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"));
    }

    #[test]
    fn cmd_validate_accepts_toml_extension() {
        let result = validate_toml_extension(Path::new("config.toml"));
        result.unwrap();
    }

    #[test]
    fn cmd_validate_accepts_uppercase_toml() {
        let result = validate_toml_extension(Path::new("config.TOML"));
        result.unwrap();
    }

    #[test]
    fn cmd_validate_rejects_no_extension() {
        let result = validate_toml_extension(Path::new("config"));
        assert!(result.is_err());
    }

    #[test]
    fn is_toml_config_accepts_lowercase_toml() {
        assert!(is_toml_config(Path::new("a.toml")));
    }

    #[test]
    fn is_toml_config_accepts_uppercase_toml() {
        assert!(is_toml_config(Path::new("a.TOML")));
    }

    #[test]
    fn is_toml_config_rejects_json() {
        assert!(!is_toml_config(Path::new("a.json")));
    }

    #[test]
    fn is_toml_config_rejects_missing_extension() {
        assert!(!is_toml_config(Path::new("a")));
    }

    #[test]
    fn is_toml_config_rejects_double_extension() {
        assert!(!is_toml_config(Path::new("a.toml.bak")));
    }
}
