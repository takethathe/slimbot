//! Context pressure: one absolute anchor, every length limit derived from it.
//!
//! The provider's context window (in tokens) is the **only** absolute number in
//! the system. Every other length limit is a percentage of that window,
//! converted to characters through the session's `char_per_token_ratio`
//! (token-flavoured percentages, so switching language or provider does not
//! move them by a factor of two) and clamped to a sane absolute range.
//!
//! This module only **measures and judges**: it derives limits and answers
//! questions about them. The actions stay where they belong (AfterTool
//! truncation, PreChat residual cleanup and snip, post-run consolidation,
//! Dream's disk sweep), and there is deliberately no central dispatcher.
//! See `docs/adr/0013-context-pressure-single-anchor.md`.

use crate::config::{Config, PressureConfig};

/// Cold-start chars-per-token ratio, used until a session has observed usage.
///
/// Deliberately conservative (the previous value was 4.0): before the first
/// run there is no usage to learn from, and over-trimming is cheaper than
/// sending a prompt that turns out to be too large for the window.
pub const DEFAULT_CHAR_PER_TOKEN_RATIO: f64 = 3.0;

/// Window assumed when no provider declares one.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 32_768;

/// Absolute clamps for a single tool result (chars).
pub const MIN_SINGLE_TOOL_RESULT_CHARS: usize = 1_000;
pub const MAX_SINGLE_TOOL_RESULT_CHARS: usize = 64_000;

/// Absolute clamps for the persisted-result preview (chars).
pub const MIN_PERSISTED_PREVIEW_CHARS: usize = 256;
pub const MAX_PERSISTED_PREVIEW_CHARS: usize = 8_000;

/// Absolute clamps for the snip safety margin (tokens).
pub const MIN_SNIP_SAFETY_TOKENS: u32 = 128;
pub const MAX_SNIP_SAFETY_TOKENS: u32 = 8_192;

/// Absolute clamps for a consolidation summary (chars).
pub const MIN_SUMMARY_CHARS: usize = 256;
pub const MAX_SUMMARY_CHARS: usize = 8_192;

/// The single window anchor plus the ratio table that hangs off it.
///
/// `Default` is the historical window (32k) with the documented default
/// percentages, which keeps unit tests and provider-less code paths sane.
#[derive(Clone, Debug)]
pub struct PressureAnchor {
    /// The provider's context window in tokens — the only absolute number.
    pub window_tokens: u32,
    /// Percentage table (all limits derive from these).
    pub config: PressureConfig,
}

impl Default for PressureAnchor {
    fn default() -> Self {
        Self {
            window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            config: PressureConfig::default(),
        }
    }
}

impl PressureAnchor {
    /// Anchor an explicit window with an explicit ratio table.
    pub fn new(window_tokens: u32, config: PressureConfig) -> Self {
        Self {
            window_tokens,
            config,
        }
    }

    /// Anchor an explicit window with the default ratio table.
    pub fn from_window(window_tokens: u32) -> Self {
        Self {
            window_tokens,
            config: PressureConfig::default(),
        }
    }

    /// Anchor from a loaded config: the window is a property of the provider
    /// referenced by `agent.provider`, not of the agent.
    pub fn from_config(config: &Config) -> Self {
        let window_tokens = config
            .providers
            .get(&config.agent.provider)
            .map(|p| p.context_window_tokens)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        Self {
            window_tokens,
            config: config.pressure.clone(),
        }
    }

    /// Derive every limit for a session with the given chars-per-token ratio.
    pub fn limits(&self, char_per_token_ratio: f64) -> PressureLimits {
        PressureLimits::derive(self.window_tokens, char_per_token_ratio, &self.config)
    }
}

/// All limits derived from one window anchor, ready for the enforcement points.
#[derive(Clone, Debug)]
pub struct PressureLimits {
    /// The anchor this was derived from (tokens).
    pub window_tokens: u32,
    /// Effective chars-per-token ratio (cold-start default when unknown).
    pub char_per_token_ratio: f64,
    /// A single tool result may take at most this many chars in context.
    pub single_tool_result_chars: usize,
    /// Preview length of an oversized result that was persisted to disk.
    pub persisted_preview_chars: usize,
    /// Headroom reserved for tokenizer estimation drift (tokens).
    pub snip_safety_tokens: u32,
    /// Prompt tokens above which consolidation runs (authoritative usage track).
    pub consolidate_trigger_tokens: u32,
    /// Token level consolidation aims for (a fraction of the trigger point).
    pub consolidate_target_tokens: u32,
    /// Maximum summary length in chars.
    pub summary_max_chars: usize,
    /// Floor on how many non-system messages snip keeps.
    pub min_keep_messages: usize,
    /// How many recent tool results residual cleanup leaves untouched.
    pub keep_recent_tool_results: usize,
}

