//! The built-in pattern rule: one regex, one action per scope.

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;

use crate::{GuardRule, GuardVerdict, RuleError};

/// What the rule does on a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    /// Refuse the request or response.
    Deny,
    /// Replace the match with a placeholder (masking guards).
    Mask {
        /// The placeholder a match becomes, for example `[REDACTED]`.
        placeholder: String,
    },
}

/// Which texts the rule sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    /// Request texts only (prompts).
    Requests,
    /// Response texts only.
    Responses,
    /// Both sides.
    Both,
}

/// A regex rule: matches on request texts, response texts or both, and
/// denies or masks.
#[derive(Debug, Clone)]
pub struct PatternRule {
    pattern: Arc<Regex>,
    action: RuleAction,
    scope: RuleScope,
    reason: String,
}

impl PatternRule {
    /// A deny rule matching `pattern` on both sides, rejecting with
    /// `reason`.
    ///
    /// # Errors
    ///
    /// [`RuleError`] when the pattern does not compile.
    pub fn deny(pattern: &str, reason: &str) -> Result<Self, RuleError> {
        Self::new(pattern, RuleAction::Deny, RuleScope::Both, reason)
    }

    /// A mask rule replacing matches on both sides with `placeholder`.
    ///
    /// # Errors
    ///
    /// [`RuleError`] when the pattern does not compile.
    pub fn mask(pattern: &str, placeholder: &str) -> Result<Self, RuleError> {
        Self::new(
            pattern,
            RuleAction::Mask {
                placeholder: placeholder.to_owned(),
            },
            RuleScope::Both,
            "masked by pattern rule",
        )
    }

    /// A rule with every knob.
    ///
    /// # Errors
    ///
    /// [`RuleError`] when the pattern does not compile.
    pub fn new(
        pattern: &str,
        action: RuleAction,
        scope: RuleScope,
        reason: &str,
    ) -> Result<Self, RuleError> {
        Ok(Self {
            pattern: Arc::new(
                Regex::new(pattern)
                    .map_err(|error| RuleError(format!("pattern {pattern:?}: {error}")))?,
            ),
            action,
            scope,
            reason: reason.to_owned(),
        })
    }

    fn verdict(&self, text: &str) -> GuardVerdict {
        match &self.action {
            RuleAction::Deny => {
                if self.pattern.is_match(text) {
                    GuardVerdict::Reject(self.reason.clone())
                } else {
                    GuardVerdict::Allow
                }
            }
            RuleAction::Mask { placeholder } => {
                if self.pattern.is_match(text) {
                    GuardVerdict::Mask(
                        self.pattern
                            .replace_all(text, placeholder.as_str())
                            .into_owned(),
                    )
                } else {
                    GuardVerdict::Allow
                }
            }
        }
    }
}

#[async_trait]
impl GuardRule for PatternRule {
    async fn check_request_text(&self, text: &str) -> Result<GuardVerdict, RuleError> {
        Ok(match self.scope {
            RuleScope::Responses => GuardVerdict::Allow,
            _ => self.verdict(text),
        })
    }

    async fn check_response_text(&self, text: &str) -> Result<GuardVerdict, RuleError> {
        Ok(match self.scope {
            RuleScope::Requests => GuardVerdict::Allow,
            _ => self.verdict(text),
        })
    }
}
