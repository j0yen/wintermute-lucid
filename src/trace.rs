//! Turn timeline reconstruction — the read-side of lucid-trace.
//!
//! Exposes [`trace_turn`] (one turn by id) and [`last_turns`] (most-recent N
//! turns) as pure logic over the [`LucidStore`], kept separate from CLI
//! wiring so the functions are unit-testable without touching the filesystem.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::Record;

// ---------------------------------------------------------------------------
// Stage model
// ---------------------------------------------------------------------------

/// The expected ordered sequence of bus topics for a healthy turn.
/// "Missing next stage" is computed by walking this table, not hardcoded.
pub const STAGE_SEQUENCE: &[&str] = &[
    "wm.audio.wake",
    "wm.audio.speech.start",
    "wm.audio.speech.end",
    "wm.stt.final",
    "wm.dialog.turn.user",
    "wm.brain.route",
    "wm.brain.reply",
    "wm.tts.start",
    "wm.tts.end",
];

/// Topics that mark a dead/failed turn.
pub const FAILURE_TOPICS: &[&str] = &[
    "wm.stt.uncertain",
    "wm.stt.error",
    "wm.dialog.timeout",
    "wm.dialog.unheard",
    "wm.brain.error",
    "wm.tts.error",
];

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// One row in the rendered timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineRow {
    /// Milliseconds elapsed since turn-start (first event).
    pub offset_ms: u64,
    /// Bus topic for this event.
    pub topic: String,
    /// Optional stage duration in milliseconds (end - start for bounded stages).
    pub stage_duration_ms: Option<u64>,
    /// Optional annotation extracted from the payload (score, tier, text…).
    pub annotation: Option<String>,
}

/// Terminal status of the reconstructed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStatus {
    /// Turn reached `wm.tts.end` successfully.
    Completed {
        /// Total latency from first to last event.
        total_ms: u64,
    },
    /// Turn stopped before `wm.tts.end`.
    Stalled {
        /// The last-reached topic.
        last_topic: String,
        /// The next expected topic from the stage sequence (None if last_topic
        /// is not in the stage sequence or it was the final stage).
        expected_next: Option<String>,
    },
    /// Turn ended on an explicit failure topic.
    Failed {
        /// The failure topic.
        failure_topic: String,
        /// The next expected topic from the stage sequence before failure
        /// (for context).
        last_good_topic: Option<String>,
    },
}

/// The fully reconstructed timeline for one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTimeline {
    /// The turn correlation id.
    pub turn_id: String,
    /// Ordered event rows from oldest to newest.
    pub rows: Vec<TimelineRow>,
    /// Terminal status of the turn.
    pub status: TurnStatus,
    /// User-utterance text extracted from `wm.stt.final` payload, if present.
    pub utterance: Option<String>,
}

/// One-line summary for `lucid last N`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSummary {
    pub turn_id: String,
    pub status: TurnStatus,
    pub utterance: Option<String>,
    pub total_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Reconstruction logic
// ---------------------------------------------------------------------------