impl Default for PressureLimits {
    fn default() -> Self {
        PressureAnchor::default().limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
    }
}

impl PressureLimits {
    /// Derive the limits for one window and one ratio.
    ///
    /// A zero window means "unknown anchor": every derived length is zero so
    /// callers keep their "no limit known, do not act" behaviour.
    pub fn derive(window_tokens: u32, char_per_token_ratio: f64, config: &PressureConfig) -> Self {
        let ratio = if char_per_token_ratio > 0.0 {
            char_per_token_ratio
        } else {
            DEFAULT_CHAR_PER_TOKEN_RATIO
        };

        if window_tokens == 0 {
            return Self {
                window_tokens: 0,
                char_per_token_ratio: ratio,
                single_tool_result_chars: 0,
                persisted_preview_chars: 0,
                snip_safety_tokens: 0,
                consolidate_trigger_tokens: 0,
                consolidate_target_tokens: 0,
                summary_max_chars: 0,
                min_keep_messages: config.min_keep_messages,
                keep_recent_tool_results: config.keep_recent_tool_results,
            };
        }

        let trigger_tokens = ratio_tokens(window_tokens, config.consolidate_trigger_ratio);

        Self {
            window_tokens,
            char_per_token_ratio: ratio,
            single_tool_result_chars: ratio_chars(
                window_tokens,
                config.single_tool_result_ratio,
                ratio,
                MIN_SINGLE_TOOL_RESULT_CHARS,
                MAX_SINGLE_TOOL_RESULT_CHARS,
            ),
            persisted_preview_chars: ratio_chars(
                window_tokens,
                config.persisted_preview_ratio,
                ratio,
                MIN_PERSISTED_PREVIEW_CHARS,
                MAX_PERSISTED_PREVIEW_CHARS,
            ),
            snip_safety_tokens: ratio_tokens(window_tokens, config.snip_safety_ratio)
                .clamp(MIN_SNIP_SAFETY_TOKENS, MAX_SNIP_SAFETY_TOKENS),
            consolidate_trigger_tokens: trigger_tokens,
            // The target is a fraction *of the trigger point*, not of the window:
            // it is the level consolidation brings the prompt down to.
            consolidate_target_tokens: ratio_tokens(
                trigger_tokens,
                config.consolidate_target_ratio,
            ),
            summary_max_chars: ratio_chars(
                window_tokens,
                config.summary_ratio,
                ratio,
                MIN_SUMMARY_CHARS,
                MAX_SUMMARY_CHARS,
            ),
            min_keep_messages: config.min_keep_messages,
            keep_recent_tool_results: config.keep_recent_tool_results,
        }
    }

    /// Tokens a given character count is estimated to occupy.
    pub fn estimate_tokens(&self, chars: usize) -> u32 {
        if self.char_per_token_ratio > 0.0 {
            ((chars as f64 / self.char_per_token_ratio).ceil() as u32).max(4)
        } else {
            4
        }
    }
}

/// `window × ratio` in tokens, rounded down.
fn ratio_tokens(window_tokens: u32, ratio: f32) -> u32 {
    ((window_tokens as f64) * (ratio as f64)) as u32
}

