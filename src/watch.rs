//! `lucid watch` — live TUI monitor for the wintermute pipeline.
//!
//! Shows a pipeline row of the six ordered stages, a dialog FSM state line,
//! a streaming partial transcript, stall detection, and a completed-turn
//! scrollback — all updated in real time from agorabus events.
//!
//! The `--plain` flag falls back to a line-per-event NDJSON stream without
//! any TUI control codes.
//!
//! # Feature gating
//! The actual TUI rendering is guarded by `#[cfg(feature = "tui")]` so that
//! builds without the feature don't pull in ratatui/crossterm.

use anyhow::{Context, Result};
use std::path::Path;

// ─── Pipeline stage model ────────────────────────────────────────────────────

/// The six ordered pipeline stages, in activation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Wake,
    Capture,
    Stt,
    Dialog,
    Brain,
    Tts,
}

impl Stage {
    pub const ALL: &'static [Stage] = &[
        Stage::Wake,
        Stage::Capture,
        Stage::Stt,
        Stage::Dialog,
        Stage::Brain,
        Stage::Tts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Wake => "wake",
            Stage::Capture => "capture",
            Stage::Stt => "stt",
            Stage::Dialog => "dialog",
            Stage::Brain => "brain",
            Stage::Tts => "tts",
        }
    }
}

/// Visual state of a pipeline stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageState {
    #[default]
    /// Not yet activated this turn.
    Idle,
    /// Currently processing.
    Active,
    /// Completed successfully.
    Done,
    /// Failed (error topic received).
    Failed,
    /// Active longer than the stall threshold with no successor.
    Stalled,
}

/// Map a bus topic to the stage it activates / completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    StageStart(Stage),
    StageEnd(Stage),
    StageError(Stage),
    DialogState,
    SttPartial,
    TtsEnd,
    Unrelated,
}

/// Classify a bus topic into the pipeline model.
pub fn classify_topic(topic: &str) -> TopicKind {
    match topic {
        "wm.audio.wake" => TopicKind::StageStart(Stage::Wake),
        "wm.audio.capture.start" | "wm.audio.speech.start" => {
            TopicKind::StageStart(Stage::Capture)
        }
        "wm.audio.capture.end" | "wm.audio.speech.end" => TopicKind::StageEnd(Stage::Capture),
        "wm.audio.error" => TopicKind::StageError(Stage::Capture),
        "wm.stt.partial" => TopicKind::SttPartial,
        "wm.stt.final" | "wm.stt.uncertain" => TopicKind::StageEnd(Stage::Stt),
        "wm.stt.error" => TopicKind::StageError(Stage::Stt),
        "wm.dialog.state" => TopicKind::DialogState,
        "wm.dialog.confirm.denied" | "wm.dialog.confirm.granted" => {
            TopicKind::StageEnd(Stage::Dialog)
        }
        "wm.brain.reply" | "wm.brain.reply.destructive" => TopicKind::StageEnd(Stage::Brain),
        "wm.brain.error" => TopicKind::StageError(Stage::Brain),
        "wm.tts.end" => TopicKind::TtsEnd,
        "wm.tts.error" => TopicKind::StageError(Stage::Tts),
        _ => {
            // Infer stage from prefix for topics we don't explicitly know.
            if topic.starts_with("wm.stt.") {
                TopicKind::StageStart(Stage::Stt)
            } else if topic.starts_with("wm.dialog.") {
                TopicKind::StageStart(Stage::Dialog)
            } else if topic.starts_with("wm.brain.") {
                TopicKind::StageStart(Stage::Brain)
            } else if topic.starts_with("wm.tts.") {
                TopicKind::StageStart(Stage::Tts)
            } else {
                TopicKind::Unrelated
            }
        }
    }
}

// ─── Live state ──────────────────────────────────────────────────────────────

