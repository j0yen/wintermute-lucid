//! End-to-end tests for the two `lucid explain` acceptance criteria that have
//! a world-side component the pure-narration unit tests in `src/explain.rs`
//! cannot reach:
//!
//! - **AC4** (`--voice`): explaining a turn with `--voice` publishes the
//!   narration text to `wm.tts.say`, and *without* `--voice` publishes
//!   nothing. Verified by capturing the event off a real in-process agorabus
//!   bus (PRD criterion 4: "verifiable by capturing the bus event").
//! - **AC6** (`--last`): `explain --last` with no id argument narrates the
//!   most-recently-recorded turn — proven by seeding two turns and asserting
//!   `--last` resolves to (and produces byte-identical narration to) the
//!   newer one's explicit id.
//!
//! Both drive the shipped `wm-lucid` binary (`CARGO_BIN_EXE_wm-lucid`) against
//! a seeded on-disk store, so they exercise arg parsing, store resolution, and
//! the publish path exactly as a user would. The bus harness mirrors the
//! `wake_bus_smoke` / `vad_bus_smoke` in-process-daemon pattern.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_assert_message
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agorabus::{Client, DaemonConfig, run_daemon};
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::timeout;

use wintermute_lucid::{LucidStore, Record, RotationPolicy};

// ── Store seeding ────────────────────────────────────────────────────────────

/// One record with `turn_id` (and any extra fields) folded into the payload —
/// the same shape `wm-lucid record` persists from a live bus event.
fn rec(ts: u64, topic: &str, turn: &str, extra: serde_json::Value) -> Record {
    let mut payload = json!({ "turn_id": turn });
    if let (Some(obj), Some(ext)) = (payload.as_object_mut(), extra.as_object()) {
        for (k, v) in ext {
            obj.insert(k.clone(), v.clone());
        }
    }
    Record::new(ts, topic.to_string(), "peer".to_string(), payload)
}

/// A complete, successful turn (wake → reply → tts.end) anchored at `base_ts`,
/// so two turns seeded with different `base_ts` have an unambiguous recency
/// ordering for the `--last` resolution.
fn happy_turn(turn: &str, utterance: &str, base_ts: u64) -> Vec<Record> {
    vec![
        rec(base_ts, "wm.audio.wake", turn, json!({ "score": 0.99 })),
        rec(base_ts + 18, "wm.audio.speech.start", turn, json!({})),
        rec(base_ts + 1258, "wm.audio.speech.end", turn, json!({})),
        rec(base_ts + 3968, "wm.stt.final", turn, json!({ "text": utterance })),
        rec(base_ts + 3980, "wm.dialog.turn.user", turn, json!({})),
        rec(
            base_ts + 3988,
            "wm.brain.route",
            turn,
            json!({ "tier": "sonnet", "model": "claude-sonnet-4-6" }),
        ),
        rec(base_ts + 6133, "wm.brain.reply", turn, json!({ "text": "It's about 3:45" })),
        rec(base_ts + 6158, "wm.tts.start", turn, json!({})),
        rec(base_ts + 8418, "wm.tts.end", turn, json!({})),
    ]
}

fn seed_store(dir: &Path, turns: &[Vec<Record>]) {
    let mut store = LucidStore::open(dir, RotationPolicy::default()).expect("open store");
    for turn in turns {
        for r in turn {
            store.append(r).expect("append record");
        }
    }
}

// ── Binary driver ────────────────────────────────────────────────────────────

