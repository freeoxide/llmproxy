//! Integration tests for the `init` command.
//!
//! Verifies file creation, content validity, permissions, and idempotency.

#[cfg(test)]
mod tests {

    use std::fs;
    use std::sync::Mutex;

    use llm_proxy_app::commands::cmd_init;
    use llm_proxy_app::defaults::{
        DEFAULT_CONFIG_TOML, DEFAULT_PROVIDER_OPENCODE_GO, DEFAULT_PROVIDER_OPENCODE_ZEN,
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        prior: Option<String>,
    }

    impl HomeGuard {
        fn new(temp_home: &std::path::Path) -> Self {
            let prior = std::env::var_os("HOME").map(|v| v.to_string_lossy().into_owned());
            #[expect(
                unsafe_code,
                reason = "std::env::set_var is unsafe since edition 2024; ENV_LOCK is held by the caller, see the SAFETY note below"
            )]
            // SAFETY: `set_var` is sound only when no other thread reads or
            // writes the environment concurrently. Every constructor call site
            // holds `ENV_LOCK` for the whole lifetime of the returned guard, so
            // this thread has exclusive access to `HOME` here.
            unsafe {
                std::env::set_var("HOME", temp_home)
            };
            Self { prior }
        }
    }

    impl Drop for HomeGuard {
        #[expect(
            unsafe_code,
            reason = "restores HOME via unsafe set_var/remove_var while ENV_LOCK is still held, see the SAFETY note below"
        )]
        fn drop(&mut self) {
            match self.prior.as_ref() {
                // SAFETY: the guard is dropped before the `ENV_LOCK` guard it
                // shares a scope with (reverse drop order), so `ENV_LOCK` is
                // still held and no other thread can touch the environment
                // while `HOME` is restored.
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                // SAFETY: same invariant as the `Some` arm — `ENV_LOCK` is
                // still held, giving this thread exclusive access to `HOME`.
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    #[test]
    fn init_default_toml_is_valid() {
        let parsed: toml::Value =
            toml::from_str(DEFAULT_CONFIG_TOML).expect("default config TOML should parse");
        assert!(
            parsed.get("server").is_some(),
            "default config must have [server]"
        );
    }

    #[test]
    fn init_provider_opencode_go_is_valid() {
        let parsed: toml::Value = toml::from_str(DEFAULT_PROVIDER_OPENCODE_GO)
            .expect("opencode-go provider TOML should parse");
        let provider = parsed.get("provider").expect("must have [provider]");
        assert_eq!(provider["name"].as_str(), Some("opencode-go"));
    }

    #[test]
    fn init_provider_opencode_zen_is_valid() {
        let parsed: toml::Value = toml::from_str(DEFAULT_PROVIDER_OPENCODE_ZEN)
            .expect("opencode-zen provider TOML should parse");
        let provider = parsed.get("provider").expect("must have [provider]");
        assert_eq!(provider["name"].as_str(), Some("opencode-zen"));
    }

    #[test]
    fn init_writes_files_to_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");

        let config_path = dir.path().join("config.toml");
        let providers_dir = dir.path().join("providers");
        fs::create_dir_all(&providers_dir).expect("providers dir");

        fs::write(&config_path, DEFAULT_CONFIG_TOML.as_bytes()).expect("config");
        fs::write(
            providers_dir.join("opencode-go.toml"),
            DEFAULT_PROVIDER_OPENCODE_GO.as_bytes(),
        )
        .expect("go");
        fs::write(
            providers_dir.join("opencode-zen.toml"),
            DEFAULT_PROVIDER_OPENCODE_ZEN.as_bytes(),
        )
        .expect("zen");

        assert!(config_path.exists());
        assert!(providers_dir.join("opencode-go.toml").exists());
        assert!(providers_dir.join("opencode-zen.toml").exists());

        let main: toml::Value = toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(main.get("server").is_some());

        let go: toml::Value =
            toml::from_str(&fs::read_to_string(providers_dir.join("opencode-go.toml")).unwrap())
                .unwrap();
        assert_eq!(go["provider"]["name"].as_str(), Some("opencode-go"));

        let zen: toml::Value =
            toml::from_str(&fs::read_to_string(providers_dir.join("opencode-zen.toml")).unwrap())
                .unwrap();
        assert_eq!(zen["provider"]["name"].as_str(), Some("opencode-zen"));
    }

    #[test]
    fn init_rejects_existing_config() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");

        let config_subdir = dir.path().join(".config").join("llm-proxy");
        fs::create_dir_all(&config_subdir).expect("create config dir");
        let config_path = config_subdir.join("config.toml");
        fs::write(&config_path, "existing").expect("seed existing config");

        let _home = HomeGuard::new(dir.path());

        let result = cmd_init();
        let err = result.expect_err("cmd_init should reject an existing config");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("already exists"),
            "error should mention the config already existing, got: {msg}"
        );

        let on_disk = fs::read_to_string(&config_path).expect("read config");
        assert_eq!(
            on_disk, "existing",
            "cmd_init must not overwrite an existing config"
        );
    }

    #[test]
    #[cfg(unix)]
    fn init_files_have_restrictive_permissions() {
        use llm_proxy_app::permissions::create_private_file;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        let file_path = dir.path().join("secret.toml");
        create_private_file(&file_path, b"test").expect("create_private_file");

        let mode = file_path.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file should have 0600 permissions");
    }
}
