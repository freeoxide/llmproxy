//! Integration tests for the `serve` command: pre-flight config validation
//! only (starting the server binds and blocks). The full extension matrix
//! lives in `src/config_validation.rs`.

#[cfg(test)]
mod tests {

    use llm_proxy_app::config_validation::validate_toml_extension;

    #[test]
    fn serve_rejects_non_toml_config_with_unsupported_message() {
        let result = validate_toml_extension(std::path::Path::new("config.json"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported"),
            "expected 'unsupported' in error: {msg}"
        );
    }
}
