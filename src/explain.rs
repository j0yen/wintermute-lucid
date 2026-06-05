//! `lucid explain` — plain-language narration of a recorded turn.
//!
//! Composes the structured outputs of [`crate::trace`] and [`crate::mind`]
//! into one or two human-readable sentences describing what happened during
//! a turn.
//!
//! **Deterministic, not generative.** Every sentence is assembled from
//! templates over the structured fields — no LLM call is ever made, so
//! explaining a turn never consumes a brain turn or cloud credit, and
//! explaining a failed turn works even when the brain is what failed.
//!
//! **No wm.brain.* bus events are emitted.** The only optional bus output
//! is `wm.tts.say` (controlled by `--voice`).
//!
//! # Stall → cause table
//!
//! | Stall / failure topic      | Human cause                                        |
//! |----------------------------|----------------------------------------------------|
//! | no-wake                    | I never heard my name, so I didn't start listening |
//! | `wm.stt.uncertain`         | I started listening but couldn't make out any words |
//! | `wm.stt.error`             | I started listening but my speech recognition failed |
//! | `wm.dialog.timeout`        | I heard you but you went quiet before I could confirm |
//! | `wm.dialog.unheard`        | My dialog system lost track of the turn            |
//! | `wm.brain.error`           | I understood you but my thinking step failed       |
//! | `wm.tts.error`             | I had the answer but couldn't speak it             |
//! | stt-stalled                | I started listening but couldn't make out any words |

use serde::{Deserialize, Serialize};

use crate::mind::TurnMind;
use crate::trace::{TurnStatus, TurnTimeline};

// ── Persona ────────────────────────────────────────────────────────────────────

/// Voice register for the spoken or printed narration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    /// Warm, first-person, conversational (the `hearth` voice).
    Hearth,
    /// Flat, diagnostic, log-like.
    Flat,
}

impl Default for Persona {
    fn default() -> Self {
        Self::Flat
    }
}

impl std::str::FromStr for Persona {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hearth" => Ok(Self::Hearth),
            "flat" => Ok(Self::Flat),
            other => Err(format!("unknown persona {other:?}; expected hearth or flat")),
        }
    }
}

// ── Explanation ────────────────────────────────────────────────────────────────

/// The structured narration produced for a single turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnExplanation {
    /// The turn correlation id that was explained.
    pub turn_id: String,
    /// The one-or-two sentence plain-language account.
    pub text: String,
    /// The persona the text was rendered with.
    pub persona: Persona,
    /// Whether the turn completed successfully.
    pub success: bool,
}

// ── Explain engine ─────────────────────────────────────────────────────────────

/// Build a [`TurnExplanation`] from a trace timeline and optional mind view.
///
/// The `mind` argument may be `None` when the brain never ran (e.g. the turn
/// died at wake or STT) — the function degrades gracefully.
#[must_use]
pub fn explain(
    timeline: &TurnTimeline,
    mind: Option<&TurnMind>,
    persona: Persona,
) -> TurnExplanation {
    let text = match &timeline.status {
        TurnStatus::Completed { total_ms } => {
            explain_success(timeline, mind, *total_ms, persona)
        }
        TurnStatus::Stalled {
            last_topic,
            expected_next,
        } => explain_stall(last_topic, expected_next.as_deref(), persona),
        TurnStatus::Failed {
            failure_topic,
            last_good_topic: _,
        } => explain_failure(failure_topic, persona),
    };

    let success = matches!(timeline.status, TurnStatus::Completed { .. });

    TurnExplanation {
        turn_id: timeline.turn_id.clone(),
        text,
        persona,
        success,
    }
}

// ── Success narration ──────────────────────────────────────────────────────────

