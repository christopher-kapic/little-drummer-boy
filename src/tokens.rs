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

use tiktoken_rs::{
    cl100k_base_singleton, o200k_base_singleton, p50k_base_singleton, p50k_edit_singleton,
    r50k_base_singleton,
};

/// Count tokens in `text` using cl100k_base — the documented global
/// default / fallback (GOALS §10).
pub fn count(text: &str) -> usize {
    count_with(text, TokenizerStrategy::Cl100k)
}

/// A tiktoken encoding strategy. Per-`(provider, model)` calibration
/// picks whichever of these best matches the provider's reported
/// counts; `Cl100k` is the floor when nothing is calibrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerStrategy {
    R50k,
    P50k,
    P50kEdit,
    Cl100k,
    O200k,
}

/// Every strategy, in a fixed order — the calibration loop tries each.
pub const STRATEGIES: [TokenizerStrategy; 5] = [
    TokenizerStrategy::R50k,
    TokenizerStrategy::P50k,
    TokenizerStrategy::P50kEdit,
    TokenizerStrategy::Cl100k,
    TokenizerStrategy::O200k,
];

impl TokenizerStrategy {
    /// The string persisted in `tokenizer_calibration.strategy`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::R50k => "r50k_base",
            Self::P50k => "p50k_base",
            Self::P50kEdit => "p50k_edit",
            Self::Cl100k => "cl100k_base",
            Self::O200k => "o200k_base",
        }
    }

    /// Parse a persisted strategy name; unknown names fall back to the
    /// cl100k_base floor rather than erroring.
    pub fn from_name(name: &str) -> Self {
        match name {
            "r50k_base" => Self::R50k,
            "p50k_base" => Self::P50k,
            "p50k_edit" => Self::P50kEdit,
            "o200k_base" => Self::O200k,
            _ => Self::Cl100k,
        }
    }
}

/// Count tokens in `text` with a specific [`TokenizerStrategy`].
pub fn count_with(text: &str, strategy: TokenizerStrategy) -> usize {
    if text.is_empty() {
        return 0;
    }
    let bpe = match strategy {
        TokenizerStrategy::R50k => r50k_base_singleton(),
        TokenizerStrategy::P50k => p50k_base_singleton(),
        TokenizerStrategy::P50kEdit => p50k_edit_singleton(),
        TokenizerStrategy::Cl100k => cl100k_base_singleton(),
        TokenizerStrategy::O200k => o200k_base_singleton(),
    };
    bpe.encode_with_special_tokens(text).len()
}