/// `window × ratio` converted to chars through the estimation ratio, clamped.
fn ratio_chars(
    window_tokens: u32,
    ratio: f32,
    char_per_token_ratio: f64,
    min_chars: usize,
    max_chars: usize,
) -> usize {
    let chars = (ratio_tokens(window_tokens, ratio) as f64 * char_per_token_ratio) as usize;
    chars.clamp(min_chars, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(window_tokens: u32) -> Config {
        let mut config = Config::default();
        let provider = config
            .providers
            .entry("default".to_string())
            .or_insert_with(Default::default);
        provider.context_window_tokens = window_tokens;
        config.agent.provider = "default".to_string();
        config
    }

    #[test]
    fn test_default_char_per_token_ratio_is_three() {
        assert!((DEFAULT_CHAR_PER_TOKEN_RATIO - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_anchor_default_window_is_32k() {
        let anchor = PressureAnchor::default();
        assert_eq!(anchor.window_tokens, 32_768);
    }

    #[test]
    fn test_anchor_from_config_reads_provider_window() {
        let anchor = PressureAnchor::from_config(&config_with(100_000));
        assert_eq!(anchor.window_tokens, 100_000);
    }

    #[test]
    fn test_anchor_from_config_missing_provider_falls_back() {
        let mut config = Config::default();
        config.agent.provider = "missing".to_string();
        let anchor = PressureAnchor::from_config(&config);
        assert_eq!(anchor.window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
    }

    #[test]
    fn test_derive_defaults_match_documented_percentages() {
        let limits = PressureAnchor::default().limits(3.0);
        // 25% of 32768 tokens, then × 3.0 chars/token.
        assert_eq!(limits.single_tool_result_chars, 24_576);
        // 4% of 32768 tokens → 1310 tokens → 3930 chars.
        assert_eq!(limits.persisted_preview_chars, 3_930);
        // 3% of 32768 tokens.
        assert_eq!(limits.snip_safety_tokens, 983);
        // 50% of 32768 tokens, target = 60% of the trigger point.
        assert_eq!(limits.consolidate_trigger_tokens, 16_384);
        assert_eq!(limits.consolidate_target_tokens, 9_830);
        // 2% of 32768 tokens → 655 tokens → 1965 chars.
        assert_eq!(limits.summary_max_chars, 1_965);
        // Count floors stay absolute.
        assert_eq!(limits.min_keep_messages, 4);
        assert_eq!(limits.keep_recent_tool_results, 10);
    }

    #[test]
    fn test_derive_doubling_window_doubles_budget() {
        let small = PressureAnchor::from_window(32_768).limits(3.0);
        let large = PressureAnchor::from_window(65_536).limits(3.0);

        let doubled = |a: usize, b: usize| (b as f64 / a as f64 - 2.0).abs() < 0.01;
        assert!(doubled(
            small.single_tool_result_chars,
            large.single_tool_result_chars
        ));
        assert!(doubled(
            small.persisted_preview_chars,
            large.persisted_preview_chars
        ));
        assert!(doubled(small.summary_max_chars, large.summary_max_chars));
        assert_eq!(
            large.snip_safety_tokens,
            small.snip_safety_tokens * 2,
            "safety margin is a token-flavoured percentage"
        );
        assert_eq!(
            large.consolidate_trigger_tokens,
            small.consolidate_trigger_tokens * 2
        );
        assert_eq!(
            large.consolidate_target_tokens,
            small.consolidate_target_tokens * 2
        );
    }

    #[test]
    fn test_derive_clamps_single_tool_result_at_max() {
        let limits = PressureAnchor::from_window(200_000).limits(3.0);
        assert_eq!(
            limits.single_tool_result_chars,
            MAX_SINGLE_TOOL_RESULT_CHARS
        );
    }

    #[test]
    fn test_derive_clamps_single_tool_result_at_min() {
        let limits = PressureAnchor::from_window(1024).limits(3.0);
        assert_eq!(
            limits.single_tool_result_chars,
            MIN_SINGLE_TOOL_RESULT_CHARS
        );
        assert_eq!(limits.persisted_preview_chars, MIN_PERSISTED_PREVIEW_CHARS);
        assert_eq!(limits.snip_safety_tokens, MIN_SNIP_SAFETY_TOKENS);
        assert_eq!(limits.summary_max_chars, MIN_SUMMARY_CHARS);
    }

    #[test]
    fn test_derive_clamps_summary_at_max() {
        let limits = PressureAnchor::from_window(200_000).limits(3.0);
        assert_eq!(limits.summary_max_chars, MAX_SUMMARY_CHARS);
    }

    #[test]
    fn test_derive_unknown_ratio_uses_cold_start_default() {
        let anchor = PressureAnchor::default();
        let unknown = anchor.limits(0.0);
        let cold_start = anchor.limits(DEFAULT_CHAR_PER_TOKEN_RATIO);
        assert_eq!(
            unknown.single_tool_result_chars,
            cold_start.single_tool_result_chars
        );
        assert!((unknown.char_per_token_ratio - DEFAULT_CHAR_PER_TOKEN_RATIO).abs() < f64::EPSILON);
    }

    #[test]
    fn test_derive_zero_window_disables_limits() {
        let limits = PressureAnchor::from_window(0).limits(3.0);
        assert_eq!(limits.single_tool_result_chars, 0);
        assert_eq!(limits.persisted_preview_chars, 0);
        assert_eq!(limits.snip_safety_tokens, 0);
        assert_eq!(limits.consolidate_trigger_tokens, 0);
        assert_eq!(limits.summary_max_chars, 0);
    }

    #[test]
    fn test_derive_honours_configured_percentages() {
        let config = PressureConfig {
            single_tool_result_ratio: 0.5,
            keep_recent_tool_results: 3,
            ..Default::default()
        };
        let limits = PressureAnchor::new(32_768, config).limits(3.0);
        // 50% of 32768 = 16384 tokens → 49152 chars.
        assert_eq!(limits.single_tool_result_chars, 49_152);
        assert_eq!(limits.keep_recent_tool_results, 3);
    }

    #[test]
    fn test_estimate_tokens_uses_derived_ratio() {
        let limits = PressureAnchor::from_window(32_768).limits(2.0);
        assert_eq!(limits.estimate_tokens(10), 5);
        assert_eq!(limits.estimate_tokens(1), 4, "minimum of 4 tokens per unit");
    }
}
