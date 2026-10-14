//! Shared environment-variable interpolation for configuration files.

use regex::Regex;

static ENV_VAR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("env var regex is valid")
});

pub(crate) fn env_var_regex() -> &'static Regex {
    &ENV_VAR_RE
}

/// Replace `${ENV_VAR}` patterns in `input` with the value of the
/// corresponding environment variable; unset variables are left as-is.
///
/// # Trust boundary
///
/// Values are substituted as raw text, so a misconfigured environment
/// variable can inject arbitrary TOML structure into the output.
///
/// Interpolation runs on the whole raw file content before parsing;
/// `{model}`-style single-brace patterns are **not** interpolated.
pub fn interpolate_env_vars(input: &str) -> String {
    let re = env_var_regex();
    re.replace_all(input, |caps: &regex::Captures<'_>| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| caps[0].to_owned())
    })
    .into_owned()
}

/// Check whether `s` contains an unresolved `${VAR}` pattern.
///
/// Returns the first variable name found, or `None` if no pattern remains.
pub fn find_unresolved_env_var(s: &str) -> Option<String> {
    let re = env_var_regex();
    re.captures(s).map(|caps| caps[1].to_owned())
}

/// Find all `${VAR}` patterns in `s` and return them as `(var_name,
/// resolved_value)` pairs; the value is `None` when the variable is unset.
pub fn find_env_var_refs(s: &str) -> Vec<(String, Option<String>)> {
    let re = env_var_regex();
    re.captures_iter(s)
        .map(|caps| {
            let var_name = caps[1].to_owned();
            let value = std::env::var(&var_name).ok();
            (var_name, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_known_var() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _guard = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_INTERPOLATE", "hello");
        let result = interpolate_env_vars("key = ${_LLM_PROXY_TEST_INTERPOLATE}");
        assert_eq!(result, "key = hello");
    }

    #[test]
    fn leaves_unknown_var_as_is() {
        let result = interpolate_env_vars("key = ${_LLM_PROXY_NEVER_EXISTS_12345}");
        assert_eq!(result, "key = ${_LLM_PROXY_NEVER_EXISTS_12345}");
    }

    #[test]
    fn find_unresolved_returns_var_name() {
        assert_eq!(
            find_unresolved_env_var("${MISSING_VAR}"),
            Some("MISSING_VAR".to_owned())
        );
    }

    #[test]
    fn find_unresolved_returns_none_when_resolved() {
        assert_eq!(find_unresolved_env_var("no vars here"), None);
    }

    #[test]
    fn empty_braces_not_interpolated() {
        let result = interpolate_env_vars("key = ${}");
        assert_eq!(
            result, "key = ${}",
            "empty braces should not be interpolated"
        );
    }

    #[test]
    fn hyphenated_var_not_interpolated() {
        let result = interpolate_env_vars("key = ${my-var}");
        assert_eq!(
            result, "key = ${my-var}",
            "hyphenated var should not be interpolated"
        );
    }

    #[test]
    fn dotted_var_not_interpolated() {
        let result = interpolate_env_vars("key = ${my.var}");
        assert_eq!(
            result, "key = ${my.var}",
            "dotted var should not be interpolated"
        );
    }

    #[test]
    fn numeric_only_var_not_interpolated() {
        let result = interpolate_env_vars("key = ${123}");
        assert_eq!(result, "key = ${123}");
    }

    #[test]
    fn multiple_vars_in_single_string() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _a = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_MULTI_A", "hello");
        let _b = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_MULTI_B", "world");
        let result =
            interpolate_env_vars("${_LLM_PROXY_TEST_MULTI_A} and ${_LLM_PROXY_TEST_MULTI_B}");
        assert_eq!(result, "hello and world");
    }

    #[test]
    fn mix_of_resolved_and_unresolved() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _a = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_MIX_A", "yes");
        let result =
            interpolate_env_vars("${_LLM_PROXY_TEST_MIX_A} ${_LLM_PROXY_NEVER_EXISTS_MIX_B}");
        assert_eq!(result, "yes ${_LLM_PROXY_NEVER_EXISTS_MIX_B}");
    }

    #[test]
    fn var_set_to_empty_string_replaces_with_empty() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _guard = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_EMPTY_VAR", "");
        let result = interpolate_env_vars("key = ${_LLM_PROXY_TEST_EMPTY_VAR}");
        assert_eq!(result, "key = ");
    }

    #[test]
    fn adjacent_patterns() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _a = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_ADJ_A", "X");
        let _b = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_ADJ_B", "Y");
        let result = interpolate_env_vars("${_LLM_PROXY_TEST_ADJ_A}${_LLM_PROXY_TEST_ADJ_B}");
        assert_eq!(result, "XY");
    }

    #[test]
    fn dollar_sign_near_pattern() {
        let result = interpolate_env_vars("$$${_LLM_PROXY_NEVER_EXISTS_999}$$");
        assert_eq!(result, "$$${_LLM_PROXY_NEVER_EXISTS_999}$$");
    }

    #[test]
    fn find_env_var_refs_multiple_matches() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _guard_a = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_REFS_A", "val_a");
        let refs = find_env_var_refs("${_LLM_PROXY_TEST_REFS_A} and ${_LLM_PROXY_TEST_REFS_B}");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].0, "_LLM_PROXY_TEST_REFS_A");
        assert_eq!(refs[0].1, Some("val_a".to_owned()));
        assert_eq!(refs[1].0, "_LLM_PROXY_TEST_REFS_B");
        assert_eq!(refs[1].1, None);
    }

    #[test]
    fn find_env_var_refs_empty_var() {
        let _lock = crate::test_support::TestEnvLock::acquire();
        let _guard = crate::test_support::EnvVarGuard::set("_LLM_PROXY_TEST_REFS_EMPTY", "");
        let refs = find_env_var_refs("${_LLM_PROXY_TEST_REFS_EMPTY}");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "_LLM_PROXY_TEST_REFS_EMPTY");
        assert_eq!(refs[0].1, Some(String::new()));
    }

    #[test]
    fn find_env_var_refs_no_matches() {
        let refs = find_env_var_refs("no patterns here");
        assert!(refs.is_empty());
    }

    #[test]
    fn find_env_var_refs_duplicate_names() {
        let refs = find_env_var_refs("${DUP} and ${DUP}");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].0, "DUP");
        assert_eq!(refs[1].0, "DUP");
    }
}