/// Apply a calibrated `(strategy, scale)` to `text`: `count_with * scale`,
/// rounded. This is the model-aware estimate once a calibration row (or
/// the `(cl100k, 1.0)` default) has been resolved.
pub fn scaled_estimate(text: &str, strategy: TokenizerStrategy, scale: f64) -> u64 {
    let raw = count_with(text, strategy) as f64 * scale;
    raw.round().max(0.0) as u64
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

// ---- per-model tokenizer calibration ---------------------------------------

/// Close the calibration window once cumulative actual tokens reach this
/// and the call-count floor is met. The floor stops one giant outlier
/// call from deciding the fit. Both tunable.
pub const CALIBRATION_TOKEN_TARGET: u64 = 20_000;
pub const CALIBRATION_MIN_CALLS: usize = 5;

/// One sampled inference call: the provider's `input + output` total and
/// the estimate each strategy produced for the same text basis.
#[derive(Debug, Clone)]
struct CalSample {
    actual: u64,
    ests: [usize; STRATEGIES.len()],
}

/// Accumulates inference samples in memory (per session, never
/// persisted in-progress) and, once the window closes, picks the
/// strategy with the lowest mean *relative* error and the scale factor
/// that maps its estimate onto the provider's real count. See GOALS §15
/// / the calibration spec.
#[derive(Debug, Clone, Default)]
pub struct Calibrator {
    samples: Vec<CalSample>,
    cumulative_actual: u64,
}

impl Calibrator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a sample: estimate `basis` under every strategy and record it
    /// against the provider's `actual` (= input + output) tokens.
    pub fn add_sample(&mut self, basis: &str, actual: u64) {
        let ests = STRATEGIES.map(|s| count_with(basis, s));
        self.cumulative_actual = self.cumulative_actual.saturating_add(actual);
        self.samples.push(CalSample { actual, ests });
    }

    /// True once enough volume has accumulated to trust a fit.
    pub fn window_closed(&self) -> bool {
        self.cumulative_actual >= CALIBRATION_TOKEN_TARGET
            && self.samples.len() >= CALIBRATION_MIN_CALLS
    }

    pub fn sample_calls(&self) -> usize {
        self.samples.len()
    }

    pub fn cumulative_actual(&self) -> u64 {
        self.cumulative_actual
    }

    /// Compute the fit: `argmin_s mean_i |est - actual| / actual` (mean
    /// relative error, so big calls don't dominate), then
    /// `scale = mean_i actual / est_chosen` so `est * scale ≈ actual`.
    /// `None` when there are no samples. Estimates of 0 are clamped to 1
    /// to keep the ratios finite.
    pub fn result(&self) -> Option<(TokenizerStrategy, f64)> {
        if self.samples.is_empty() {
            return None;
        }
        let n = self.samples.len() as f64;
        let mut best: Option<(usize, f64)> = None;
        for si in 0..STRATEGIES.len() {
            let mut sum_rel = 0.0;
            for s in &self.samples {
                let est = s.ests[si].max(1) as f64;
                let actual = s.actual.max(1) as f64;
                sum_rel += (est - actual).abs() / actual;
            }
            let mean_rel = sum_rel / n;
            if best.is_none_or(|(_, b)| mean_rel < b) {
                best = Some((si, mean_rel));
            }
        }
        let (chosen, _) = best?;
        let mut sum_scale = 0.0;
        for s in &self.samples {
            let est = s.ests[chosen].max(1) as f64;
            sum_scale += s.actual as f64 / est;
        }
        Some((STRATEGIES[chosen], sum_scale / n))
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
    fn count_with_matches_default_cl100k() {
        let t = "The quick brown fox jumps over the lazy dog.";
        assert_eq!(count(t), count_with(t, TokenizerStrategy::Cl100k));
    }

    #[test]
    fn strategy_name_round_trips() {
        for s in STRATEGIES {
            assert_eq!(TokenizerStrategy::from_name(s.as_str()), s);
        }
        // Unknown names fall back to the cl100k floor.
        assert_eq!(
            TokenizerStrategy::from_name("bogus"),
            TokenizerStrategy::Cl100k
        );
    }

    #[test]
    fn calibrator_window_needs_volume_and_calls() {
        let mut c = Calibrator::new();
        c.add_sample("hello world", 100_000); // one giant call
        assert!(!c.window_closed(), "call-count floor not met");
        for _ in 0..5 {
            c.add_sample("hello world", 1);
        }
        assert!(c.window_closed());
    }

    #[test]
    fn calibrator_picks_lowest_relative_error_strategy() {
        // Synthesize samples whose actual equals the cl100k count, so
        // cl100k has zero relative error and must win with scale ≈ 1.
        let texts = [
            "fn main() { println!(\"hello\"); }",
            "The quick brown fox jumps over the lazy dog, repeatedly.",
            "lorem ipsum dolor sit amet consectetur adipiscing elit",
            "alpha beta gamma delta epsilon zeta eta theta iota kappa",
            "one two three four five six seven eight nine ten eleven",
        ];
        let mut c = Calibrator::new();
        for t in texts {
            let actual = count_with(t, TokenizerStrategy::Cl100k) as u64;
            c.add_sample(t, actual);
        }
        let (strategy, scale) = c.result().expect("samples present");
        assert_eq!(strategy, TokenizerStrategy::Cl100k);
        assert!(
            (scale - 1.0).abs() < 1e-9,
            "scale should be ~1.0, got {scale}"
        );
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
