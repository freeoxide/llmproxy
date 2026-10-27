use std::path::PathBuf;

use thiserror::Error;

use crate::provider_config::ConfigValidationError;

/// Errors that can occur in `llm-proxy-core`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// Failed to read a config file from disk.
    #[error("failed to read config at {path}: {source}")]
    ConfigLoad {
        /// Path to the config file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Failed to parse a TOML config file.
    #[error("failed to parse config: {0}")]
    ConfigParse(#[from] toml::de::Error),
    /// Config validation failed.
    ///
    /// The structured [`ConfigValidationError`] is carried typed (not boxed
    /// behind `dyn Error`); match it via [`CoreError::validation_error`].
    #[error("config validation error: {message}")]
    ConfigValidation {
        /// Human-readable description of the validation failure.
        message: String,
        /// The structured validation error, when this failure originated from
        /// a typed [`ConfigValidationError`]. `None` for message-only
        /// validations.
        #[source]
        source: Option<ConfigValidationError>,
    },
    /// Provider registry construction or protocol validation failed.
    ///
    /// Carries only a diagnostic message; there is no typed source.
    #[error("provider resolution error: {message}")]
    ProviderResolution {
        /// Human-readable description of the resolution failure.
        message: String,
    },
}

impl CoreError {
    /// Create a [`CoreError::ConfigValidation`] from a message alone.
    pub fn config_validation(message: impl Into<String>) -> Self {
        Self::ConfigValidation {
            message: message.into(),
            source: None,
        }
    }

    /// Create a [`CoreError::ProviderResolution`] from a message alone.
    pub fn provider_resolution(message: impl Into<String>) -> Self {
        Self::ProviderResolution {
            message: message.into(),
        }
    }

    /// Return the structured validation error when this is a typed
    /// [`CoreError::ConfigValidation`], otherwise `None`.
    ///
    /// ```
    /// use llm_proxy_core::{CoreError, provider_config::ConfigValidationError};
    ///
    /// let err: CoreError = ConfigValidationError::EmptyProviderName.into();
    /// assert_eq!(
    ///     err.validation_error(),
    ///     Some(&ConfigValidationError::EmptyProviderName)
    /// );
    /// ```
    #[must_use]
    pub fn validation_error(&self) -> Option<&ConfigValidationError> {
        if let Self::ConfigValidation { source, .. } = self {
            source.as_ref()
        } else {
            None
        }
    }
}

impl From<ConfigValidationError> for CoreError {
    fn from(e: ConfigValidationError) -> Self {
        Self::ConfigValidation {
            message: e.to_string(),
            source: Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn config_load_display_includes_path() {
        let err = CoreError::ConfigLoad {
            path: PathBuf::from("/tmp/test.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/tmp/test.toml"),
            "display should contain path"
        );
        assert!(msg.contains("failed to read config"));
    }

    #[test]
    fn config_load_has_source() {
        let err = CoreError::ConfigLoad {
            path: PathBuf::from("/tmp/test.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert!(err.source().is_some(), "ConfigLoad should have a source");
    }

    #[test]
    fn config_parse_display() {
        let err = CoreError::ConfigParse(toml::from_str::<toml::Value>("bad [[[").unwrap_err());
        let msg = err.to_string();
        assert!(msg.contains("failed to parse config"));
    }

    #[test]
    fn config_validation_from_message() {
        let err = CoreError::config_validation("test message");
        let msg = err.to_string();
        assert!(msg.contains("test message"));
        assert!(err.source().is_none());
    }

    #[test]
    fn config_validation_from_config_validation_error() {
        let validation_err = crate::provider_config::ConfigValidationError::EmptyProviderName;
        let core_err: CoreError = validation_err.into();
        assert_eq!(
            core_err.validation_error(),
            Some(&crate::provider_config::ConfigValidationError::EmptyProviderName),
        );
        assert!(core_err.source().is_some());
    }

    #[test]
    fn config_validation_preserves_fielded_variant() {
        let validation_err = crate::provider_config::ConfigValidationError::EmptyApiKey {
            provider: "my-provider".to_owned(),
        };
        let core_err: CoreError = validation_err.into();
        match core_err.validation_error() {
            Some(crate::provider_config::ConfigValidationError::EmptyApiKey { provider }) => {
                assert_eq!(provider, "my-provider");
            }
            other => panic!("expected EmptyApiKey, got {other:?}"),
        }
    }

    #[test]
    fn provider_resolution_from_message() {
        let err = CoreError::provider_resolution("something went wrong");
        let msg = err.to_string();
        assert!(msg.contains("something went wrong"));
        assert!(err.source().is_none());
    }
}
