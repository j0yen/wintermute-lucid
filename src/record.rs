//! The on-disk record schema — the contract the rest of the `lucid` fleet
//! (lucid-explain, lucid-live, lucid-mind, lucid-trace, lucid-turn-id) reads.
//!
//! A record is one bus event as observed by the recorder. It is serialized
//! one-per-line as JSON (NDJSON) into rotating log segments. The schema is
//! intentionally small and additive-friendly: future fields are appended,
//! never renamed, so older readers keep parsing newer segments.

use serde::{Deserialize, Serialize};

/// Schema version stamped into every record. Bump only on a
/// backward-incompatible change (renaming/removing a field). Additive
/// changes keep the same version.
pub const SCHEMA_VERSION: u32 = 1;

/// A single recorded bus event.
///
/// `turn_id` is the primary correlation key when present (sourced from the
/// event payload, per lucid-turn-id). Events without one are still recorded
/// and bucketed under a synthetic key — see [`Record::correlation_key`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Schema version of this record.
    #[serde(default = "default_schema_version")]
    pub v: u32,
    /// Milliseconds since the Unix epoch at the moment the recorder received
    /// the event. This is the recorder's clock, distinct from any timestamp
    /// inside the payload, and is monotonic-enough for ordering within a
    /// segment.
    pub ts_received: u64,
    /// Dotted bus topic the event was published on (e.g. `wm.brain.reply`).
    pub topic: String,
    /// The correlation turn id extracted from the payload, if the publisher
    /// tagged the event (lucid-turn-id). `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// `session_id` of the publishing peer (the bus `from` field).
    pub from: String,
    /// The event payload exactly as published.
    pub raw_payload: serde_json::Value,
}

const fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Record {
    /// Build a record from the observed event parts, extracting `turn_id`
    /// from the payload if the publisher embedded one.
    #[must_use]
    pub fn new(ts_received: u64, topic: String, from: String, raw_payload: serde_json::Value) -> Self {
        let turn_id = extract_turn_id(&raw_payload);
        Self {
            v: SCHEMA_VERSION,
            ts_received,
            topic,
            turn_id,
            from,
            raw_payload,
        }
    }

    /// The bucket this record correlates under. Uses `turn_id` when present;
    /// otherwise a synthetic `untagged-<ts_received>` key so an event is
    /// never dropped for lacking a turn id (AC3).
    #[must_use]
    pub fn correlation_key(&self) -> String {
        match &self.turn_id {
            Some(t) => t.clone(),
            None => format!("untagged-{}", self.ts_received),
        }
    }

    /// Serialize to a single NDJSON line (no trailing newline).
    ///
    /// # Errors
    /// Returns an error if the record cannot be serialized to JSON.
    pub fn to_ndjson(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parse one NDJSON line back into a record.
    ///
    /// # Errors
    /// Returns an error if the line is not valid record JSON.
    pub fn from_ndjson(line: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(line)?)
    }
}

/// Pull a turn id out of a payload. Recognizes a top-level string field named
/// `turn_id` (the lucid-turn-id convention). Returns `None` for any other
/// shape so an untagged event flows through unchanged.
#[must_use]
pub fn extract_turn_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("turn_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}