/// Run the shipped `wm-lucid` binary with the given subcommand args, pointing
/// it at `data_dir` (and `socket`, when the path is supplied). Returns the
/// captured stdout. Blocking — call inside `spawn_blocking`.
fn run_lucid(data_dir: &Path, socket: Option<&Path>, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wm-lucid"));
    cmd.arg("--data-dir").arg(data_dir);
    if let Some(s) = socket {
        cmd.arg("--socket").arg(s);
    }
    cmd.args(args);
    let out = cmd.output().expect("spawn wm-lucid");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── In-process bus ───────────────────────────────────────────────────────────

struct Bus {
    socket: PathBuf,
    _tmp: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Bus {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("bus tempdir");
        let socket = tmp.path().join("sock");
        let cfg = DaemonConfig {
            socket_path: socket.clone(),
            heartbeat_timeout: Duration::from_secs(60),
            broadcast_capacity: 256,
            drain_grace_ms: agorabus::DEFAULT_DRAIN_GRACE_MS,
            drain_resume_hint_ms: agorabus::DEFAULT_DRAIN_RESUME_HINT_MS,
            state_file: tmp.path().join("state.json"),
            state_flush_ms: agorabus::DEFAULT_STATE_FLUSH_MS,
        };
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move { run_daemon(cfg, Some(ready_tx), shutdown_rx).await });
        timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("bus ready timeout")
            .expect("bus ready");
        Self { socket, _tmp: tmp, shutdown: Some(shutdown_tx), join: Some(join) }
    }

    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

/// Connect a subscriber to `topic`. The agorabus `Client` buffers events that
/// arrive before the next `next_event()` await, so subscribing before the
/// binary publishes is sufficient to never miss the event.
async fn subscribe(socket: &Path, topic: &str) -> Client {
    let mut c = Client::connect(socket).await.expect("subscriber connect");
    c.announce("explain-voice-smoke-sub", std::process::id(), "", "test-subscriber")
        .await
        .expect("subscriber announce");
    c.subscribe(topic).await.expect("subscribe");
    c
}

// ── AC4: --voice publishes the narration to wm.tts.say ───────────────────────

#[test]
fn ac4_voice_publishes_narration_to_tts_say() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let store_dir = tempfile::tempdir().expect("store tempdir");
        seed_store(store_dir.path(), &[happy_turn("turn-voice", "what time is it", 1_000)]);

        let bus = Bus::start().await;
        let mut sub = subscribe(&bus.socket, "wm.tts.say").await;

        // Run `explain turn-voice --voice`. stdout carries the same narration
        // text the binary publishes, so we can assert byte-equality.
        let data = store_dir.path().to_path_buf();
        let sock = bus.socket.clone();
        let (ok, stdout) = tokio::task::spawn_blocking(move || {
            run_lucid(&data, Some(&sock), &["explain", "turn-voice", "--voice", "--persona", "hearth"])
        })
        .await
        .expect("join blocking");
        assert!(ok, "wm-lucid explain --voice exited non-zero");
        let narration = stdout.trim().to_string();
        assert!(
            narration.contains("what time is it"),
            "stdout narration should name the utterance, got: {narration:?}"
        );

        // The wm.tts.say event must carry exactly that narration text.
        let ev = timeout(Duration::from_secs(5), sub.next_event())
            .await
            .expect("timed out waiting for wm.tts.say")
            .expect("next_event error")
            .expect("bus closed before event");
        assert_eq!(ev.topic, "wm.tts.say");
        let spoken = ev
            .data
            .get("text")
            .and_then(serde_json::Value::as_str)
            .expect("wm.tts.say payload missing text");
        assert_eq!(spoken, narration, "spoken text must equal printed narration");

        bus.shutdown().await;
    });
}

#[test]
fn ac4_without_voice_publishes_nothing() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let store_dir = tempfile::tempdir().expect("store tempdir");
        seed_store(store_dir.path(), &[happy_turn("turn-silent", "what time is it", 1_000)]);

        let bus = Bus::start().await;
        let mut sub = subscribe(&bus.socket, "wm.tts.say").await;

        let data = store_dir.path().to_path_buf();
        let sock = bus.socket.clone();
        let (ok, stdout) = tokio::task::spawn_blocking(move || {
            run_lucid(&data, Some(&sock), &["explain", "turn-silent"])
        })
        .await
        .expect("join blocking");
        assert!(ok, "wm-lucid explain exited non-zero");
        assert!(stdout.contains("what time is it"), "narration still printed to stdout");

        // No --voice → nothing on the bus. A quiet window with no event is the
        // pass condition; getting one is the failure.
        match timeout(Duration::from_millis(800), sub.next_event()).await {
            Err(_) => {} // quiet — correct: nothing was spoken
            Ok(Ok(Some(ev))) => panic!("unexpected publish without --voice: {}", ev.topic),
            Ok(Ok(None)) => {} // bus closed, also fine
            Ok(Err(e)) => panic!("subscriber error: {e:#}"),
        }

        bus.shutdown().await;
    });
}

// ── AC6: --last narrates the most-recently-recorded turn ─────────────────────

#[test]
fn ac6_last_resolves_to_most_recent_turn() {
    let store_dir = tempfile::tempdir().expect("store tempdir");
    // Older turn first, newer turn anchored far later so recency is unambiguous.
    seed_store(
        store_dir.path(),
        &[
            happy_turn("turn-old", "what time is it", 1_000),
            happy_turn("turn-new", "what is the weather", 500_000),
        ],
    );

    // `--last` with no id, JSON for an exact comparison.
    let (ok_last, last_json) = run_lucid(store_dir.path(), None, &["explain", "--last", "--json"]);
    assert!(ok_last, "explain --last exited non-zero");
    let last: serde_json::Value = serde_json::from_str(&last_json).expect("parse --last json");

    // It must resolve to the newer turn, not the older one.
    assert_eq!(
        last.get("turn_id").and_then(serde_json::Value::as_str),
        Some("turn-new"),
        "--last should pick the most recent turn"
    );
    let text = last.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
    assert!(
        text.contains("what is the weather"),
        "--last narration should describe the newer turn, got: {text:?}"
    );
    assert!(
        !text.contains("what time is it"),
        "--last must not narrate the older turn"
    );

    // And it must be byte-identical to explaining that turn by explicit id —
    // proving `--last` is exactly "resolve newest, then explain".
    let (ok_id, id_json) =
        run_lucid(store_dir.path(), None, &["explain", "turn-new", "--json"]);
    assert!(ok_id, "explain turn-new exited non-zero");
    assert_eq!(last_json, id_json, "--last must equal explicit-id explain of the newest turn");
}
