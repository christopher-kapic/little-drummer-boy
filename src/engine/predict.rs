//! Composer next-message prediction (`prompts/predict-next-message.md`).
//!
//! After each agent turn the TUI asks the utility model to predict what
//! the user is likely to type next, and offers the result as grey ghost
//! text in an empty composer. This module owns the *pure* pieces — turn
//! assembly, prompt construction, output bounding — and the one-shot
//! utility-model call that produces a prediction. The ghost-text
//! lifecycle / accept state machine lives on the composer (`src/tui/`);
//! this module never touches the UI.
//!
//! The call is a history-free, one-shot
//! [`Model::text_completion`](crate::engine::model::Model::text_completion)
//! against [`ExtendedConfig::utility_model`], mirroring the auto-titling
//! (§17d) and translation (`translate.rs`) utility-model paths. The
//! assembled prompt is **scrubbed through [`crate::redact::RedactionTable`]
//! before it leaves the process** — redaction is non-bypassable for every
//! outbound prompt (GOALS §7).
//!
//! Token economy (GOALS §10): the model sees only the **last 3 turns**,
//! each turn reduced to the user's input + the agent's final response —
//! no tool calls, no intermediate reasoning. The predicted output is
//! bounded to the mode (`short` ≈ one line; `long` a bounded full
//! response, never unbounded).

use std::time::Duration;

use crate::config::extended::{ExtendedConfig, PredictNextMessage};
use crate::config::providers::ProvidersConfig;

/// Timeout for one prediction call. Predictions are best-effort ghost
/// text; if the provider stalls we drop the prediction rather than tie up
/// a task.
pub const PREDICT_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Hard character cap on a `short` prediction (one line). Belt-and-braces
/// over the prompt instruction so a misbehaving model can't blow the
/// single-line affordance.
pub const SHORT_MAX_CHARS: usize = 200;

/// Hard character cap on a `long` prediction. Bounds the full proposed
/// response so it never grows unbounded (token economy, GOALS §10).
pub const LONG_MAX_CHARS: usize = 2000;

/// One conversation turn reduced to what the predictor sees: the user's
/// input and the agent's final response. Tool calls and reasoning are
/// excluded by construction — the caller only ever populates these two
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictionTurn {
    /// The user's message that opened the turn.
    pub user: String,
    /// The agent's final response text for the turn. Empty when the turn
    /// produced no final text (e.g. a tool-only turn) — still carried so
    /// the pairing with `user` is faithful.
    pub agent: String,
}

/// Number of most-recent turns fed to the predictor (token economy).
pub const PREDICTION_TURN_WINDOW: usize = 3;

/// Reduce a flat list of (user, agent-final-response) turns to the last
/// [`PREDICTION_TURN_WINDOW`] turns. The input is assumed already free of
/// tool calls / reasoning — callers build it from the user + agent-final
/// projections only. Returned oldest-first.
pub fn last_turns(turns: &[PredictionTurn]) -> Vec<PredictionTurn> {
    let start = turns.len().saturating_sub(PREDICTION_TURN_WINDOW);
    turns[start..].to_vec()
}

/// Build the one-shot prediction prompt from the last-3-turns transcript.
/// Names the mode's length bound so the utility model self-limits, fences
/// the transcript so the model treats it as context (not instructions),
/// and asks for ONLY the predicted next user message.
///
/// `mode` must be a non-`off` mode; `off` short-circuits before any prompt
/// is built (no utility call at all).
pub fn build_prediction_prompt(turns: &[PredictionTurn], mode: PredictNextMessage) -> String {
    let length_instruction = match mode {
        PredictNextMessage::Short => {
            "Keep it to a single short line — one sentence or phrase, no line breaks."
        }
        PredictNextMessage::Long => {
            "Write the full message the user would likely send next; it may span multiple \
             lines, but keep it concise — a few short paragraphs at most."
        }
        // Unreachable: the caller gates on `is_enabled()`. Fall back to the
        // short bound rather than panic.
        PredictNextMessage::Off => "Keep it to a single short line.",
    };

    let mut transcript = String::new();
    for turn in turns {
        transcript.push_str("USER: ");
        transcript.push_str(turn.user.trim());
        transcript.push('\n');
        if !turn.agent.trim().is_empty() {
            transcript.push_str("AGENT: ");
            transcript.push_str(turn.agent.trim());
            transcript.push('\n');
        }
    }

    format!(
        "You are predicting the next message a user will type to a coding agent, given the \
         recent conversation. Respond AS the user, in the first person — write the message \
         they would most likely send next. {length_instruction} Return ONLY the predicted \
         message, with no preamble, no quotes, and no explanation.\n\n\
         <conversation>\n{transcript}</conversation>",
    )
}

/// Trim a raw model response to a usable prediction and enforce the mode's
/// bound. `short` collapses to the first non-empty line and caps at
/// [`SHORT_MAX_CHARS`]; `long` keeps the whole response (trimmed) capped
/// at [`LONG_MAX_CHARS`]. Returns `None` when nothing usable survives
/// (the caller then shows no ghost).
pub fn bound_prediction(raw: &str, mode: PredictNextMessage) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bounded = match mode {
        PredictNextMessage::Short => {
            // Collapse to the first non-empty line, then char-cap.
            let line = trimmed.lines().find(|l| !l.trim().is_empty())?.trim();
            truncate_chars(line, SHORT_MAX_CHARS)
        }
        PredictNextMessage::Long => truncate_chars(trimmed, LONG_MAX_CHARS),
        PredictNextMessage::Off => return None,
    };
    if bounded.trim().is_empty() {
        None
    } else {
        Some(bounded)
    }
}

