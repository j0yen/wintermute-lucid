//! `wm-lucid` — the agorabus flight recorder.
//!
//! Subcommands:
//! - `record` (default): subscribe to the `wm.` prefix, persist every event
//!   to the rotating store. This is what the systemd unit runs.
//! - `tap [--topic <prefix>]`: tail live events to stdout as one JSON object
//!   per line — the first-class replacement for ad-hoc `agorabus subscribe`
//!   one-shots.
//! - `trace <turn_id> [--json] [--full]`: reconstruct a single turn as a
//!   stage-by-stage timeline with per-stage latency (lucid-trace AC1-7).
//! - `last [N] [--json]`: trace the most recent turn (or last N turns as
//!   one-line summaries) — lucid-trace AC5.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agorabus::Client;
use wintermute_lucid::{
    now_ms, render_summary, render_timeline, summarise_turns, trace_turn, LucidStore, Record,
    RotationPolicy, TurnStatus,
};

/// The intent tag this recorder announces itself with on the bus (AC1).
const LUCID_INTENT: &str = "wm-lucid recorder";
/// The session id the recorder registers under.
const LUCID_SESSION: &str = "wm-lucid";
/// The prefix the recorder subscribes to: the whole wintermute namespace.
const WM_PREFIX: &str = "wm.";

#[derive(Parser)]
#[command(name = "wm-lucid", version, about = "Flight recorder for the agorabus bus")]
struct Cli {
    /// Override the data directory (default: $XDG_CACHE_HOME/wintermute/lucid
    /// or ~/.cache/wintermute/lucid).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Override the agorabus socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the recorder daemon: subscribe to `wm.` and persist every event.
    Record {
        /// Max segment size in bytes before rotation.
        #[arg(long)]
        max_segment_bytes: Option<u64>,
        /// Max number of segments to retain.
        #[arg(long)]
        max_segments: Option<usize>,
    },
    /// Tail live events to stdout as one JSON object per line.
    Tap {
        /// Restrict to topics beginning with this prefix (default: `wm.`).
        #[arg(long)]
        topic: Option<String>,
    },
    /// Reconstruct a single turn as a stage-by-stage latency timeline.
    Trace {
        /// The turn correlation id to trace.
        turn_id: String,
        /// Emit structured JSON instead of the human-readable timeline.
        #[arg(long)]
        json: bool,
        /// Include the raw payload for every event row.
        #[arg(long)]
        full: bool,
    },
    /// Trace the most recent turn; or show the last N turns as one-line summaries.
    Last {
        /// Number of turns to show (default: 1, which traces in full).
        n: Option<usize>,
        /// Emit structured JSON instead of the human-readable format.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Toolkit convention: reset SIGPIPE so `wm-lucid tap | head` exits
    // cleanly instead of coredumping (AC6).
    sigpipe::reset();

    let cli = Cli::parse();
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(agorabus::default_socket_path);

    match cli.cmd.unwrap_or(Cmd::Record {
        max_segment_bytes: None,
        max_segments: None,
    }) {
        Cmd::Record {
            max_segment_bytes,
            max_segments,
        } => {
            let dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(LucidStore::default_data_dir);
            let mut policy = RotationPolicy::default();
            if let Some(b) = max_segment_bytes {
                policy.max_segment_bytes = b;
            }
            if let Some(s) = max_segments {
                policy.max_segments = s;
            }
            run_record(&socket, dir, policy).await
        }
        Cmd::Tap { topic } => {
            let prefix = topic.unwrap_or_else(|| WM_PREFIX.to_string());
            run_tap(&socket, &prefix).await
        }
        Cmd::Trace { turn_id, json, full } => {
            let dir = cli
                .data_dir
                .unwrap_or_else(LucidStore::default_data_dir);
            run_trace(&dir, &turn_id, json, full)
        }
        Cmd::Last { n, json } => {
            let dir = cli
                .data_dir
                .unwrap_or_else(LucidStore::default_data_dir);
            run_last(&dir, n.unwrap_or(1), json)
        }
    }
}

// ---------------------------------------------------------------------------
// trace subcommand
// ---------------------------------------------------------------------------

fn run_trace(dir: &std::path::Path, turn_id: &str, as_json: bool, full: bool) -> Result<()> {
    let store = open_store_ro(dir)?;
    let records = store.records_for(turn_id).context("reading records for turn")?;
    if records.is_empty() {
        eprintln!("wm-lucid: no records for turn {turn_id}");
        std::process::exit(1);
    }

    let tl = trace_turn(turn_id, &records)
        .ok_or_else(|| anyhow::anyhow!("no records for turn {turn_id}"))?;

    if as_json {
        if full {
            // Build enriched timeline with raw payloads merged in.
            let enriched = build_full_json(turn_id, &records, &tl);
            println!("{}", serde_json::to_string_pretty(&enriched)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&tl)?);
        }
    } else if full {
        // Human-readable with raw payloads appended per row.
        print_full_timeline(turn_id, &records, &tl);
    } else {
        print!("{}", render_timeline(&tl));
    }

