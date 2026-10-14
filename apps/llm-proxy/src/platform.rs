//! Platform-specific helpers for auto-start configuration.
//!
//! Provides plist generation (macOS), desktop entry generation (Linux),
//! and associated escaping utilities.

#[cfg(target_os = "macos")]
use crate::paths::config_dir;

/// Generate a macOS launchd plist for auto-start.
///
/// Each argument becomes its own XML-escaped `<string>` in `ProgramArguments`,
/// avoiding shell interpretation entirely.
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
/// Each argument is escaped per the Desktop Entry Specification for the `Exec`
/// key, avoiding shell interpretation entirely.
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

/// Escape a single argument for use in a Desktop Entry `Exec` key, per the
/// XDG Desktop Entry Specification.
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
        assert_eq!(xml_escape("]]><![CDATA["), "]]&gt;&lt;![CDATA[");
        assert_eq!(
            xml_escape("<?xml version='1.0'?>"),
            "&lt;?xml version=&apos;1.0&apos;?&gt;"
        );
        assert_eq!(
            xml_escape("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&apos;xss&apos;)&lt;/script&gt;"
        );
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

    #[cfg(target_os = "macos")]
    #[test]
    fn format_plist_basic() {
        let args: Vec<String> = ["/usr/bin/llm-proxy", "serve"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plist = format_plist(&args);

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

        assert!(!plist.contains("<special>"));
        assert!(plist.contains("&lt;special&gt;"));
    }

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
        assert!(escaped.contains("\\$"));
        assert!(escaped.contains("\\`"));
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