/// Truncate `s` to at most `max` characters on a char boundary, trimming
/// any trailing whitespace the cut may expose.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    truncated.trim_end().to_string()
}

/// Issue the prediction call for `turns` under `mode`. Returns the bounded
/// prediction, or `None` on every disabled/degrade path (mode `off`, no
/// utility model, empty transcript, build/send error, timeout, empty or
/// unusable response) so the caller simply shows no ghost text.
///
/// The assembled prompt is scrubbed through `redactor` before the
/// provider round-trip — redaction is non-bypassable (GOALS §7).
pub async fn predict(
    turns: &[PredictionTurn],
    mode: PredictNextMessage,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
    redactor: &crate::redact::RedactionTable,
) -> Option<String> {
    if !mode.is_enabled() {
        return None;
    }
    let window = last_turns(turns);
    // No agent response yet (fresh session) → nothing to predict.
    if window.is_empty() || window.iter().all(|t| t.agent.trim().is_empty()) {
        return None;
    }

    let model_ref = extended.utility_model.as_deref()?;
    let (provider_id, model_id) = model_ref.split_once(':')?;
    let model = match crate::engine::model::Model::for_provider(providers, provider_id, model_id) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "predict: model build failed; no ghost text");
            return None;
        }
    };

    let prompt = build_prediction_prompt(&window, mode);
    // Non-bypassable redaction of the outbound prompt (GOALS §7).
    let prompt = redactor.scrub(&prompt);

    let response =
        match tokio::time::timeout(PREDICT_CALL_TIMEOUT, model.text_completion(&prompt)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "predict: call failed; no ghost text");
                return None;
            }
            Err(_) => {
                tracing::debug!("predict: call timed out; no ghost text");
                return None;
            }
        };

    bound_prediction(&response, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(user: &str, agent: &str) -> PredictionTurn {
        PredictionTurn {
            user: user.to_string(),
            agent: agent.to_string(),
        }
    }

    #[test]
    fn last_turns_keeps_only_the_most_recent_three() {
        let turns = vec![
            turn("t1", "a1"),
            turn("t2", "a2"),
            turn("t3", "a3"),
            turn("t4", "a4"),
        ];
        let last = last_turns(&turns);
        assert_eq!(last.len(), 3);
        assert_eq!(last[0], turn("t2", "a2"));
        assert_eq!(last[2], turn("t4", "a4"));
    }

    #[test]
    fn last_turns_handles_fewer_than_window() {
        let turns = vec![turn("only", "resp")];
        assert_eq!(last_turns(&turns), turns);
        assert!(last_turns(&[]).is_empty());
    }

    #[test]
    fn prompt_contains_user_and_agent_text_but_not_tool_noise() {
        // The transcript fed in only ever carries user + agent-final text;
        // this asserts the prompt reflects exactly that and never invents
        // tool/reasoning markers.
        let turns = vec![turn("add a flag", "I added the flag.")];
        let p = build_prediction_prompt(&turns, PredictNextMessage::Short);
        assert!(p.contains("USER: add a flag"), "{p}");
        assert!(p.contains("AGENT: I added the flag."), "{p}");
        assert!(p.contains("single short line"), "{p}");
        // No tool-call / reasoning vocabulary leaks into the prompt body.
        assert!(!p.contains("tool_call"), "{p}");
        assert!(!p.contains("<think>"), "{p}");
    }

    #[test]
    fn prompt_omits_agent_line_when_response_empty() {
        // A tool-only turn (no final text) still pairs faithfully: the
        // USER line is present, the AGENT line is omitted (no empty marker).
        let turns = vec![turn("run the tests", "")];
        let p = build_prediction_prompt(&turns, PredictNextMessage::Long);
        assert!(p.contains("USER: run the tests"), "{p}");
        assert!(!p.contains("AGENT:"), "{p}");
        // Long mode names the multi-line allowance.
        assert!(p.contains("multiple"), "{p}");
    }

    #[test]
    fn bound_short_collapses_to_first_line_and_caps() {
        // Multi-line model output in short mode → first non-empty line.
        let raw = "first line\nsecond line\nthird";
        assert_eq!(
            bound_prediction(raw, PredictNextMessage::Short).as_deref(),
            Some("first line")
        );
        // Over-length single line is char-capped.
        let long = "x".repeat(SHORT_MAX_CHARS + 50);
        let bounded = bound_prediction(&long, PredictNextMessage::Short).unwrap();
        assert!(bounded.chars().count() <= SHORT_MAX_CHARS);
    }

    #[test]
    fn bound_long_keeps_multiline_but_caps_total() {
        let raw = "line one\nline two\nline three";
        assert_eq!(
            bound_prediction(raw, PredictNextMessage::Long).as_deref(),
            Some("line one\nline two\nline three")
        );
        let long = "y".repeat(LONG_MAX_CHARS + 100);
        let bounded = bound_prediction(&long, PredictNextMessage::Long).unwrap();
        assert!(bounded.chars().count() <= LONG_MAX_CHARS);
    }

    #[test]
    fn bound_returns_none_for_empty_or_whitespace() {
        assert_eq!(bound_prediction("", PredictNextMessage::Short), None);
        assert_eq!(bound_prediction("   \n  ", PredictNextMessage::Short), None);
        assert_eq!(bound_prediction("\n\n", PredictNextMessage::Long), None);
        // `off` never produces a prediction even from non-empty raw.
        assert_eq!(bound_prediction("hi", PredictNextMessage::Off), None);
    }
}
