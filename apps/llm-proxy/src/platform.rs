//! Platform-specific helpers for auto-start configuration.
//!
//! Provides plist generation (macOS), desktop entry generation (Linux),
//! and associated escaping utilities.

// Only the macOS plist builder needs the config dir (for the log path); the
// Linux desktop entry carries no absolute paths.
#[cfg(target_os = "macos")]
use crate::paths::config_dir;

/// Generate a macOS launchd plist for auto-start.
///
/// Each argument is placed in its own `<string>` element in the
/// `ProgramArguments` array, avoiding shell interpretation entirely.
/// Argument values are XML-escaped to prevent malformed plists when the
/// executable path contains XML special characters (<, >, &, ", ').
#[cfg(target_os = "macos")]
pub fn format_plist(args: &[String]) -> String {
    let arg_elements: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("\n");
    let log_path = config_dir().join("llm-proxy.log").display().to_string();
    let escaped_log = xml_escape(&log_path);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.llm-proxy</string>
    <key>ProgramArguments</key>
    <array>
{arg_elements}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{escaped_log}</string>
    <key>StandardErrorPath</key>
    <string>{escaped_log}</string>
</dict>
</plist>
"#
    )
}

/// Generate a Linux XDG desktop entry for auto-start.
///
/// Each argument is individually escaped per the Desktop Entry Specification
/// for the `Exec` key, then joined with spaces. This avoids shell injection
/// by properly handling spaces, quotes, dollar signs, backticks, backslashes,
/// and other shell metacharacters in paths.
#[cfg(target_os = "linux")]
pub fn format_desktop_entry(args: &[String]) -> String {
    let escaped_args: String = args
        .iter()
        .map(|a| desktop_exec_escape(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"[Desktop Entry]
Type=Application
Name=LLM Proxy
Exec={escaped_args}
X-GNOME-Autostart-enabled=true
Hidden=false
"#
    )
}

/// Escape a single argument for use in a Desktop Entry `Exec` key.
///
/// Per the XDG Desktop Entry Specification, the following characters must be
/// escaped in Exec values: space, tab, newline, backslash, double-quote,
/// single-quote, dollar, backtick, tilde, hash, percent, ampersand, asterisk,
/// parentheses, vertical bar, semicolon, less-than, greater-than, question mark,
/// square brackets, curly braces.
#[cfg(target_os = "linux")]
pub fn desktop_exec_escape(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    for ch in arg.chars() {
        match ch {
            ' ' => out.push_str("\\s"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\\'"),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            '~' => out.push_str("\\~"),
            '#' => out.push_str("\\#"),
            '%' => out.push_str("\\%"),
            '&' => out.push_str("\\&"),
            '*' => out.push_str("\\*"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '|' => out.push_str("\\|"),
            ';' => out.push_str("\\;"),
            '<' => out.push_str("\\<"),
            '>' => out.push_str("\\>"),
            '?' => out.push_str("\\?"),
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escape a string for safe inclusion in XML content.
///
/// Replaces the five predefined XML entities: `<`, `>`, `&`, `"`, `'`.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escape the regex metacharacters in a literal string.
    ///
    /// Used to build an `insta::Settings::add_filter` regex that matches the
    /// verbatim log path (which contains regex-special characters like `.`)
    /// so it can be redacted to `[log_path]` in snapshots.
    #[cfg(target_os = "macos")]
    fn regex_escape(literal: &str) -> String {
        let mut out = String::with_capacity(literal.len());
        for ch in literal.chars() {
            if "\\.+*?()[]{}|^$".contains(ch) {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    // --- xml_escape tests ---

    #[test]
    fn xml_escape_handles_all_entities() {
        assert_eq!(xml_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn xml_escape_no_escape_needed() {
        assert_eq!(xml_escape("hello world"), "hello world");
    }

    #[test]
    fn xml_escape_xss_vectors() {
        // CDATA injection attempt
        assert_eq!(xml_escape("]]><![CDATA["), "]]&gt;&lt;![CDATA[");
        // Processing instruction injection
        assert_eq!(
            xml_escape("<?xml version='1.0'?>"),
            "&lt;?xml version=&apos;1.0&apos;?&gt;"
        );
        // Script tag injection
        assert_eq!(
            xml_escape("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&apos;xss&apos;)&lt;/script&gt;"
        );
        // Null byte
        assert_eq!(xml_escape("a\x00b"), "a\x00b");
    }

    #[test]
    fn xml_escape_empty_string() {
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn xml_escape_repeated_chars() {
        assert_eq!(xml_escape("<<<"), "&lt;&lt;&lt;");
    }

    // --- format_plist tests (macOS only) ---
    //
    // The full-document test (`format_plist_basic`) snapshots the rendered
    // plist via insta so structural regressions (a dropped `<key>`, a missing
    // `<array>`) fail loudly instead of slipping past substring checks. The
    // generated plist embeds a platform- and home-dependent absolute log path
    // (`config_dir()/llm-proxy.log`), which would otherwise make the snapshot
    // non-reproducible across machines, so the test binds an `insta::Settings`
    // scope that filters the concrete log path down to a `[log_path]`
    // placeholder before snapshotting. The two companion tests below keep
    // targeted `contains()` assertions for the XML-escaping contract
    // (positive/negative containment), which reads more clearly there than a
    // snapshot would.

    #[cfg(target_os = "macos")]
    #[test]
    fn format_plist_basic() {
        let args: Vec<String> = ["/usr/bin/llm-proxy", "serve"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plist = format_plist(&args);

        // Redact the machine-specific log path so the snapshot is portable.
        // `add_filter` takes a regex, so the path's regex metacharacters (e.g.
        // the dots in `llm-proxy.log`) are escaped first.
        let log_path = config_dir().join("llm-proxy.log").display().to_string();
        let filter = format!("({})", regex_escape(&log_path));
        let mut settings = insta::Settings::clone_current();
        settings.add_filter(&filter, "[log_path]");
        settings.bind(|| insta::assert_snapshot!("launchd_plist_basic", plist));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn format_plist_escapes_special_chars() {
        let args: Vec<String> = ["/path/with<special>&chars\"'"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plist = format_plist(&args);

        assert!(plist.contains("&lt;"));
        assert!(plist.contains("&amp;"));
        assert!(plist.contains("&quot;"));
        assert!(plist.contains("&apos;"));

        assert!(!plist.contains("<special>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn format_plist_malformed_path() {
        let args: Vec<String> = ["/path/with spaces/and<special>"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plist = format_plist(&args);

        // XML special characters in the path should be escaped.
        assert!(!plist.contains("<special>"));
        assert!(plist.contains("&lt;special&gt;"));
    }

    // --- desktop_exec_escape tests ---

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_exec_escape_spaces() {
        assert_eq!(
            desktop_exec_escape("/path/with spaces"),
            "/path/with\\sspaces"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_exec_escape_shell_metacharacters() {
        let escaped = desktop_exec_escape("$HOME/`echo test`");
        // Every metacharacter must survive in backslash-escaped form...
        assert!(escaped.contains("\\$"));
        assert!(escaped.contains("\\`"));
        // ...and none may remain unescaped: once the escaped forms are
        // removed, no bare '$' or '`' may be left in the output.
        let unescaped = escaped.replace("\\$", "").replace("\\`", "");
        assert!(
            !unescaped.contains('$') && !unescaped.contains('`'),
            "metacharacters must always be escaped, got: {escaped}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_exec_escape_no_special_chars() {
        assert_eq!(
            desktop_exec_escape("/usr/bin/llm-proxy"),
            "/usr/bin/llm-proxy"
        );
    }

    // --- format_desktop_entry full-document snapshot (Linux only) ---
    //
    // Unlike the plist, the desktop entry contains no platform-dependent
    // absolute paths, so the full rendered document can be snapshotted
    // directly. This catches structural regressions (e.g. a dropped
    // `X-GNOME-Autostart-enabled` key) that substring checks would miss.

    #[cfg(target_os = "linux")]
    #[test]
    fn format_desktop_entry_basic() {
        let args: Vec<String> = ["/usr/bin/llm-proxy", "serve"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let entry = format_desktop_entry(&args);
        insta::assert_snapshot!("desktop_entry_basic", entry);
    }
}