fn explain_success(
    timeline: &TurnTimeline,
    mind: Option<&TurnMind>,
    total_ms: u64,
    persona: Persona,
) -> String {
    let utterance = timeline.utterance.as_deref().unwrap_or("something");
    let latency_s = total_ms as f64 / 1000.0;

    let route_part = mind.and_then(|m| {
        let tier = m.tier.as_deref().unwrap_or("an unknown tier");
        let model = m.model.as_deref();
        Some(match model {
            Some(m) => format!("{tier} ({m})"),
            None => tier.to_string(),
        })
    });

    let reply_preview = mind
        .and_then(|m| m.reply_text.as_deref())
        .map(|t| truncate(t, 60));

    match persona {
        Persona::Hearth => {
            let first = match &reply_preview {
                Some(reply) => format!(
                    "You said \"{utterance}\" and I replied \"{reply}\"."
                ),
                None => format!("You said \"{utterance}\" and I answered."),
            };
            let second = match &route_part {
                Some(route) => format!(
                    "I routed it through {route} and the whole turn took {latency_s:.1} seconds."
                ),
                None => format!("The whole turn took {latency_s:.1} seconds."),
            };
            format!("{first} {second}")
        }
        Persona::Flat => {
            let route_str = route_part
                .map(|r| format!(" via {r}"))
                .unwrap_or_default();
            let reply_str = reply_preview
                .map(|r| format!(" reply=\"{r}\""))
                .unwrap_or_default();
            format!(
                "turn={} status=completed utterance=\"{utterance}\"{route_str}{reply_str} latency={latency_s:.1}s",
                timeline.turn_id
            )
        }
    }
}

// ── Stall narration ────────────────────────────────────────────────────────────

fn explain_stall(last_topic: &str, expected_next: Option<&str>, persona: Persona) -> String {
    // Map where we stalled to a human cause.
    let cause = stall_cause(last_topic, expected_next);
    match persona {
        Persona::Hearth => format!("{cause}."),
        Persona::Flat => format!("status=stalled last={last_topic} cause=\"{cause}\""),
    }
}

/// Map a stall position to a human-readable cause sentence (no trailing
/// period — the caller adds it if needed).
fn stall_cause(last_topic: &str, expected_next: Option<&str>) -> &'static str {
    // If we never hit audio.wake, the turn never started.
    if last_topic.is_empty() || !last_topic.starts_with("wm.") {
        return "I never heard my name, so I didn't start listening";
    }

    // Stalled after wake but before STT completion.
    match (last_topic, expected_next) {
        // Never started — no wake event at all (synthetic empty turn).
        ("", _) => "I never heard my name, so I didn't start listening",

        // Stalled at wake — speech capture never started.
        ("wm.audio.wake", _) => {
            "I heard my name but speech capture never started"
        }

        // Stalled at speech.start — speech capture started but never ended.
        ("wm.audio.speech.start", _) => {
            "I started listening but you stopped speaking before I could capture anything"
        }

        // Stalled at speech.end — STT never completed.
        ("wm.audio.speech.end", _) | (_, Some("wm.stt.final")) => {
            "I started listening but couldn't make out any words"
        }

        // Stalled at stt.final — dialog never picked it up.
        ("wm.stt.final", _) | (_, Some("wm.dialog.turn.user")) => {
            "I understood what you said but my dialog system didn't pick it up"
        }

        // Stalled at dialog.turn.user — brain routing never happened.
        ("wm.dialog.turn.user", _) | (_, Some("wm.brain.route")) => {
            "I received your message but my brain router never started"
        }

        // Stalled at brain.route — brain never replied.
        ("wm.brain.route", _) | (_, Some("wm.brain.reply")) => {
            "I understood you but my thinking step didn't produce a reply"
        }

        // Stalled at brain.reply — TTS never started.
        ("wm.brain.reply", _) | (_, Some("wm.tts.start")) => {
            "I had the answer but my text-to-speech system never started"
        }

        // Stalled at tts.start — TTS started but never ended.
        ("wm.tts.start", _) | (_, Some("wm.tts.end")) => {
            "I started speaking but my text-to-speech stream didn't finish"
        }

        _ => "the turn stopped unexpectedly before completing",
    }
}

// ── Failure narration ──────────────────────────────────────────────────────────

fn explain_failure(failure_topic: &str, persona: Persona) -> String {
    let cause = failure_cause(failure_topic);
    match persona {
        Persona::Hearth => format!("{cause}."),
        Persona::Flat => format!(
            "status=failed failure={failure_topic} cause=\"{cause}\""
        ),
    }
}