/// A completed turn entry in the scrollback.
#[derive(Debug, Clone)]
pub struct TurnEntry {
    pub turn_id: Option<String>,
    pub transcript: String,
    pub elapsed_ms: u64,
    pub success: bool,
    pub stall_stage: Option<Stage>,
}

impl TurnEntry {
    pub fn format_line(&self) -> String {
        if self.success {
            let secs = self.elapsed_ms as f64 / 1000.0;
            let t = if self.transcript.is_empty() {
                "(no transcript)".to_string()
            } else {
                format!("\"{}\"", self.transcript)
            };
            format!("✓ {t} {secs:.1}s")
        } else {
            let stage = self.stall_stage.map(|s| s.label()).unwrap_or("unknown");
            format!("✗ stalled @ {stage}")
        }
    }
}

/// Live pipeline state for the current (or most recent) turn.
#[derive(Debug, Default)]
pub struct PipelineState {
    /// Per-stage visual state.
    pub stages: [StageState; 6],
    /// Milliseconds since the start of the current turn (wake event).
    pub elapsed_ms: u64,
    /// Turn start wall-clock time (ms since epoch).
    pub turn_start_ms: Option<u64>,
    /// Active turn id, if any.
    pub turn_id: Option<String>,
    /// Current dialog FSM state string.
    pub dialog_state: String,
    /// Latest partial transcript.
    pub partial_transcript: String,
    /// Completed turns scrollback (bounded).
    pub scrollback: Vec<TurnEntry>,
    /// Stalled stage, if any.
    pub stall_stage: Option<Stage>,
    /// Time (ms epoch) each stage last became active.
    pub stage_active_since: [Option<u64>; 6],
}

impl PipelineState {
    const MAX_SCROLLBACK: usize = 20;
    /// Stages are considered stalled after this many ms with no successor.
    pub const STALL_THRESHOLD_MS: u64 = 8_000;

    fn stage_idx(s: Stage) -> usize {
        match s {
            Stage::Wake => 0,
            Stage::Capture => 1,
            Stage::Stt => 2,
            Stage::Dialog => 3,
            Stage::Brain => 4,
            Stage::Tts => 5,
        }
    }

    /// Apply a bus event to the live state.
    /// Returns true if the UI should re-render.
    pub fn apply(
        &mut self,
        topic: &str,
        payload: &serde_json::Value,
        now_ms: u64,
        stall_threshold_ms: u64,
    ) -> bool {
        let kind = classify_topic(topic);
        let ev_turn = payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);

        // New wake event → start a fresh turn.
        if kind == TopicKind::StageStart(Stage::Wake) {
            self.flush_stalled_turn(now_ms);
            self.reset_for_new_turn(now_ms, ev_turn);
            let i = Self::stage_idx(Stage::Wake);
            self.stages[i] = StageState::Active;
            self.stage_active_since[i] = Some(now_ms);
            return true;
        }

        // If no turn is active, only surface dialog-state updates.
        if self.turn_start_ms.is_none() {
            if kind == TopicKind::DialogState {
                if let Some(s) = payload.get("state").and_then(serde_json::Value::as_str) {
                    self.dialog_state = s.to_string();
                    return true;
                }
            }
            return false;
        }

