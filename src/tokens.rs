//! Token counting.
//!
//! Two sources, in this order of preference:
//!
//! 1. **Provider-reported usage** ([`TokenUsage`], populated from the
//!    response after each round-trip). Authoritative for the call that
//!    just completed.
//! 2. **Local cl100k_base estimate** ([`count`], via `tiktoken-rs`'s
//!    lazy singleton). Used pre-flight — composer context indicator,
//!    auto-title threshold gate, anywhere we need a number before the
//!    next inference returns.
//!
//! The user-facing contract for any token-budget enforcement remains
//! "≈" — exactness is not promised across providers.

use tiktoken_rs::cl100k_base_singleton;

/// Count tokens in `text` using cl100k_base.
pub fn count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    cl100k_base_singleton().encode_with_special_tokens(text).len()
}

/// Per-call provider-reported token usage. Mirrors the columns we
/// persist into `inference_calls`; rig surfaces more fields (e.g.
/// `cache_creation_input_tokens`) that we don't store yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

impl TokenUsage {
    /// Sum of input + output. Useful for the TUI "current context"
    /// affordance, which wants a single number.
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// `true` if the provider reported nothing meaningful (rig signals
    /// this by leaving every field at 0 — see `rig::completion::Usage`
    /// docs).
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0 && self.cached_input_tokens == 0
    }
}

impl From<rig::completion::Usage> for TokenUsage {
    fn from(u: rig::completion::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_input_tokens: u.cached_input_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_zero() {
        assert_eq!(count(""), 0);
    }

    #[test]
    fn hello_world_is_a_few_tokens() {
        let n = count("Hello, world!");
        assert!((1..=10).contains(&n), "got {n}");
    }

    #[test]
    fn longer_text_is_more_tokens_than_short() {
        let short = count("hi");
        let long = count("The quick brown fox jumps over the lazy dog.");
        assert!(long > short);
    }
}
