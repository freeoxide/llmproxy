//! Token counter with model-aware BPE dispatch.
//!
//! [`Counter`] is the public front door for token counting: a real BPE
//! tokenizer (via [`super::tiktoken`]) for known OpenAI model ids, a
//! character heuristic (~4 chars/token) for everything else. BPE ranks are
//! bundled at compile time, so counting is offline-safe.
//!
//! The per-message overhead mirrors `tiktoken-rs`'s
//! `num_tokens_from_messages` convention (4 tokens/message for the `gpt-3.5`
//! family, 3 otherwise, plus a trailing +3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::token::tiktoken::{self, Encoding, HeuristicTokenizer, TiktokenTokenizer, Tokenizer};

/// A single chat message with a role and content string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageContent {
    /// The role of the message author (e.g. `"user"`, `"assistant"`, `"system"`).
    /// Validation is the caller's responsibility.
    pub role: String,
    /// The text content of the message.
    pub content: String,
}

impl MessageContent {
    /// Create a new message from a role and content string.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// Model-aware token counter.
///
/// Holds a heuristic backend plus a lazily-populated cache of BPE tokenizers
/// keyed by [`Encoding`]; the cache uses interior mutability so a `&Counter`
/// can load encodings on first use. Cloning a `Counter` shares the cache.
#[derive(Clone, Default)]
pub struct Counter {
    heuristic: HeuristicTokenizer,
    cache: Arc<Mutex<HashMap<Encoding, Arc<TiktokenTokenizer>>>>,
}

impl std::fmt::Debug for Counter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.cache.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("Counter")
            .field("backend", &"bpe+heuristic")
            .field("loaded_encodings", &loaded)
            .finish()
    }
}

impl Counter {
    /// Create a new counter with an empty BPE cache (encodings load lazily).
    pub fn new() -> Self {
        Self::default()
    }

    /// Count tokens in `text` using the model's BPE encoding when the model is
    /// known, otherwise the heuristic.
    ///
    /// BPE encodings load lazily on first use for a known model and are cached
    /// for subsequent calls. If an encoding cannot be loaded (offline, corrupt
    /// ranks), the heuristic is used so a request never fails on the tokenizer.
    pub fn count_tokens(&self, model: &str, text: &str) -> usize {
        match tiktoken::encoding_for_model(model) {
            Some(encoding) => match self.bpe_for(encoding) {
                Some(tokenizer) => tokenizer.count_tokens(text),
                None => self.heuristic.count_tokens(text),
            },
            None => self.heuristic.count_tokens(text),
        }
    }