        let changed = match kind {
            TopicKind::StageStart(s) => {
                let i = Self::stage_idx(s);
                if self.stages[i] == StageState::Idle || self.stages[i] == StageState::Stalled {
                    self.stages[i] = StageState::Active;
                    self.stage_active_since[i] = Some(now_ms);
                    // Mark the predecessor Done if it was still Active.
                    if i > 0 && self.stages[i - 1] == StageState::Active {
                        self.stages[i - 1] = StageState::Done;
                    }
                    true
                } else {
                    false
                }
            }
            TopicKind::StageEnd(s) => {
                let i = Self::stage_idx(s);
                self.stages[i] = StageState::Done;
                self.stage_active_since[i] = None;
                true
            }
            TopicKind::StageError(s) => {
                let i = Self::stage_idx(s);
                self.stages[i] = StageState::Failed;
                self.stage_active_since[i] = None;
                true
            }
            TopicKind::TtsEnd => {
                let i = Self::stage_idx(Stage::Tts);
                self.stages[i] = StageState::Done;
                let elapsed = now_ms.saturating_sub(self.turn_start_ms.unwrap_or(now_ms));
                let entry = TurnEntry {
                    turn_id: self.turn_id.clone(),
                    transcript: self.partial_transcript.clone(),
                    elapsed_ms: elapsed,
                    success: true,
                    stall_stage: None,
                };
                self.push_scrollback(entry);
                self.turn_start_ms = None;
                self.partial_transcript.clear();
                true
            }
            TopicKind::DialogState => {
                if let Some(s) = payload.get("state").and_then(serde_json::Value::as_str) {
                    self.dialog_state = s.to_string();
                    true
                } else {
                    false
                }
            }
            TopicKind::SttPartial => {
                let i = Self::stage_idx(Stage::Stt);
                if self.stages[i] == StageState::Idle {
                    self.stages[i] = StageState::Active;
                    self.stage_active_since[i] = Some(now_ms);
                    let ci = Self::stage_idx(Stage::Capture);
                    if self.stages[ci] == StageState::Active {
                        self.stages[ci] = StageState::Done;
                    }
                }
                if let Some(t) = payload.get("text").and_then(serde_json::Value::as_str) {
                    self.partial_transcript = t.to_string();
                }
                true
            }
            TopicKind::Unrelated => false,
        };

        if changed {
            if let Some(tid) = ev_turn {
                if self.turn_id.is_none() {
                    self.turn_id = Some(tid);
                }
            }
            self.elapsed_ms = now_ms.saturating_sub(self.turn_start_ms.unwrap_or(now_ms));
            self.check_stalls(now_ms, stall_threshold_ms);
        }
        changed
    }

    /// Check each active stage against the stall threshold.
    pub fn check_stalls(&mut self, now_ms: u64, threshold_ms: u64) {
        self.stall_stage = None;
        for (i, st) in self.stages.iter_mut().enumerate() {
            if *st == StageState::Active {
                if let Some(since) = self.stage_active_since[i] {
                    if now_ms.saturating_sub(since) > threshold_ms {
                        *st = StageState::Stalled;
                        self.stall_stage = Some(Stage::ALL[i]);
                    }
                }
            }
        }
    }

    /// Update elapsed time without triggering a full state change.
    pub fn tick(&mut self, now_ms: u64) {
        if self.turn_start_ms.is_some() {
            self.elapsed_ms = now_ms.saturating_sub(self.turn_start_ms.unwrap_or(now_ms));
        }
    }

    fn push_scrollback(&mut self, entry: TurnEntry) {
        if self.scrollback.len() >= Self::MAX_SCROLLBACK {
            self.scrollback.remove(0);
        }
        self.scrollback.push(entry);
    }

    fn reset_for_new_turn(&mut self, now_ms: u64, turn_id: Option<String>) {
        self.stages = [StageState::Idle; 6];
        self.stage_active_since = [None; 6];
        self.turn_start_ms = Some(now_ms);
        self.elapsed_ms = 0;
        self.turn_id = turn_id;
        self.stall_stage = None;
        self.partial_transcript.clear();
    }

    fn flush_stalled_turn(&mut self, now_ms: u64) {
        if self.turn_start_ms.is_none() {
            return;
        }
        let stall_stage = Stage::ALL
            .iter()
            .enumerate()
            .find_map(|(i, &s)| {
                if self.stages[i] == StageState::Active || self.stages[i] == StageState::Stalled {
                    Some(s)
                } else {
                    None
                }
            });
        let elapsed = now_ms.saturating_sub(self.turn_start_ms.unwrap_or(now_ms));
        let entry = TurnEntry {
            turn_id: self.turn_id.clone(),
            transcript: self.partial_transcript.clone(),
            elapsed_ms: elapsed,
            success: false,
            stall_stage,
        };
        self.push_scrollback(entry);
    }
}

