//! Provider model catalog types, merging, and filtering.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{CoreError, ProviderCatalogConfig, ProviderCatalogMode, StaticModelCatalogEntry};

/// Metadata stored alongside a persisted provider catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFileMetadata {
    /// Provider that owns the catalog.
    pub provider: String,
    /// Catalog source, normally `live`.
    pub source: String,
    /// Best-effort timestamp string (normally RFC 3339) recording when the
    /// catalog was generated. Opaque and **not** validated at parse time:
    /// consumers treat a malformed value as stale.
    pub generated_at: String,
    /// Discovered model entries.
    #[serde(default)]
    pub models: Vec<StaticModelCatalogEntry>,
}

/// On-disk catalog cache file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    /// Cache metadata.
    pub catalog: CatalogFileMetadata,
}

/// Parse a persisted catalog cache file.
///
/// Returns [`CoreError::ConfigParse`] when the input is not valid TOML or does
/// not match the on-disk catalog schema.
///
/// ```
/// use llm_proxy_core::parse_catalog_file;
///
/// let raw = r#"
/// [catalog]
/// provider = "example"
/// source = "live"
/// generated_at = "2026-06-09T00:00:00Z"
///
/// [[catalog.models]]
/// id = "gpt-4"
/// "#;
/// let file = parse_catalog_file(raw).expect("valid catalog TOML");
/// assert_eq!(file.catalog.provider, "example");
/// assert_eq!(file.catalog.models.len(), 1);
/// assert_eq!(file.catalog.models[0].id, "gpt-4");
/// ```
pub fn parse_catalog_file(raw: &str) -> Result<CatalogFile, CoreError> {
    toml::from_str(raw).map_err(CoreError::ConfigParse)
}

/// Merge static and discovered catalog entries according to provider config.
///
/// In `Hybrid` mode, static entries override discovered entries that share
/// the same `id` (static metadata wins).
///
/// ```
/// use std::time::Duration;
/// use llm_proxy_core::{ProviderCatalogConfig, ProviderCatalogMode, StaticModelCatalogEntry, merge_catalog};
/// use llm_proxy_core::ProviderRouteKind;
///
/// let cfg = ProviderCatalogConfig {
///     mode: ProviderCatalogMode::Discovered,
///     enforce: false,
///     cache_ttl: Duration::from_secs(60),
///     allow: vec!["*".to_owned()],
///     deny: Vec::new(),
///     models: Vec::new(),
/// };
/// let discovered = vec![StaticModelCatalogEntry {
///     id: "gpt-4".to_owned(),
///     display_name: None,
///     supports: vec![ProviderRouteKind::ChatCompletions],
///     context_length: None,
/// }];
/// let merged = merge_catalog(&cfg, &discovered);
/// assert_eq!(merged.len(), 1);
/// assert_eq!(merged[0].id, "gpt-4");
/// ```
#[must_use]
pub fn merge_catalog(
    config: &ProviderCatalogConfig,
    discovered: &[StaticModelCatalogEntry],
) -> Vec<StaticModelCatalogEntry> {
    let mut entries: Vec<StaticModelCatalogEntry> = Vec::new();

    if matches!(
        config.mode,
        ProviderCatalogMode::Discovered | ProviderCatalogMode::Hybrid
    ) {
        for model in discovered {
            entries.push(model.clone());
        }
    }
    if matches!(
        config.mode,
        ProviderCatalogMode::Static | ProviderCatalogMode::Hybrid
    ) {
        for model in &config.models {
            if let Some(existing) = entries.iter_mut().find(|entry| entry.id == model.id) {
                *existing = model.clone();
            } else {
                entries.push(model.clone());
            }
        }
    }

    entries.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    entries
        .into_iter()
        .filter(|model| model_allowed(&model.id, &config.allow, &config.deny))
        .collect()
}