    /// Get the cached BPE tokenizer for an encoding, loading it lazily on first
    /// use. Returns `None` only if the ranks could not be loaded.
    fn bpe_for(&self, encoding: Encoding) -> Option<Arc<TiktokenTokenizer>> {
        if let Ok(cache) = self.cache.lock() {
            if let Some(tokenizer) = cache.get(&encoding).cloned() {
                return Some(tokenizer);
            }
        }
        let tokenizer = Arc::new(TiktokenTokenizer::new(encoding.load()?));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(encoding, Arc::clone(&tokenizer));
        }
        Some(tokenizer)
    }

    /// Estimate the total token count for a chat completion request.
    ///
    /// Follows `tiktoken-rs`'s `num_tokens_from_messages` convention:
    ///
    /// ```text
    /// tokens_per_message = if model.starts_with("gpt-3.5") { 4 } else { 3 }
    /// total = sum over (system-as-one-message + each message) of
    ///           (tokens_per_message + count_tokens(role) + count_tokens(content))
    ///       + 3   // trailing priming tokens
    /// ```
    ///
    /// The system prompt is folded into the message loop as a single
    /// `"system"`-role message when it is non-empty; when `system` is empty no
    /// per-message overhead is added for it.
    ///
    /// `system` is a flat string: if the upstream protocol supports multiple
    /// system blocks, the caller must pre-concatenate them. Role validation is
    /// the caller's responsibility.
    pub fn count_messages(&self, model: &str, system: &str, messages: &[MessageContent]) -> usize {
        const GPT_35_PER_MESSAGE: usize = 4;
        const DEFAULT_PER_MESSAGE: usize = 3;
        const TRAILING_TOKENS: usize = 3;

        let tokens_per_message = if model.starts_with("gpt-3.5") {
            GPT_35_PER_MESSAGE
        } else {
            DEFAULT_PER_MESSAGE
        };

        let system_overhead = if system.is_empty() {
            0
        } else {
            tokens_per_message
                + self.count_tokens(model, "system")
                + self.count_tokens(model, system)
        };

        let message_tokens: usize = messages
            .iter()
            .map(|msg| {
                tokens_per_message
                    + self.count_tokens(model, &msg.role)
                    + self.count_tokens(model, &msg.content)
            })
            .sum();

        system_overhead + message_tokens + TRAILING_TOKENS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEURISTIC_MODEL: &str = "unknown-test-model";

    #[test]
    fn count_tokens_empty_string() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, ""), 0);
    }

    #[test]
    fn count_tokens_single_char() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "a"), 1);
    }

    #[test]
    fn count_tokens_short_text() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "hello"), 1);
    }

    #[test]
    fn count_tokens_medium_text() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, &"a".repeat(20)), 5);
    }

    #[test]
    fn count_tokens_longer_text() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, &"a".repeat(100)), 25);
    }

    #[test]
    fn count_messages_no_messages() {
        let counter = Counter::new();
        let total = counter.count_messages(HEURISTIC_MODEL, "test", &[]);
        assert_eq!(total, 8);
    }

    #[test]
    fn count_messages_empty_system() {
        let counter = Counter::new();
        let total = counter.count_messages(HEURISTIC_MODEL, "", &[]);
        assert_eq!(total, 3);
    }

    #[test]
    fn count_messages_single_user_message() {
        let counter = Counter::new();
        let messages = vec![MessageContent::new("user", "hello")];
        let total = counter.count_messages(HEURISTIC_MODEL, "", &messages);
        assert_eq!(total, 8);
    }

    #[test]
    fn count_messages_multiple_messages() {
        let counter = Counter::new();
        let messages = vec![
            MessageContent::new("system", "You are a helpful assistant."),
            MessageContent::new("user", "Hello, how are you?"),
            MessageContent::new("assistant", "I'm doing well, thank you!"),
        ];
        let total = counter.count_messages(HEURISTIC_MODEL, "Be helpful", &messages);
        assert_eq!(total, 39);
    }

    #[test]
    fn count_messages_with_system_prompt() {
        let counter = Counter::new();
        let messages = vec![MessageContent::new("user", "Hi")];
        let total = counter.count_messages(HEURISTIC_MODEL, "You are helpful", &messages);
        assert_eq!(total, 15);
    }

    #[test]
    fn message_content_new() {
        let msg = MessageContent::new("user", "test content");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "test content");
    }

    #[test]
    fn counter_default_is_valid() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "hello"), 1);
    }

    #[test]
    fn count_messages_empty_role() {
        let counter = Counter::new();
        let messages = vec![MessageContent::new("", "hello")];
        let total = counter.count_messages(HEURISTIC_MODEL, "", &messages);
        assert_eq!(total, 7);
    }

    #[test]
    fn count_messages_empty_content() {
        let counter = Counter::new();
        let messages = vec![MessageContent::new("user", "")];
        let total = counter.count_messages(HEURISTIC_MODEL, "", &messages);
        assert_eq!(total, 7);
    }

    #[test]
    fn count_tokens_whitespace_only() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "   "), 1);
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "\t\n"), 1);
    }

    #[test]
    fn count_tokens_multibyte_unicode() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "\u{1F600}"), 1);
        assert_eq!(
            counter.count_tokens(HEURISTIC_MODEL, "\u{4F60}\u{597D}\u{4E16}\u{754C}"),
            1
        );
        let cjk_8: String =
            "\u{4F60}\u{597D}\u{4E16}\u{754C}\u{4F60}\u{597D}\u{4E16}\u{754C}".to_owned();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, &cjk_8), 2);
    }

    #[test]
    fn count_tokens_mixed_ascii_unicode() {
        let counter = Counter::new();
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, "hello\u{1F600}"), 1);
    }

    #[test]
    fn count_tokens_unknown_model_uses_heuristic() {
        let counter = Counter::new();
        let text = "The quick brown fox jumps over the lazy dog.";
        let heuristic = (text.chars().count() / 4).max(1);
        assert_eq!(counter.count_tokens(HEURISTIC_MODEL, text), heuristic);
    }

    #[test]
    fn count_tokens_known_model_uses_bpe() {
        let counter = Counter::new();
        let text = "The quick brown fox jumps over the lazy dog.";

        let o200k = tiktoken_rs::o200k_base().expect("o200k_base loads");
        assert_eq!(
            counter.count_tokens("gpt-4o", text),
            o200k.encode_with_special_tokens(text).len()
        );

        let cl100k = tiktoken_rs::cl100k_base().expect("cl100k_base loads");
        assert_eq!(
            counter.count_tokens("gpt-4", text),
            cl100k.encode_with_special_tokens(text).len()
        );

        let heuristic = (text.chars().count() / 4).max(1);
        assert_ne!(
            counter.count_tokens("gpt-4o", text),
            heuristic,
            "BPE count should differ from the heuristic for this fixture"
        );
    }

    #[test]
    fn count_tokens_bpe_encoding_is_cached() {
        let counter = Counter::new();
        let _ = counter.count_tokens("gpt-4o", "hello world");
        let _ = counter.count_tokens("gpt-4o-2024-08-06", "another");
        let loaded = counter.cache.lock().unwrap().len();
        assert_eq!(
            loaded, 1,
            "gpt-4o and gpt-4o-2024-08-06 share the o200k_base encoding"
        );

        let _ = counter.count_tokens("gpt-4", "third");
        let loaded = counter.cache.lock().unwrap().len();
        assert_eq!(loaded, 2, "gpt-4 adds the cl100k_base encoding");
    }

    fn golden_messages() -> Vec<MessageContent> {
        vec![
            MessageContent::new(
                "system",
                "You are a helpful assistant that only speaks French.",
            ),
            MessageContent::new("user", "Hello, how are you?"),
            MessageContent::new("assistant", "Parlez-vous francais?"),
        ]
    }

    fn golden_tiktoken_messages() -> Vec<tiktoken_rs::ChatCompletionRequestMessage> {
        use tiktoken_rs::ChatCompletionRequestMessage;
        vec![
            ChatCompletionRequestMessage {
                role: "system".to_owned(),
                content: Some("You are a helpful assistant that only speaks French.".to_owned()),
                name: None,
                function_call: None,
            },
            ChatCompletionRequestMessage {
                role: "user".to_owned(),
                content: Some("Hello, how are you?".to_owned()),
                name: None,
                function_call: None,
            },
            ChatCompletionRequestMessage {
                role: "assistant".to_owned(),
                content: Some("Parlez-vous francais?".to_owned()),
                name: None,
                function_call: None,
            },
        ]
    }

    const GOLDEN_SYSTEM: &str = "Be concise and correct.";

    #[test]
    fn count_messages_matches_tiktoken_for_gpt4o() {
        let counter = Counter::new();
        let messages = golden_messages();
        let tiktoken_msgs = golden_tiktoken_messages();

        let ours = counter.count_messages("gpt-4o", GOLDEN_SYSTEM, &messages);
        let theirs = tiktoken_rs::num_tokens_from_messages("gpt-4o", &tiktoken_msgs)
            .expect("gpt-4o resolves to a supported chat tokenizer");

        let mut with_system = golden_tiktoken_messages();
        with_system.insert(
            0,
            tiktoken_rs::ChatCompletionRequestMessage {
                role: "system".to_owned(),
                content: Some(GOLDEN_SYSTEM.to_owned()),
                name: None,
                function_call: None,
            },
        );
        let theirs_with_system = tiktoken_rs::num_tokens_from_messages("gpt-4o", &with_system)
            .expect("gpt-4o resolves to a supported chat tokenizer");

        assert!(
            (ours as isize - theirs_with_system as isize).abs() <= 3,
            "gpt-4o mismatch: ours={ours}, tiktoken_baseline_no_system={theirs}, \
             tiktoken_with_system={theirs_with_system}"
        );
        assert!(
            ours > theirs,
            "gpt-4o: adding a system prompt must increase the count (ours={ours}, baseline={theirs})"
        );
    }

    #[test]
    fn count_messages_matches_tiktoken_for_gpt35_turbo() {
        let counter = Counter::new();
        let messages = golden_messages();

        let mut with_system = golden_tiktoken_messages();
        with_system.insert(
            0,
            tiktoken_rs::ChatCompletionRequestMessage {
                role: "system".to_owned(),
                content: Some(GOLDEN_SYSTEM.to_owned()),
                name: None,
                function_call: None,
            },
        );
        let theirs = tiktoken_rs::num_tokens_from_messages("gpt-3.5-turbo", &with_system)
            .expect("gpt-3.5-turbo resolves to a supported chat tokenizer");
        let ours = counter.count_messages("gpt-3.5-turbo", GOLDEN_SYSTEM, &messages);

        assert!(
            (ours as isize - theirs as isize).abs() <= 3,
            "gpt-3.5-turbo mismatch: ours={ours}, tiktoken={theirs}"
        );
    }

    #[test]
    fn count_messages_gpt35_overhead_is_higher_than_gpt4() {
        let counter = Counter::new();
        let messages = vec![MessageContent::new("user", "Hello, how are you today?")];

        let gpt35 = counter.count_messages("gpt-3.5-turbo", "Be helpful", &messages);
        let gpt4 = counter.count_messages("gpt-4", "Be helpful", &messages);
        assert_eq!(
            gpt35 - gpt4,
            2,
            "gpt-3.5 uses 4 tokens/message vs gpt-4's 3; with 2 messages the gap is exactly 2"
        );
    }

    #[test]
    fn counter_is_clone_and_debug() {
        let counter = Counter::new();
        let cloned = counter.clone();
        let s = format!("{counter:?}");
        assert!(s.contains("bpe+heuristic"), "debug output: {s}");
        assert!(format!("{cloned:?}").contains("bpe+heuristic"));
    }

    #[test]
    fn counter_is_not_zero_sized() {
        assert!(
            std::mem::size_of::<Counter>() > 0,
            "Counter must not be zero-sized; it carries an Arc<Mutex<HashMap<..>>>"
        );
    }
}