/// Map a failure topic to a human-readable cause sentence (no trailing
/// period).
fn failure_cause(topic: &str) -> &'static str {
    match topic {
        "wm.stt.uncertain" => {
            "I started listening but couldn't make out any words"
        }
        "wm.stt.error" => {
            "I started listening but my speech recognition failed"
        }
        "wm.dialog.timeout" => {
            "I heard you but you went quiet before I could confirm"
        }
        "wm.dialog.unheard" => {
            "My dialog system lost track of the turn"
        }
        "wm.brain.error" => {
            "I understood you but my thinking step failed"
        }
        "wm.tts.error" => {
            "I had the answer but couldn't speak it"
        }
        _ => "an unexpected error ended the turn",
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Truncate a string to at most `max_chars` characters, appending `…` if
/// truncation occurred.
fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = chars[..max_chars].iter().collect();
        format!("{truncated}…")
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;
    use crate::record::Record;
    use crate::trace::trace_turn;
    use serde_json::json;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn rec(ts: u64, topic: &str, turn: &str, extra: serde_json::Value) -> Record {
        let mut payload = json!({"turn_id": turn});
        if let (Some(obj), Some(ext)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in ext {
                obj.insert(k.clone(), v.clone());
            }
        }
        Record::new(ts, topic.to_string(), "peer".to_string(), payload)
    }

    fn happy_records(turn: &str) -> Vec<Record> {
        vec![
            rec(1000, "wm.audio.wake", turn, json!({"score": 0.99})),
            rec(1018, "wm.audio.speech.start", turn, json!({})),
            rec(2258, "wm.audio.speech.end", turn, json!({})),
            rec(4968, "wm.stt.final", turn, json!({"text": "what time is it"})),
            rec(4980, "wm.dialog.turn.user", turn, json!({})),
            rec(4988, "wm.brain.route", turn, json!({"tier": "sonnet", "model": "claude-sonnet-4-6"})),
            rec(7133, "wm.brain.reply", turn, json!({"text": "It's about 3:45"})),
            rec(7158, "wm.tts.start", turn, json!({})),
            rec(9418, "wm.tts.end", turn, json!({})),
        ]
    }

    fn failure_records(turn: &str, fail_topic: &str) -> Vec<Record> {
        let mut base = vec![
            rec(1000, "wm.audio.wake", turn, json!({"score": 0.9})),
            rec(1018, "wm.audio.speech.start", turn, json!({})),
            rec(2258, "wm.audio.speech.end", turn, json!({})),
            rec(4968, "wm.stt.final", turn, json!({"text": "hello"})),
        ];
        base.push(rec(5000, fail_topic, turn, json!({})));
        base
    }

    // ── AC1: successful turn narration ────────────────────────────────────────

    #[test]
    fn ac1_success_hearth_names_utterance_reply_latency() {
        let recs = happy_records("t1");
        let tl = trace_turn("t1", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(ex.success);
        assert!(ex.text.contains("what time is it"), "must name utterance");
        assert!(ex.text.contains("3:45") || ex.text.contains("answered"), "must include reply or answered");
    }

    #[test]
    fn ac1_success_flat_contains_completed() {
        let recs = happy_records("t1f");
        let tl = trace_turn("t1f", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Flat);
        assert!(ex.success);
        assert!(ex.text.contains("completed"), "flat must say completed");
        assert!(ex.text.contains("what time is it"), "flat must name utterance");
    }

    // ── AC2: each failure mode → distinct sentence ────────────────────────────

    #[test]
    fn ac2_stt_uncertain_distinct_sentence() {
        let recs = failure_records("t-unc", "wm.stt.uncertain");
        let tl = trace_turn("t-unc", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("couldn't make out any words"),
            "got: {}",
            ex.text
        );
    }

    #[test]
    fn ac2_dialog_timeout_distinct_sentence() {
        let recs = failure_records("t-to", "wm.dialog.timeout");
        let tl = trace_turn("t-to", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("went quiet") || ex.text.contains("confirm"),
            "got: {}",
            ex.text
        );
    }

    #[test]
    fn ac2_brain_error_distinct_sentence() {
        let recs = failure_records("t-be", "wm.brain.error");
        let tl = trace_turn("t-be", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("thinking step failed"),
            "got: {}",
            ex.text
        );
    }

    #[test]
    fn ac2_tts_error_distinct_sentence() {
        let recs = failure_records("t-tts", "wm.tts.error");
        let tl = trace_turn("t-tts", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("couldn't speak it"),
            "got: {}",
            ex.text
        );
    }

    #[test]
    fn ac2_stt_error_distinct_sentence() {
        let recs = failure_records("t-se", "wm.stt.error");
        let tl = trace_turn("t-se", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("speech recognition failed"),
            "got: {}",
            ex.text
        );
    }

    #[test]
    fn ac2_dialog_unheard_distinct_sentence() {
        let recs = failure_records("t-du", "wm.dialog.unheard");
        let tl = trace_turn("t-du", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert!(!ex.success);
        assert!(
            ex.text.contains("lost track"),
            "got: {}",
            ex.text
        );
    }

    // ── AC3: determinism (golden output) ──────────────────────────────────────

    #[test]
    fn ac3_deterministic_same_input_same_output() {
        let recs = happy_records("t-det");
        let tl = trace_turn("t-det", &recs).unwrap();
        let a = explain(&tl, None, Persona::Hearth);
        let b = explain(&tl, None, Persona::Hearth);
        assert_eq!(a.text, b.text, "explain must be deterministic");
    }

    #[test]
    fn ac3_golden_failure_brain_error() {
        // Golden: brain.error Hearth sentence is fixed.
        let recs = failure_records("t-gold", "wm.brain.error");
        let tl = trace_turn("t-gold", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert_eq!(
            ex.text,
            "I understood you but my thinking step failed.",
            "golden hearth brain-error sentence changed"
        );
    }

    #[test]
    fn ac3_golden_failure_stt_uncertain() {
        let recs = failure_records("t-g2", "wm.stt.uncertain");
        let tl = trace_turn("t-g2", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Hearth);
        assert_eq!(
            ex.text,
            "I started listening but couldn't make out any words.",
            "golden hearth stt-uncertain sentence changed"
        );
    }

    // ── AC5: persona hearth vs flat differ ────────────────────────────────────

    #[test]
    fn ac5_hearth_vs_flat_differ_for_success() {
        let recs = happy_records("t-p");
        let tl = trace_turn("t-p", &recs).unwrap();
        let hearth = explain(&tl, None, Persona::Hearth);
        let flat = explain(&tl, None, Persona::Flat);
        assert_ne!(hearth.text, flat.text, "personas must produce different text");
        // Hearth uses first-person "You said"
        assert!(hearth.text.contains("You said"), "hearth must use 'You said'");
        // Flat uses key=value
        assert!(flat.text.contains("status=completed"), "flat must use key=value");
    }

    #[test]
    fn ac5_hearth_vs_flat_differ_for_failure() {
        let recs = failure_records("t-pf", "wm.brain.error");
        let tl = trace_turn("t-pf", &recs).unwrap();
        let hearth = explain(&tl, None, Persona::Hearth);
        let flat = explain(&tl, None, Persona::Flat);
        assert_ne!(hearth.text, flat.text, "personas must produce different text for failure");
        assert!(flat.text.contains("status=failed"), "flat must say status=failed");
    }

    // ── AC7: no brain events emitted ─────────────────────────────────────────
    // This test proves structurally: explain() takes no bus Client parameter
    // and has no async path, so it cannot emit bus events.
    #[test]
    fn ac7_explain_is_sync_and_pure() {
        // If this compiles and runs, explain cannot have emitted bus events.
        let recs = happy_records("t-pure");
        let tl = trace_turn("t-pure", &recs).unwrap();
        let ex = explain(&tl, None, Persona::Flat);
        // Success if we get here — no bus interaction possible from sync fn.
        assert!(ex.success);
    }
}
