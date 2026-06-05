//! `wintermute-lucid` — the flight recorder for the agorabus bus.
//!
//! This is the foundational crate of the `lucid` subsystem. It subscribes to
//! the whole `wm.` prefix and persists every event to a rotating, turn-keyed
//! structured log that survives daemon and reboot death.
//!
//! The load-bearing pieces are:
//! - [`record::Record`] — the on-disk schema the rest of the `lucid` fleet
//!   (lucid-explain, lucid-live, lucid-mind, lucid-trace, lucid-turn-id)
//!   reads. Stable and additive-only.
//! - [`store::LucidStore`] — the file-backed, size-rotating, turn-indexed
//!   substrate that bounds disk use and survives restarts.
//! - [`trace`] — turn timeline reconstruction: `lucid trace <id>` and
//!   `lucid last [N]`.
//!
//! The bus-facing recorder and `tap` foreground mode live in the binary
//! (`src/main.rs`); they are thin glue over these modules plus the
//! `agorabus` client.

pub mod mind;
pub mod record;
pub mod store;
pub mod trace;

pub use mind::{assemble, decode_reason, most_recent_turn_id, render, TurnMind};
pub use record::{Record, SCHEMA_VERSION};
pub use store::{LucidStore, RecordLocation, RotationPolicy};
pub use trace::{
    render_summary, render_timeline, summarise_turns, trace_turn, TurnStatus, TurnSummary,
    TurnTimeline,
};

/// Current wall-clock time in milliseconds since the Unix epoch.
///
/// # Errors
/// Returns an error if the system clock is before the Unix epoch.
pub fn now_ms() -> anyhow::Result<u64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before unix epoch: {e}"))?;
    Ok(u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
}