/// Return whether a model ID passes the configured glob filters.
///
/// A model is allowed when it matches the `allow` list (or `allow` is empty)
/// and is not matched by `deny`, unless a non-wildcard `allow` pattern
/// explicitly includes it (which overrides `deny`).
///
/// ```
/// use llm_proxy_core::model_allowed;
///
/// // Empty allow list permits everything not explicitly denied.
/// assert!(model_allowed("gpt-4", &[], &[]));
/// // Glob allow pattern matches.
/// assert!(model_allowed("gpt-4", &["gpt-*".to_owned()], &[]));
/// // Deny excludes unless a concrete allow pattern overrides it.
/// assert!(!model_allowed("gpt-4-preview", &["*".to_owned()], &["*-preview".to_owned()]));
/// assert!(model_allowed(
///     "gpt-4-preview",
///     &["*".to_owned(), "gpt-4-preview".to_owned()],
///     &["*-preview".to_owned()],
/// ));
/// ```
#[must_use]
pub fn model_allowed(model: &str, allow: &[String], deny: &[String]) -> bool {
    let matched: Vec<bool> = allow
        .iter()
        .map(|pattern| glob_matches(pattern, model))
        .collect();
    let allowed = allow.is_empty() || matched.iter().any(|&m| m);
    let denied = deny.iter().any(|pattern| glob_matches(pattern, model));
    let explicitly_allowed = allow
        .iter()
        .zip(matched.iter())
        .any(|(pattern, &m)| pattern != "*" && m);
    allowed && (!denied || explicitly_allowed)
}

static GLOB_CACHE: std::sync::Mutex<Vec<(String, Regex)>> = std::sync::Mutex::new(Vec::new());