/// Reconstruct a single turn timeline from its records.
///
/// Returns `None` when no records carry this `turn_id` (AC7 — non-zero exit
/// with a clear message is the caller's responsibility).
#[must_use]
pub fn trace_turn(turn_id: &str, records: &[Record]) -> Option<TurnTimeline> {
    if records.is_empty() {
        return None;
    }

    // Sort by ts_received ascending.
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| r.ts_received);

    let t0 = sorted[0].ts_received;

    // Build a topic → last-seen-ts map for stage duration calculation.
    let mut topic_ts: BTreeMap<String, u64> = BTreeMap::new();
    let mut rows: Vec<TimelineRow> = Vec::new();
    let mut utterance: Option<String> = None;

    for rec in &sorted {
        let offset_ms = rec.ts_received.saturating_sub(t0);

        // Stage duration: for topics that bound a stage, look up the
        // corresponding start event.
        let stage_duration_ms = stage_start_topic(&rec.topic)
            .and_then(|start| topic_ts.get(start))
            .map(|&start_ts| rec.ts_received.saturating_sub(start_ts));

        // Annotation: pull notable scalar fields from the payload.
        let annotation = extract_annotation(&rec.topic, &rec.raw_payload);

        // Extract utterance text from stt.final.
        if rec.topic == "wm.stt.final" {
            utterance = rec
                .raw_payload
                .get("text")
                .or_else(|| rec.raw_payload.get("transcript"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
        }

        rows.push(TimelineRow {
            offset_ms,
            topic: rec.topic.clone(),
            stage_duration_ms,
            annotation,
        });

        topic_ts.insert(rec.topic.clone(), rec.ts_received);
    }

    // SAFETY: sorted is non-empty (early return above guards this).
    let last_ts = sorted.iter().map(|r| r.ts_received).max().unwrap_or(t0);
    let total_ms = last_ts.saturating_sub(t0);

    let status = compute_status(&sorted, total_ms);

    Some(TurnTimeline {
        turn_id: turn_id.to_string(),
        rows,
        status,
        utterance,
    })
}

/// Derive turn summaries for a set of turns (for `lucid last N`).
/// Input is a slice of `(turn_id, records_for_that_turn)`.
#[must_use]
pub fn summarise_turns(turns: &[(String, Vec<Record>)]) -> Vec<TurnSummary> {
    let mut out = Vec::new();
    for (id, recs) in turns {
        if let Some(tl) = trace_turn(id, recs) {
            let total_ms = match &tl.status {
                TurnStatus::Completed { total_ms } => Some(*total_ms),
                _ => None,
            };
            out.push(TurnSummary {
                turn_id: tl.turn_id,
                status: tl.status,
                utterance: tl.utterance,
                total_ms,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stage helpers
// ---------------------------------------------------------------------------

/// Given a topic that *ends* a stage, return the topic that *starts* that
/// stage (for duration computation).
fn stage_start_topic(end_topic: &str) -> Option<&'static str> {
    match end_topic {
        "wm.audio.speech.end" => Some("wm.audio.speech.start"),
        "wm.stt.final" => Some("wm.audio.speech.end"),
        "wm.brain.reply" => Some("wm.dialog.turn.user"),
        "wm.tts.end" => Some("wm.tts.start"),
        _ => None,
    }
}

/// Compute the terminal status of a turn from its sorted records.
fn compute_status(sorted: &[Record], total_ms: u64) -> TurnStatus {
    // sorted is non-empty at this point (callers guarantee it).
    let Some(last) = sorted.last() else {
        return TurnStatus::Stalled { last_topic: String::new(), expected_next: None };
    };

    // Check for explicit failure topics first.
    for rec in sorted.iter().rev() {
        if FAILURE_TOPICS.contains(&rec.topic.as_str()) {
            // Find the last "good" topic before this failure.
            let last_good = sorted
                .iter()
                .rev()
                .skip_while(|r| r.topic == rec.topic)
                .find(|r| !FAILURE_TOPICS.contains(&r.topic.as_str()))
                .map(|r| r.topic.clone());
            return TurnStatus::Failed {
                failure_topic: rec.topic.clone(),
                last_good_topic: last_good,
            };
        }
    }

    // Check if it completed (last topic is wm.tts.end).
    if last.topic == "wm.tts.end" {
        return TurnStatus::Completed { total_ms };
    }

    // Stalled: determine the expected next stage.
    let expected_next = STAGE_SEQUENCE
        .iter()
        .position(|&s| s == last.topic.as_str())
        .and_then(|idx| STAGE_SEQUENCE.get(idx + 1))
        .map(|s| (*s).to_string());

    TurnStatus::Stalled {
        last_topic: last.topic.clone(),
        expected_next,
    }
}

/// Pull a compact annotation from well-known payload fields.
fn extract_annotation(topic: &str, payload: &serde_json::Value) -> Option<String> {
    match topic {
        "wm.audio.wake" => payload
            .get("score")
            .and_then(|v| v.as_f64())
            .map(|f| format!("score={f:.2}")),
        "wm.stt.final" => payload
            .get("text")
            .or_else(|| payload.get("transcript"))
            .and_then(serde_json::Value::as_str)
            .map(|t| format!("\"{t}\"")),
        "wm.brain.route" => payload
            .get("tier")
            .or_else(|| payload.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(|t| format!("tier={t}")),
        "wm.brain.reply" => payload
            .get("text")
            .or_else(|| payload.get("reply"))
            .and_then(serde_json::Value::as_str)
            .map(|t| {
                let preview: String = t.chars().take(40).collect();
                format!("\"{preview}\"")
            }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers (human display)
// ---------------------------------------------------------------------------

/// Render a [`TurnTimeline`] to a human-readable string (for `lucid trace`).
#[must_use]
pub fn render_timeline(tl: &TurnTimeline) -> String {
    let mut out = String::new();

    // Header line.
    let utterance = tl.utterance.as_deref().unwrap_or("(unknown)");
    out.push_str(&format!("turn {}   \"{utterance}\"\n", tl.turn_id));

    for row in &tl.rows {
        let offset = format!("+{}ms", row.offset_ms);
        let dur_part = match row.stage_duration_ms {
            Some(d) => format!("({:.2}s stage)", d as f64 / 1000.0),
            None => String::new(),
        };
        let ann_part = row.annotation.as_deref().unwrap_or("");

        let extra = match (dur_part.is_empty(), ann_part.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!("  {dur_part}"),
            (true, false) => format!("  {ann_part}"),
            (false, false) => format!("  {dur_part}  {ann_part}"),
        };

        out.push_str(&format!("  {offset:<10}  {:<35}{extra}\n", row.topic));
    }

    // Status footer.
    match &tl.status {
        TurnStatus::Completed { total_ms } => {
            out.push_str(&format!(
                "  ✓ completed  (end-to-end {:.2}s)\n",
                *total_ms as f64 / 1000.0
            ));
        }
        TurnStatus::Stalled {
            last_topic,
            expected_next,
        } => {
            let next = expected_next
                .as_deref()
                .unwrap_or("(unknown — last in sequence)");
            out.push_str(&format!(
                "  ✗ stalled after {last_topic} — no {next}\n"
            ));
        }
        TurnStatus::Failed {
            failure_topic,
            last_good_topic,
        } => {
            let last_good = last_good_topic.as_deref().unwrap_or("(start)");
            out.push_str(&format!(
                "  ✗ failed at {failure_topic} (after {last_good})\n"
            ));
        }
    }

    out
}

/// Render a one-line summary for `lucid last N`.
#[must_use]
pub fn render_summary(s: &TurnSummary) -> String {
    let utterance = s.utterance.as_deref().unwrap_or("(unknown)");
    let status_str = match &s.status {
        TurnStatus::Completed { total_ms } => {
            format!("✓ {:.2}s", *total_ms as f64 / 1000.0)
        }
        TurnStatus::Stalled { last_topic, .. } => {
            format!("✗ stalled@{last_topic}")
        }
        TurnStatus::Failed { failure_topic, .. } => {
            format!("✗ failed:{failure_topic}")
        }
    };
    format!("{}  {}  \"{}\"", s.turn_id, status_str, utterance)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_record(ts: u64, topic: &str, turn: &str, extra: serde_json::Value) -> Record {
        let mut payload = json!({"turn_id": turn});
        if let (Some(obj), Some(ext)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in ext {
                obj.insert(k.clone(), v.clone());
            }
        }
        Record::new(ts, topic.to_string(), "peer".to_string(), payload)
    }

    fn happy_path_records(turn: &str) -> Vec<Record> {
        vec![
            make_record(1000, "wm.audio.wake", turn, json!({"score": 0.99})),
            make_record(1018, "wm.audio.speech.start", turn, json!({})),
            make_record(2258, "wm.audio.speech.end", turn, json!({})),
            make_record(4968, "wm.stt.final", turn, json!({"text": "what time is it"})),
            make_record(4980, "wm.dialog.turn.user", turn, json!({})),
            make_record(4988, "wm.brain.route", turn, json!({"tier": "sonnet"})),
            make_record(7133, "wm.brain.reply", turn, json!({"text": "It's about 3:45"})),
            make_record(7158, "wm.tts.start", turn, json!({})),
            make_record(9418, "wm.tts.end", turn, json!({})),
        ]
    }

    // AC1: trace returns rows ordered by offset, oldest first.
    #[test]
    fn ac1_rows_ordered_oldest_first() {
        let recs = happy_path_records("turn-1");
        let tl = trace_turn("turn-1", &recs).expect("some");
        for i in 1..tl.rows.len() {
            assert!(
                tl.rows[i].offset_ms >= tl.rows[i - 1].offset_ms,
                "row {i} out of order"
            );
        }
    }

    // AC2: stage durations are computed for capture, stt, brain, tts.
    #[test]
    fn ac2_stage_durations_computed() {
        let recs = happy_path_records("turn-2");
        let tl = trace_turn("turn-2", &recs).expect("some");
        // Find rows with durations.
        let dur_map: BTreeMap<String, u64> = tl
            .rows
            .iter()
            .filter_map(|r| r.stage_duration_ms.map(|d| (r.topic.clone(), d)))
            .collect();
        // capture = speech.end - speech.start
        assert_eq!(dur_map.get("wm.audio.speech.end"), Some(&(2258 - 1018)));
        // stt = stt.final - speech.end
        assert_eq!(dur_map.get("wm.stt.final"), Some(&(4968 - 2258)));
        // brain = brain.reply - dialog.turn.user
        assert_eq!(dur_map.get("wm.brain.reply"), Some(&(7133 - 4980)));
        // tts = tts.end - tts.start
        assert_eq!(dur_map.get("wm.tts.end"), Some(&(9418 - 7158)));
    }

    // AC3: stall detection — truncated after stt.final names the missing stage.
    #[test]
    fn ac3_stall_after_stt_final_names_missing_stage() {
        let turn = "turn-stall";
        let recs = vec![
            make_record(1000, "wm.audio.wake", turn, json!({"score": 0.95})),
            make_record(1018, "wm.audio.speech.start", turn, json!({})),
            make_record(2258, "wm.audio.speech.end", turn, json!({})),
            make_record(4968, "wm.stt.final", turn, json!({"text": "hello"})),
            // no dialog.turn.user onward
        ];
        let tl = trace_turn(turn, &recs).expect("some");
        match &tl.status {
            TurnStatus::Stalled {
                last_topic,
                expected_next,
            } => {
                assert_eq!(last_topic, "wm.stt.final");
                assert_eq!(
                    expected_next.as_deref(),
                    Some("wm.dialog.turn.user"),
                    "must name the missing next stage"
                );
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
    }

    // AC4: completed turn reports total latency.
    #[test]
    fn ac4_completed_reports_total_latency() {
        let recs = happy_path_records("turn-ok");
        let tl = trace_turn("turn-ok", &recs).expect("some");
        assert!(
            matches!(tl.status, TurnStatus::Completed { total_ms } if total_ms == 9418 - 1000),
            "expected Completed with total 8418ms, got {:?}",
            tl.status
        );
    }

    // AC5: lucid last — summarise_turns returns one entry per turn.
    #[test]
    fn ac5_summarise_turns_one_entry_per_turn() {
        let a = happy_path_records("turn-a");
        let b = happy_path_records("turn-b");
        let pairs = vec![("turn-a".to_string(), a), ("turn-b".to_string(), b)];
        let summaries = summarise_turns(&pairs);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].turn_id, "turn-a");
        assert_eq!(summaries[1].turn_id, "turn-b");
    }

    // AC6: --json produces valid serde-roundtrippable output.
    #[test]
    fn ac6_timeline_serde_roundtrip() {
        let recs = happy_path_records("turn-json");
        let tl = trace_turn("turn-json", &recs).expect("some");
        let json_str = serde_json::to_string(&tl).expect("serialize");
        let back: TurnTimeline = serde_json::from_str(&json_str).expect("deserialize");
        assert_eq!(back.turn_id, tl.turn_id);
        assert_eq!(back.rows.len(), tl.rows.len());
    }

    // AC7: unknown turn id returns None (non-zero exit is caller's job).
    #[test]
    fn ac7_unknown_turn_id_returns_none() {
        let result = trace_turn("no-such-id", &[]);
        assert!(result.is_none());
    }

    // Failure detection: a turn ending on a failure topic is marked Failed.
    #[test]
    fn failure_topic_marks_turn_as_failed() {
        let turn = "turn-fail";
        let recs = vec![
            make_record(1000, "wm.audio.wake", turn, json!({"score": 0.9})),
            make_record(1018, "wm.audio.speech.start", turn, json!({})),
            make_record(2258, "wm.audio.speech.end", turn, json!({})),
            make_record(4000, "wm.stt.error", turn, json!({"msg": "decode fail"})),
        ];
        let tl = trace_turn(turn, &recs).expect("some");
        assert!(
            matches!(tl.status, TurnStatus::Failed { .. }),
            "expected Failed, got {:?}",
            tl.status
        );
    }

    // render_timeline smoke: completed turn renders ✓ footer.
    #[test]
    fn render_timeline_completed_footer() {
        let recs = happy_path_records("turn-render");
        let tl = trace_turn("turn-render", &recs).expect("some");
        let rendered = render_timeline(&tl);
        assert!(rendered.contains("✓ completed"), "must show ✓ completed");
        assert!(rendered.contains("turn-render"), "must show turn id");
    }

    // render_timeline smoke: stalled turn renders ✗ footer with missing stage.
    #[test]
    fn render_timeline_stalled_footer() {
        let turn = "turn-stall2";
        let recs = vec![
            make_record(1000, "wm.audio.wake", turn, json!({})),
            make_record(1018, "wm.audio.speech.start", turn, json!({})),
            make_record(2258, "wm.stt.final", turn, json!({"text": "hi"})),
        ];
        let tl = trace_turn(turn, &recs).expect("some");
        let rendered = render_timeline(&tl);
        assert!(rendered.contains("✗ stalled"), "must show ✗ stalled");
    }
}
