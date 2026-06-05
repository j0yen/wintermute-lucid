//! `wm-lucid` — the agorabus flight recorder.
//!
//! Subcommands:
//! - `record` (default): subscribe to the `wm.` prefix, persist every event
//!   to the rotating store. This is what the systemd unit runs.
//! - `tap [--topic <prefix>]`: tail live events to stdout as one JSON object
//!   per line — the first-class replacement for ad-hoc `agorabus subscribe`
//!   one-shots.
//! - `mind <turn_id>`: show the brain's reasoning for a recorded turn —
//!   route decision, injected context, tool calls+results, and final reply.
//! - `why`: `mind` for the most recently recorded turn.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agorabus::Client;
use wintermute_lucid::{assemble, most_recent_turn_id, now_ms, render, LucidStore, Record, RotationPolicy};

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
    /// Show the brain's reasoning for a specific recorded turn.
    Mind {
        /// The turn id to inspect.
        turn_id: String,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Show the brain's reasoning for the most recently recorded turn.
    Why {
        /// Emit JSON instead of human-readable text.
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
        Cmd::Mind { turn_id, json } => {
            let dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(LucidStore::default_data_dir);
            run_mind(&dir, &turn_id, json)
        }
        Cmd::Why { json } => {
            let dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(LucidStore::default_data_dir);
            run_why(&dir, json)
        }
    }
}

/// Show the brain's reasoning for `turn_id`.
fn run_mind(dir: &std::path::Path, turn_id: &str, json_out: bool) -> Result<()> {
    let store = LucidStore::open(dir, RotationPolicy::default())
        .with_context(|| format!("opening store at {}", dir.display()))?;
    match assemble(&store, turn_id)? {
        Some(mind) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&mind)?);
            } else {
                print!("{}", render(&mind));
            }
            Ok(())
        }
        None => {
            eprintln!("wm-lucid mind: no records found for turn_id {turn_id:?}");
            std::process::exit(1);
        }
    }
}

/// Show the brain's reasoning for the most recently recorded turn.
fn run_why(dir: &std::path::Path, json_out: bool) -> Result<()> {
    let store = LucidStore::open(dir, RotationPolicy::default())
        .with_context(|| format!("opening store at {}", dir.display()))?;
    match most_recent_turn_id(&store)? {
        Some(turn_id) => run_mind(dir, &turn_id, json_out),
        None => {
            eprintln!("wm-lucid why: no turns recorded yet");
            std::process::exit(1);
        }
    }
}

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