    // Exit non-zero for failed/stalled turns (useful for scripting).
    if !matches!(tl.status, TurnStatus::Completed { .. }) {
        std::process::exit(2);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// last subcommand
// ---------------------------------------------------------------------------

fn run_last(dir: &std::path::Path, n: usize, as_json: bool) -> Result<()> {
    let store = open_store_ro(dir)?;

    // Collect all records grouped by turn_id, preserving insertion order for
    // the "most recent" semantics. We need to scan all records for recency.
    let all_records = store.read_all().context("reading all records")?;
    if all_records.is_empty() {
        eprintln!("wm-lucid: no records in store");
        return Ok(());
    }

    // Group by turn_id, tracking the maximum ts_received per turn for sorting.
    let mut by_turn: BTreeMap<String, (u64, Vec<Record>)> = BTreeMap::new();
    for rec in all_records {
        let key = rec.correlation_key();
        let entry = by_turn.entry(key).or_insert((0, Vec::new()));
        if rec.ts_received > entry.0 {
            entry.0 = rec.ts_received;
        }
        entry.1.push(rec);
    }

    // Sort by most-recent-ts descending, take N.
    let mut turns: Vec<(String, u64, Vec<Record>)> = by_turn
        .into_iter()
        .map(|(k, (ts, recs))| (k, ts, recs))
        .collect();
    turns.sort_by(|a, b| b.1.cmp(&a.1));
    turns.truncate(n);

    if n == 1 {
        // Full trace for the single most-recent turn.
        let (turn_id, _, records) = &turns[0];
        let tl = match trace_turn(turn_id, records) {
            Some(t) => t,
            None => {
                eprintln!("wm-lucid: no records for most recent turn");
                return Ok(());
            }
        };
        if as_json {
            println!("{}", serde_json::to_string_pretty(&tl)?);
        } else {
            print!("{}", render_timeline(&tl));
        }
        if !matches!(tl.status, TurnStatus::Completed { .. }) {
            std::process::exit(2);
        }
    } else {
        // One-line summaries for N turns.
        let pairs: Vec<(String, Vec<Record>)> = turns
            .into_iter()
            .map(|(id, _, recs)| (id, recs))
            .collect();
        let summaries = summarise_turns(&pairs);

        if as_json {
            println!("{}", serde_json::to_string_pretty(&summaries)?);
        } else {
            for s in &summaries {
                println!("{}", render_summary(s));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open the store read-only (no appends needed for trace/last).
fn open_store_ro(dir: &std::path::Path) -> Result<LucidStore> {
    // LucidStore::open creates the dir if absent — that's fine for a query
    // command; it just finds zero segments.
    LucidStore::open(dir, RotationPolicy::default())
        .with_context(|| format!("opening store at {}", dir.display()))
}

/// Build a JSON object that merges per-row raw payloads into the timeline for
/// `--json --full`.
fn build_full_json(
    turn_id: &str,
    records: &[Record],
    tl: &wintermute_lucid::TurnTimeline,
) -> serde_json::Value {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| r.ts_received);
    let t0 = sorted.first().map(|r| r.ts_received).unwrap_or(0);

    let rows: Vec<serde_json::Value> = tl
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let raw = sorted.get(i).map(|r| r.raw_payload.clone());
            let mut v = serde_json::json!({
                "offset_ms": row.offset_ms,
                "topic": row.topic,
            });
            if let Some(d) = row.stage_duration_ms {
                v["stage_duration_ms"] = serde_json::json!(d);
            }
            if let Some(a) = &row.annotation {
                v["annotation"] = serde_json::json!(a);
            }
            if let Some(payload) = raw {
                v["raw_payload"] = payload;
            }
            v
        })
        .collect();

    serde_json::json!({
        "turn_id": turn_id,
        "t0_ms": t0,
        "rows": rows,
        "status": tl.status,
        "utterance": tl.utterance,
    })
}

/// Print human-readable timeline with raw payloads inline.
fn print_full_timeline(turn_id: &str, records: &[Record], tl: &wintermute_lucid::TurnTimeline) {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| r.ts_received);

    let utterance = tl.utterance.as_deref().unwrap_or("(unknown)");
    println!("turn {turn_id}   \"{utterance}\"");
    for (i, row) in tl.rows.iter().enumerate() {
        let offset = format!("+{}ms", row.offset_ms);
        let dur_part = row
            .stage_duration_ms
            .map(|d| format!("({:.2}s stage)", d as f64 / 1000.0))
            .unwrap_or_default();
        let ann_part = row.annotation.as_deref().unwrap_or("");
        println!(
            "  {offset:<10}  {:<35}{}{}",
            row.topic,
            if dur_part.is_empty() { "".to_string() } else { format!("  {dur_part}") },
            if ann_part.is_empty() { "".to_string() } else { format!("  {ann_part}") },
        );
        if let Some(rec) = sorted.get(i) {
            println!("             payload: {}", rec.raw_payload);
        }
    }
    match &tl.status {
        TurnStatus::Completed { total_ms } => {
            println!("  ✓ completed  (end-to-end {:.2}s)", *total_ms as f64 / 1000.0);
        }
        TurnStatus::Stalled { last_topic, expected_next } => {
            let next = expected_next.as_deref().unwrap_or("(unknown)");
            println!("  ✗ stalled after {last_topic} — no {next}");
        }
        TurnStatus::Failed { failure_topic, last_good_topic } => {
            let last_good = last_good_topic.as_deref().unwrap_or("(start)");
            println!("  ✗ failed at {failure_topic} (after {last_good})");
        }
    }
}

// ---------------------------------------------------------------------------
// Record subcommand helpers
// ---------------------------------------------------------------------------

/// Connect, announce with the recorder intent, and subscribe to `prefix`.
async fn connect_and_subscribe(socket: &std::path::Path, prefix: &str) -> Result<Client> {
    let mut client = Client::connect(socket)
        .await
        .with_context(|| format!("connecting to agorabus at {}", socket.display()))?;
    let pid = std::process::id();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    client
        .announce(LUCID_SESSION, pid, &cwd, LUCID_INTENT)
        .await
        .context("announcing recorder peer")?;
    client
        .subscribe(prefix)
        .await
        .with_context(|| format!("subscribing to {prefix}"))?;
    Ok(client)
}

/// The recorder daemon loop: persist every event to the store.
async fn run_record(socket: &std::path::Path, dir: PathBuf, policy: RotationPolicy) -> Result<()> {
    let mut store = LucidStore::open(&dir, policy)
        .with_context(|| format!("opening store at {}", dir.display()))?;
    let mut client = connect_and_subscribe(socket, WM_PREFIX).await?;

    eprintln!(
        "wm-lucid: recording {WM_PREFIX} -> {} (max {} bytes/seg, {} segs)",
        dir.display(),
        policy.max_segment_bytes,
        policy.max_segments
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("wm-lucid: shutdown signal, flushing and exiting");
                return Ok(());
            }
            ev = client.next_event() => {
                match ev? {
                    Some(event) => {
                        let rec = Record::new(now_ms()?, event.topic, event.from, event.data);
                        store.append(&rec).context("persisting event")?;
                    }
                    None => {
                        // Stream EOF (daemon drain). Reconnect.
                        eprintln!("wm-lucid: bus stream ended, reconnecting");
                        client = connect_and_subscribe(socket, WM_PREFIX).await?;
                    }
                }
            }
        }
    }
}

/// The `tap` foreground mode: stream live events to stdout as NDJSON.
async fn run_tap(socket: &std::path::Path, prefix: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut client = connect_and_subscribe(socket, prefix).await?;
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            ev = client.next_event() => {
                match ev? {
                    Some(event) => {
                        let rec = Record::new(now_ms()?, event.topic, event.from, event.data);
                        let mut line = rec.to_ndjson()?;
                        line.push('\n');
                        // A closed downstream pipe surfaces as a write error;
                        // exit cleanly rather than propagating a panic (AC6).
                        if stdout.write_all(line.as_bytes()).await.is_err() {
                            return Ok(());
                        }
                        if stdout.flush().await.is_err() {
                            return Ok(());
                        }
                    }
                    None => {
                        client = connect_and_subscribe(socket, prefix).await?;
                    }
                }
            }
        }
    }
}
