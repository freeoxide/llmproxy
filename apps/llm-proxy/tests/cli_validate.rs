//! Integration tests for the `validate` command.
//!
//! Covers end-to-end `cmd_validate` behaviour: valid TOML, invalid TOML, and
//! the wrong-extension error surfacing through the command. The pure extension
//! accept/reject matrix is exercised as per-case unit tests in
//! `src/config_validation.rs` (see LOW-20).

#[cfg(test)]
mod tests {

    use llm_proxy_app::commands::cmd_validate;
    use llm_proxy_app::defaults::DEFAULT_CONFIG_TOML;

    #[test]
    fn validate_accepts_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, DEFAULT_CONFIG_TOML).unwrap();

        let result = cmd_validate(Some(&config));
        assert!(
            result.is_ok(),
            "default config should validate: {:?}",
            result
        );
    }

    #[test]
    fn validate_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bad.toml");
        std::fs::write(&config, "not valid toml {{{{").unwrap();

        let result = cmd_validate(Some(&config));
        assert!(result.is_err(), "malformed TOML should fail");
    }

    #[test]
    fn validate_rejects_json_extension() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        std::fs::write(&config, "{}").unwrap();

        let result = cmd_validate(Some(&config));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported") || msg.contains("extension"),
            "error should mention unsupported extension: {msg}"
        );
    }

    #[test]
    fn validate_rejects_non_loopback_bind_without_client_keys() {
        // The client-identity safety rule, exercised through the command
        // layer: an anonymous proxy exposed past loopback must refuse to
        // start, so validation fails before any listener could bind.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[server]
bind = "0.0.0.0:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"
"#,
        )
        .unwrap();

        let result = cmd_validate(Some(&config));
        // anyhow's `to_string()` shows only the outer context; `{:#}` walks
        // the source chain down to the CoreError validation message.
        let msg = format!(
            "{:#}",
            result.expect_err("non-loopback anonymous bind must be rejected")
        );
        assert!(
            msg.contains("not loopback"),
            "error should explain the loopback restriction: {msg}"
        );
    }

    #[test]
    fn validate_accepts_client_keys_on_non_loopback_bind() {
        // With at least one hashed client key configured, binding off
        // loopback is a legitimate deployment and must validate.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[server]
bind = "0.0.0.0:3456"
request_timeout = "300s"
log_level = "info"
hot_reload = false
server_name = "test"

[auth]
mode = "client-keys"

[auth.clients]
laptop = "$argon2id$v=19$m=8,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$eEnjdfBL8hfvU2QuGOpow8Yq2Jyrr8GXKYgtAAsscSs"
"#,
        )
        .unwrap();

        let result = cmd_validate(Some(&config));
        assert!(
            result.is_ok(),
            "client-keys on a non-loopback bind should validate: {result:?}"
        );
    }
}