// ─── Bus connection helper ───────────────────────────────────────────────────

pub(crate) async fn connect_watch(socket: &Path) -> Result<agorabus::Client> {
    let mut client = agorabus::Client::connect(socket)
        .await
        .with_context(|| format!("connecting to agorabus at {}", socket.display()))?;
    let pid = std::process::id();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    client
        .announce("wm-lucid-watch", pid, &cwd, "wm-lucid watch monitor")
        .await
        .context("announcing watch peer")?;
    client
        .subscribe("wm.")
        .await
        .context("subscribing to wm.")?;
    Ok(client)
}

// ─── Plain (non-TUI) mode ────────────────────────────────────────────────────

/// Run `lucid watch --plain`: stream one line per event to stdout.
pub async fn run_plain(socket: &Path, stall_threshold_ms: u64) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut client = connect_watch(socket).await?;
    let mut stdout = tokio::io::stdout();
    let mut state = PipelineState::default();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            ev = client.next_event() => {
                match ev? {
                    Some(event) => {
                        let now = crate::now_ms()?;
                        let changed = state.apply(&event.topic, &event.data, now, stall_threshold_ms);
                        if changed {
                            let stall = state
                                .stall_stage
                                .map(|s| format!("\"{}\"", s.label()))
                                .unwrap_or_else(|| "null".to_string());
                            let line = format!(
                                "{{\"topic\":{},\"dialog_state\":{},\"partial\":{},\"stall\":{}}}\n",
                                serde_json::to_string(&event.topic).unwrap_or_default(),
                                serde_json::to_string(&state.dialog_state).unwrap_or_default(),
                                serde_json::to_string(&state.partial_transcript).unwrap_or_default(),
                                stall,
                            );
                            if stdout.write_all(line.as_bytes()).await.is_err() {
                                return Ok(());
                            }
                            if stdout.flush().await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    None => {
                        client = connect_watch(socket).await?;
                    }
                }
            }
        }
    }
}

