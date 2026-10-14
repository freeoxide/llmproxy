//! Shared utility functions for the protocol crate.
//!
//! Contains small helper functions used internally by the protocol adapters.

/// Truncate `s` to at most `max_bytes` bytes on a UTF-8 char boundary,
/// returning it unchanged if it fits; the result may be up to 3 bytes
/// shorter than `max_bytes`. Never panics on multi-byte input.
pub fn truncate_str_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        s
    } else {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

/// Truncate a string to `max_len` bytes, appending `suffix` if truncation
/// occurs (suffix included in the budget, UTF-8 boundaries respected). If
/// `suffix.len() >= max_len`, the original string is returned untruncated.
pub fn truncate_with_suffix(s: &str, max_len: usize, suffix: &str) -> String {
    if s.len() <= max_len {
        return s.to_owned();
    }
    if suffix.len() >= max_len {
        return s.to_owned();
    }
    let max_content = max_len - suffix.len();
    let mut end = max_content;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}{}", &s[..end], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_safe_short_enough() {
        assert_eq!(truncate_str_safe("hello", 10), "hello");
        assert_eq!(truncate_str_safe("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_safe_truncates_ascii() {
        assert_eq!(truncate_str_safe("hello world", 5), "hello");
        assert_eq!(truncate_str_safe("abcdef", 3), "abc");
    }

    #[test]
    fn truncate_str_safe_respects_utf8_boundaries() {
        let input = "éééééé";
        let result = truncate_str_safe(input, 7);
        assert!(result.len() <= 7);
        assert!(result.is_char_boundary(result.len()));
        assert_eq!(result, "ééé");
    }

    #[test]
    fn truncate_str_safe_empty_and_zero_budget() {
        assert_eq!(truncate_str_safe("", 5), "");
        assert_eq!(truncate_str_safe("hello", 0), "");
        assert_eq!(truncate_str_safe("", 0), "");
    }

    #[test]
    fn truncate_str_safe_multibyte_boundary_split() {
        let input = "αβγδε";
        assert_eq!(truncate_str_safe(input, 3), "α");
    }

    #[test]
    fn no_truncation_when_short_enough() {
        assert_eq!(truncate_with_suffix("hello", 10, "..."), "hello");
    }

    #[test]
    fn truncates_with_suffix() {
        assert_eq!(truncate_with_suffix("hello world!", 8, "..."), "hello...");
    }

    #[test]
    fn respects_utf8_boundaries() {
        let input = "éééééé";
        let result = truncate_with_suffix(input, 7, "...");
        assert!(result.len() <= 7);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn suffix_equals_max_len_returns_original() {
        assert_eq!(truncate_with_suffix("hello", 3, "..."), "hello");
    }

    #[test]
    fn suffix_exceeds_max_len_returns_original() {
        assert_eq!(truncate_with_suffix("hello", 2, "..."), "hello");
    }

    #[test]
    fn empty_input_with_suffix() {
        assert_eq!(truncate_with_suffix("", 5, "..."), "");
    }

    #[test]
    fn empty_suffix_truncates_cleanly() {
        assert_eq!(truncate_with_suffix("hello world", 5, ""), "hello");
    }

    #[test]
    fn exactly_at_max_len_no_truncation() {
        assert_eq!(truncate_with_suffix("hello", 5, "..."), "hello");
    }

    #[test]
    fn max_len_equals_suffix_len_plus_one() {
        assert_eq!(truncate_with_suffix("abcdef", 4, "..."), "a...");
    }

    #[test]
    fn utf8_boundary_backup_produces_valid_string() {
        let input = "αβγδε";
        let result = truncate_with_suffix(input, 7, "...");
        assert!(result.len() <= 7);
        assert!(result.ends_with("..."));
        let content = &result[..result.len() - 3];
        assert!(content.is_char_boundary(content.len()));
    }
}