fn glob_matches(pattern: &str, value: &str) -> bool {
    let mut cache = GLOB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, regex)) = cache.iter().find(|(p, _)| p == pattern) {
        return regex.is_match(value);
    }
    let mut expression = String::with_capacity(pattern.len() + 2);
    expression.push('^');
    let mut literal_buf = String::new();
    let flush_literal = |buf: &mut String, out: &mut String| {
        if !buf.is_empty() {
            out.push_str(&regex::escape(buf));
            buf.clear();
        }
    };
    for ch in pattern.chars() {
        match ch {
            '*' => {
                flush_literal(&mut literal_buf, &mut expression);
                expression.push_str(".*");
            }
            '?' => {
                flush_literal(&mut literal_buf, &mut expression);
                expression.push('.');
            }
            other => {
                literal_buf.push(other);
            }
        }
    }
    flush_literal(&mut literal_buf, &mut expression);
    expression.push('$');
    let regex = match Regex::new(&expression) {
        Ok(re) => re,
        Err(_) => return false,
    };
    let matched = regex.is_match(value);
    cache.push((pattern.to_owned(), regex));
    matched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderRouteKind;
    use std::time::Duration;

    fn model(id: &str, display_name: Option<&str>) -> StaticModelCatalogEntry {
        StaticModelCatalogEntry {
            id: id.to_owned(),
            display_name: display_name.map(str::to_owned),
            supports: vec![ProviderRouteKind::ChatCompletions],
            context_length: None,
        }
    }

    fn config(mode: ProviderCatalogMode) -> ProviderCatalogConfig {
        ProviderCatalogConfig {
            mode,
            enforce: false,
            cache_ttl: Duration::from_secs(60),
            allow: vec!["*".to_owned()],
            deny: Vec::new(),
            models: vec![model("shared", Some("static"))],
        }
    }

    #[test]
    fn hybrid_static_metadata_wins() {
        let merged = merge_catalog(
            &config(ProviderCatalogMode::Hybrid),
            &[model("shared", Some("discovered")), model("other", None)],
        );
        let shared = merged
            .iter()
            .find(|m| m.id == "shared")
            .expect("shared model should be present");
        assert_eq!(shared.display_name.as_deref(), Some("static"));
        let other = merged
            .iter()
            .find(|m| m.id == "other")
            .expect("other model should be present");
        assert!(other.display_name.is_none());
    }

    #[test]
    fn merge_catalog_sorts_output_by_id() {
        let cfg = ProviderCatalogConfig {
            mode: ProviderCatalogMode::Discovered,
            enforce: false,
            cache_ttl: Duration::from_secs(60),
            allow: vec!["*".to_owned()],
            deny: Vec::new(),
            models: Vec::new(),
        };
        let discovered = vec![
            model("deepseek-v3.1", None),
            model("claude-sonnet-4", None),
            model("anthropic/claude-opus", None),
        ];
        let merged = merge_catalog(&cfg, &discovered);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["anthropic/claude-opus", "claude-sonnet-4", "deepseek-v3.1"],
            "merge_catalog must return entries sorted ascending by id"
        );
    }

    #[test]
    fn merge_catalog_sorts_after_static_override() {
        let cfg = ProviderCatalogConfig {
            mode: ProviderCatalogMode::Hybrid,
            enforce: false,
            cache_ttl: Duration::from_secs(60),
            allow: vec!["*".to_owned()],
            deny: Vec::new(),
            models: vec![model("zzz-tail", None), model("aaa-head", None)],
        };
        let discovered = vec![model("mmm-middle", None)];
        let merged = merge_catalog(&cfg, &discovered);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["aaa-head", "mmm-middle", "zzz-tail"]);
    }

    #[test]
    fn deny_filter_excludes_matching_models() {
        assert!(!model_allowed(
            "model-preview",
            &["*".to_owned()],
            &["*-preview".to_owned()]
        ));
    }

    #[test]
    fn explicit_allow_overrides_deny() {
        assert!(model_allowed(
            "model-preview",
            &["*".to_owned(), "model-preview".to_owned()],
            &["*-preview".to_owned()]
        ));
    }

    #[test]
    fn parse_catalog_file_rejects_invalid_toml() {
        let result = parse_catalog_file("this is not valid toml [[");
        assert!(result.is_err(), "expected error for invalid TOML input");
        assert!(
            matches!(result.unwrap_err(), CoreError::ConfigParse(_)),
            "expected CoreError::ConfigParse variant"
        );
    }

    #[test]
    fn discovered_mode_only_uses_discovered_entries() {
        let cfg = config(ProviderCatalogMode::Discovered);
        let merged = merge_catalog(
            &cfg,
            &[model("discovered-a", None), model("discovered-b", None)],
        );
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|m| m.id == "discovered-a"));
        assert!(merged.iter().any(|m| m.id == "discovered-b"));
        assert!(!merged.iter().any(|m| m.id == "shared"));
    }

    #[test]
    fn static_mode_only_uses_static_entries() {
        let cfg = config(ProviderCatalogMode::Static);
        let merged = merge_catalog(
            &cfg,
            &[
                model("discovered-a", None),
                model("shared", Some("discovered")),
            ],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "shared");
        assert_eq!(merged[0].display_name.as_deref(), Some("static"));
    }

    #[test]
    fn glob_matches_special_regex_characters() {
        assert!(glob_matches("model.v2", "model.v2"));
        assert!(!glob_matches("model.v2", "modelXv2"));
        assert!(glob_matches("model+v2", "model+v2"));
        assert!(!glob_matches("model+v2", "modelvv2"));
        assert!(glob_matches("model(test)", "model(test)"));
    }

    #[test]
    fn glob_matches_slashes_in_model_ids() {
        assert!(glob_matches(
            "accounts/fireworks/models/deepseek-v3p1",
            "accounts/fireworks/models/deepseek-v3p1"
        ));
        assert!(!glob_matches(
            "accounts/fireworks/models/deepseek-v3p1",
            "accounts/fireworks/models/other"
        ));
    }

    #[test]
    fn glob_matches_wildcard_patterns() {
        assert!(glob_matches("*-preview", "model-preview"));
        assert!(!glob_matches("*-preview", "model-stable"));
        assert!(glob_matches("model-*", "model-v2"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("model-?", "model-v"));
        assert!(!glob_matches("model-?", "model-v2"));
    }

    #[test]
    fn catalog_file_uses_nested_catalog_model_tables() {
        let file = CatalogFile {
            catalog: CatalogFileMetadata {
                provider: "example".to_owned(),
                source: "live".to_owned(),
                generated_at: "2026-06-09T00:00:00Z".to_owned(),
                models: vec![model("example-model", None)],
            },
        };
        let encoded = toml::to_string(&file).expect("catalog TOML");
        assert!(encoded.contains("[[catalog.models]]"));
        let decoded: CatalogFile = toml::from_str(&encoded).expect("catalog round trip");
        assert_eq!(decoded.catalog.models[0].id, "example-model");
    }
}