// ─── TUI mode ────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
pub async fn run_tui(socket: &Path, stall_threshold_ms: u64) -> Result<()> {
    use crossterm::{
        execute,
        terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        },
    };
    use std::io;

    enable_raw_mode().context("enable raw mode")?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen).context("enter alternate screen")?;

    let backend = ratatui::backend::CrosstermBackend::new(io::stderr());
    let mut terminal = ratatui::Terminal::new(backend).context("create terminal")?;

    let result = run_tui_inner(socket, stall_threshold_ms, &mut terminal).await;

    disable_raw_mode().ok();
    execute!(io::stderr(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

#[cfg(feature = "tui")]
async fn run_tui_inner(
    socket: &Path,
    stall_threshold_ms: u64,
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, List, ListItem, Paragraph},
    };
    use std::time::Duration;
    use tokio::sync::mpsc;

    let mut bus_client = connect_watch(socket).await?;
    let mut state = PipelineState::default();

    // Keyboard events are read in a blocking thread → sent over a channel.
    let (key_tx, mut key_rx) = mpsc::channel::<crossterm::event::KeyEvent>(16);
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(Event::Key(k)) = event::read() {
                    if key_tx.blocking_send(k).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(250));

    loop {
        // Render.
        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // pipeline row
                    Constraint::Length(3), // state line
                    Constraint::Min(4),    // scrollback
                    Constraint::Length(1), // help bar
                ])
                .split(area);

            // ── Pipeline row ──────────────────────────────────────────────
            let stage_spans: Vec<Span> = Stage::ALL
                .iter()
                .enumerate()
                .flat_map(|(i, &s)| {
                    let st = state.stages[i];
                    let (label_style, bracket_style) = match st {
                        StageState::Idle => (
                            Style::default().fg(Color::DarkGray),
                            Style::default().fg(Color::DarkGray),
                        ),
                        StageState::Active => (
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                            Style::default().fg(Color::Green),
                        ),
                        StageState::Done => (
                            Style::default().fg(Color::Cyan),
                            Style::default().fg(Color::DarkGray),
                        ),
                        StageState::Failed => (
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD | Modifier::RAPID_BLINK),
                            Style::default().fg(Color::Red),
                        ),
                        StageState::Stalled => (
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                            Style::default().fg(Color::Yellow),
                        ),
                    };
                    let mut spans = vec![
                        Span::styled("[", bracket_style),
                        Span::styled(s.label(), label_style),
                        Span::styled("]", bracket_style),
                    ];
                    if i < Stage::ALL.len() - 1 {
                        spans.push(Span::styled(
                            " → ",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    spans
                })
                .collect();

            let elapsed_str = if state.turn_start_ms.is_some() {
                format!("  {:.1}s", state.elapsed_ms as f64 / 1000.0)
            } else {
                String::new()
            };
            let mut all_spans = stage_spans;
            all_spans.push(Span::styled(
                elapsed_str,
                Style::default().fg(Color::White),
            ));

            let pipeline_block = Block::default().borders(Borders::ALL).title(" pipeline ");
            let pipeline_para =
                Paragraph::new(Line::from(all_spans)).block(pipeline_block);
            f.render_widget(pipeline_para, chunks[0]);

            // ── State line ────────────────────────────────────────────────
            let dialog = if state.dialog_state.is_empty() {
                "—".to_string()
            } else {
                state.dialog_state.clone()
            };
            let partial = if state.partial_transcript.is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", state.partial_transcript)
            };
            let turn_label = state
                .turn_id
                .as_deref()
                .map(|t| format!("  turn:{}", &t[..t.len().min(8)]))
                .unwrap_or_default();
            let stall_label = state
                .stall_stage
                .map(|s| {
                    format!(
                        "  ⚠ {} active {:.0}s — no successor",
                        s.label(),
                        stall_threshold_ms as f64 / 1000.0,
                    )
                })
                .unwrap_or_default();

            let state_line =
                format!("state: {dialog}{turn_label}{partial}{stall_label}");
            let state_block = Block::default().borders(Borders::ALL).title(" dialog ");
            let state_para = Paragraph::new(state_line).block(state_block);
            f.render_widget(state_para, chunks[1]);

            // ── Scrollback ────────────────────────────────────────────────
            let items: Vec<ListItem> = state
                .scrollback
                .iter()
                .rev()
                .map(|entry| {
                    let style = if entry.success {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red)
                    };
                    ListItem::new(entry.format_line()).style(style)
                })
                .collect();
            let scrollback_block = Block::default().borders(Borders::ALL).title(" turns ");
            let scrollback_list = List::new(items).block(scrollback_block);
            f.render_widget(scrollback_list, chunks[2]);

            // ── Help bar ──────────────────────────────────────────────────
            let help = Paragraph::new(" q/Ctrl-C: quit   stall-threshold: 8s");
            f.render_widget(help, chunks[3]);
        })?;

        // Event loop: bus events, keyboard, or tick for stall checks / elapsed.
        tokio::select! {
            _ = tick.tick() => {
                let now = crate::now_ms().unwrap_or(0);
                state.check_stalls(now, stall_threshold_ms);
                state.tick(now);
            }
            ev = bus_client.next_event() => {
                match ev? {
                    Some(event) => {
                        let now = crate::now_ms().unwrap_or(0);
                        state.apply(&event.topic, &event.data, now, stall_threshold_ms);
                    }
                    None => {
                        bus_client = connect_watch(socket).await?;
                    }
                }
            }
            key_ev = key_rx.recv() => {
                match key_ev {
                    Some(k) => {
                        match k.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('c')
                                if k.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(not(feature = "tui"))]
pub async fn run_tui(_socket: &Path, _stall_threshold_ms: u64) -> Result<()> {
    anyhow::bail!(
        "TUI support not compiled in; rebuild with --features tui or use --plain"
    );
}
