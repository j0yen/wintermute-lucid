//! Tests for the `lucid watch` state machine (AC1–AC7 from PRD-lucid-live).
//!
//! All tests are hermetic — no live bus or TUI required.

use serde_json::json;
use wintermute_lucid::watch::{
    PipelineState, Stage, StageState, classify_topic, TopicKind,
};

const STALL_MS: u64 = 8_000;

fn apply(state: &mut PipelineState, topic: &str, payload: serde_json::Value, now_ms: u64) -> bool {
    state.apply(topic, &payload, now_ms, STALL_MS)
}

// AC1: pipeline row updates stage state as events arrive for the active turn.
#[test]
fn ac1_pipeline_stages_light_up_in_order() {
    let mut state = PipelineState::default();

    // Before a wake, everything is idle.
    for st in state.stages {
        assert_eq!(st, StageState::Idle);
    }

    let mut t = 1_000u64;

    // Wake fires → wake stage Active.
    apply(&mut state, "wm.audio.wake", json!({"turn_id": "t1"}), t);
    assert_eq!(state.stages[0], StageState::Active, "wake should be Active");

    // Capture start → capture Active, wake still active.
    t += 100;
    apply(&mut state, "wm.audio.speech.start", json!({}), t);
    assert_eq!(state.stages[1], StageState::Active, "capture should be Active");

    // STT partial → STT active; capture done.
    t += 200;
    apply(&mut state, "wm.stt.partial", json!({"text": "hello"}), t);
    assert_eq!(state.stages[2], StageState::Active, "stt should be Active");
    assert_eq!(state.stages[1], StageState::Done, "capture should be Done");
    assert_eq!(state.partial_transcript, "hello");

    // STT final → STT done.
    t += 300;
    apply(&mut state, "wm.stt.final", json!({"text": "hello world", "turn_id": "t1"}), t);
    assert_eq!(state.stages[2], StageState::Done, "stt should be Done after final");

    // Brain reply → brain done.
    t += 500;
    apply(&mut state, "wm.brain.reply", json!({"turn_id": "t1"}), t);
    assert_eq!(state.stages[4], StageState::Done, "brain should be Done");

    // TTS end → turn complete, scrollback entry added.
    t += 300;
    apply(&mut state, "wm.tts.end", json!({"turn_id": "t1"}), t);
    assert_eq!(state.stages[5], StageState::Done, "tts should be Done");
    assert_eq!(state.scrollback.len(), 1);
    assert!(state.scrollback[0].success);
}

// AC2: dialog state from wm.dialog.state is shown live.
#[test]
fn ac2_dialog_state_updates() {
    let mut state = PipelineState::default();
    let t = 1_000u64;

    apply(&mut state, "wm.audio.wake", json!({}), t);
    apply(&mut state, "wm.dialog.state", json!({"state": "Listening"}), t + 10);
    assert_eq!(state.dialog_state, "Listening");

    apply(&mut state, "wm.dialog.state", json!({"state": "Transcribing"}), t + 200);
    assert_eq!(state.dialog_state, "Transcribing");

    apply(&mut state, "wm.dialog.state", json!({"state": "Idle"}), t + 800);
    assert_eq!(state.dialog_state, "Idle");
}

// AC3: partial transcript is displayed and updated.
#[test]
fn ac3_partial_transcript_streams() {
    let mut state = PipelineState::default();
    let t = 1_000u64;

    apply(&mut state, "wm.audio.wake", json!({}), t);
    apply(&mut state, "wm.stt.partial", json!({"text": "wint"}), t + 100);
    assert_eq!(state.partial_transcript, "wint");

    apply(&mut state, "wm.stt.partial", json!({"text": "wintermute"}), t + 200);
    assert_eq!(state.partial_transcript, "wintermute");
}

// AC4: a stage that stays active past the stall threshold is flagged Stalled.
#[test]
fn ac4_stall_detection() {
    let mut state = PipelineState::default();

    apply(&mut state, "wm.audio.wake", json!({}), 1_000);
    apply(&mut state, "wm.audio.speech.start", json!({}), 1_100);
    apply(&mut state, "wm.stt.partial", json!({"text": "hey"}), 1_500);

    // STT is active at 1500ms. No final arrives. Advance clock past threshold.
    let future_ms = 1_500 + STALL_MS + 500;
    state.check_stalls(future_ms, STALL_MS);

    assert_eq!(state.stages[2], StageState::Stalled, "stt should be Stalled");
    assert_eq!(state.stall_stage, Some(Stage::Stt));
}

// AC4 (failure topic): a failure topic flashes the stage as Failed.
#[test]
fn ac4_failure_topic_marks_stage_failed() {
    let mut state = PipelineState::default();
    let t = 1_000u64;

    apply(&mut state, "wm.audio.wake", json!({}), t);
    apply(&mut state, "wm.audio.speech.start", json!({}), t + 100);
    apply(&mut state, "wm.stt.partial", json!({"text": "hey"}), t + 200);
    apply(&mut state, "wm.brain.error", json!({}), t + 400);
    assert_eq!(state.stages[4], StageState::Failed, "brain should be Failed");
}

// AC5: completed turns accumulate in the scrollback.
#[test]
fn ac5_scrollback_accumulates() {
    let mut state = PipelineState::default();
    let mut t = 0u64;

    for i in 0..3 {
        let tid = format!("t{i}");
        t += 1_000;
        apply(&mut state, "wm.audio.wake", json!({"turn_id": tid}), t);
        t += 200;
        apply(&mut state, "wm.stt.partial", json!({"text": "hello"}), t);
        t += 300;
        apply(&mut state, "wm.tts.end", json!({"turn_id": tid}), t);
    }

    assert_eq!(state.scrollback.len(), 3);
    assert!(state.scrollback.iter().all(|e| e.success));
}

// AC6: --plain path: topic classification covers all documented topics.
#[test]
fn ac6_plain_topic_classification() {
    assert_eq!(classify_topic("wm.audio.wake"), TopicKind::StageStart(Stage::Wake));
    assert_eq!(classify_topic("wm.stt.partial"), TopicKind::SttPartial);
    assert_eq!(classify_topic("wm.stt.final"), TopicKind::StageEnd(Stage::Stt));
    assert_eq!(classify_topic("wm.brain.reply"), TopicKind::StageEnd(Stage::Brain));
    assert_eq!(classify_topic("wm.tts.end"), TopicKind::TtsEnd);
    assert_eq!(classify_topic("wm.dialog.state"), TopicKind::DialogState);
    assert_eq!(classify_topic("wm.brain.error"), TopicKind::StageError(Stage::Brain));
    assert_eq!(classify_topic("wm.other.random"), TopicKind::Unrelated);
}

// AC7: a new wake event mid-stream resets the pipeline for the new turn.
#[test]
fn ac7_new_wake_resets_pipeline() {
    let mut state = PipelineState::default();
    let t = 1_000u64;

    apply(&mut state, "wm.audio.wake", json!({"turn_id": "t1"}), t);
    apply(&mut state, "wm.stt.partial", json!({"text": "hello"}), t + 100);

    // Interrupted by another wake — previous turn flushed as stalled.
    apply(&mut state, "wm.audio.wake", json!({"turn_id": "t2"}), t + 500);

    assert_eq!(state.scrollback.len(), 1, "stalled turn should be archived");
    assert!(!state.scrollback[0].success);
    // New turn is fresh.
    assert_eq!(state.partial_transcript, "");
    assert_eq!(state.turn_id, Some("t2".to_string()));
}
