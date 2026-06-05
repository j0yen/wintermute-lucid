//! `lucid mind` — brain-reasoning reader for a single turn.
//!
//! Reconstructs the brain's decision for a given `turn_id` by reading the
//! already-recorded events from the lucid store:
//!
//! - `wm.brain.route`         → tier, model, reason, latency
//! - `wm.brain.tool.call`     → tools the brain invoked
//! - `wm.brain.tool.result`   → tool results (matched by call order)
//! - `wm.brain.reply`         → the final reply text
//! - `wm.brain.reply.destructive` → destructive-variant reply
//! - `wm.brain.context`       → injected recall context (optional, not yet published)
//!
//! All rendering is purely textual; no live bus connection is required.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::LucidStore;

// ── Topic constants ────────────────────────────────────────────────────────────

const TOPIC_ROUTE: &str = "wm.brain.route";
const TOPIC_TOOL_CALL: &str = "wm.brain.tool.call";
const TOPIC_TOOL_RESULT: &str = "wm.brain.tool.result";
const TOPIC_REPLY: &str = "wm.brain.reply";
const TOPIC_REPLY_DESTRUCTIVE: &str = "wm.brain.reply.destructive";
const TOPIC_CONTEXT: &str = "wm.brain.context";

// ── Route event shape ──────────────────────────────────────────────────────────

/// The `wm.brain.route` payload as published by the brain router.
/// Fields are optional because older recordings may not have all fields.
#[derive(Debug, Clone, Deserialize)]
pub struct RoutePayload {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
}

/// The `wm.brain.context` payload — injected recall context digest.
/// Optional/absent for turns recorded before adoption.
#[derive(Debug, Clone, Deserialize)]
pub struct ContextPayload {
    #[serde(default)]
    pub recall_hit_subjects: Option<Vec<String>>,
    #[serde(default)]
    pub recall_hit_count: Option<u32>,
    #[serde(default)]
    pub persona_tier: Option<String>,
    #[serde(default)]
    pub history_turn_count: Option<u32>,
}

/// One tool call paired with its result.
#[derive(Debug, Clone, Serialize)]
pub struct ToolPair {
    /// The tool name (from `wm.brain.tool.call`).
    pub tool: String,
    /// The call arguments (from `wm.brain.tool.call`).
    pub args: Value,
    /// `Some(ok, body)` if a matching result was found.
    pub result: Option<(bool, Value)>,
    /// `ts_received` of the call record (for display ordering).
    pub call_ts: u64,
}

/// Assembled reasoning view for a single turn.
#[derive(Debug, Clone, Serialize)]
pub struct TurnMind {
    pub turn_id: String,
    // ── Route ──────────────────────────────────────────────────────────────
    pub tier: Option<String>,
    pub model: Option<String>,
    pub reason_raw: Option<String>,
    pub reason_human: Option<String>,
    pub latency_ms: Option<u64>,
    // ── Context (from wm.brain.context, deferred) ──────────────────────────
    pub context_available: bool,
    pub recall_subjects: Vec<String>,
    pub recall_hit_count: Option<u32>,
    pub persona_tier: Option<String>,
    pub history_turn_count: Option<u32>,
    // ── Tools ──────────────────────────────────────────────────────────────
    pub tools: Vec<ToolPair>,
    // ── Reply ──────────────────────────────────────────────────────────────
    pub reply_text: Option<String>,
    pub reply_destructive: bool,
}

// ── Reason decoder ─────────────────────────────────────────────────────────────

/// Translate a machine-readable `reason` string from `wm.brain.route`
/// into a plain-English explanation suitable for human inspection.
#[must_use]
pub fn decode_reason(reason: &str) -> String {
    match reason {
        "command" => "turn classified as command/control → low-latency local path".to_string(),
        "conversational" => {
            "turn classified as conversational, API key present, online → cloud for quality"
                .to_string()
        }
        "no_key" => "conversational turn but no Anthropic API key → forced local".to_string(),
        "offline" => "conversational turn but network offline → forced local".to_string(),
        "cloud_timeout_fallback" => {
            "cloud exceeded latency budget → fell back to local tier".to_string()
        }
        "total_failure" => {
            "both local and cloud unavailable → canned degrade phrase used".to_string()
        }
        "prefer_local_only" => "deployment-wide prefer=local-only override in force".to_string(),
        "prefer_cloud_only" => "deployment-wide prefer=cloud-only override in force".to_string(),
        "override" => "per-turn model override set via `wmd swap-model`".to_string(),
        other => format!("unknown reason code: {other}"),
    }
}

// ── Assembler ──────────────────────────────────────────────────────────────────

/// Assemble a [`TurnMind`] for `turn_id` from the records in `store`.
///
/// Reads all records under the correlation key, filters by topic, and
/// assembles the view. Degrades gracefully when `wm.brain.context` is absent.
///
/// # Errors
/// Returns an error if the store cannot be read.
pub fn assemble(store: &LucidStore, turn_id: &str) -> anyhow::Result<Option<TurnMind>> {
    let records = store.records_for(turn_id)?;
    if records.is_empty() {
        return Ok(None);
    }

    let mut route: Option<RoutePayload> = None;
    let mut context: Option<ContextPayload> = None;
    let mut tool_calls: Vec<(u64, String, Value)> = Vec::new();
    let mut tool_results: Vec<(u64, String, bool, Value)> = Vec::new();
    let mut reply_text: Option<String> = None;
    let mut reply_destructive = false;

    for rec in &records {
        match rec.topic.as_str() {
            TOPIC_ROUTE => {
                if let Ok(r) = serde_json::from_value::<RoutePayload>(rec.raw_payload.clone()) {
                    route = Some(r);
                }
            }
            TOPIC_CONTEXT => {
                if let Ok(c) = serde_json::from_value::<ContextPayload>(rec.raw_payload.clone()) {
                    context = Some(c);
                }
            }
            TOPIC_TOOL_CALL => {
                let tool = rec
                    .raw_payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = rec
                    .raw_payload
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Null);
                if !tool.is_empty() {
                    tool_calls.push((rec.ts_received, tool, args));
                }
            }
            TOPIC_TOOL_RESULT => {
                let tool = rec
                    .raw_payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let ok = rec
                    .raw_payload
                    .get("ok")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let body = rec
                    .raw_payload
                    .get("body")
                    .cloned()
                    .unwrap_or(Value::Null);
                if !tool.is_empty() {
                    tool_results.push((rec.ts_received, tool, ok, body));
                }
            }
            TOPIC_REPLY => {
                if let Some(t) = rec.raw_payload.get("text").and_then(Value::as_str) {
                    reply_text = Some(t.to_string());
                    reply_destructive = false;
                }
            }
            TOPIC_REPLY_DESTRUCTIVE => {
                if let Some(t) = rec.raw_payload.get("text").and_then(Value::as_str) {
                    reply_text = Some(t.to_string());
                    reply_destructive = true;
                }
            }
            _ => {}
        }
    }

    // Sort tool calls by ts_received for stable ordering.
    tool_calls.sort_by_key(|t| t.0);

    // Pair each call with the first result that matches by tool name,
    // in call order. Each result is consumed at most once.
    let mut used_results: Vec<bool> = vec![false; tool_results.len()];
    let mut tools: Vec<ToolPair> = Vec::new();
    for (call_ts, tool, args) in tool_calls {
        let result = tool_results
            .iter()
            .enumerate()
            .find(|(i, r)| !used_results[*i] && r.1 == tool)
            .map(|(i, r)| {
                used_results[i] = true;
                (r.2, r.3.clone())
            });
        tools.push(ToolPair {
            tool,
            args,
            result,
            call_ts,
        });
    }

    let (tier, model, reason_raw, reason_human, latency_ms) = match &route {
        Some(r) => {
            let human = r.reason.as_deref().map(decode_reason);
            (
                r.tier.clone(),
                r.model.clone(),
                r.reason.clone(),
                human,
                r.latency_ms,
            )
        }
        None => (None, None, None, None, None),
    };

    let (context_available, recall_subjects, recall_hit_count, persona_tier, history_turn_count) =
        match context {
            Some(c) => (
                true,
                c.recall_hit_subjects.unwrap_or_default(),
                c.recall_hit_count,
                c.persona_tier,
                c.history_turn_count,
            ),
            None => (false, Vec::new(), None, None, None),
        };

    Ok(Some(TurnMind {
        turn_id: turn_id.to_string(),
        tier,
        model,
        reason_raw,
        reason_human,
        latency_ms,
        context_available,
        recall_subjects,
        recall_hit_count,
        persona_tier,
        history_turn_count,
        tools,
        reply_text,
        reply_destructive,
    }))
}

/// Find the most-recently-recorded `turn_id` in the store.
///
/// Scans all records in reverse and returns the first `turn_id` found.
///
/// # Errors
/// Returns an error if the store cannot be read.
pub fn most_recent_turn_id(store: &LucidStore) -> anyhow::Result<Option<String>> {
    let all = store.read_all()?;
    // Walk in reverse to find the newest tagged event.
    for rec in all.iter().rev() {
        if let Some(id) = &rec.turn_id {
            return Ok(Some(id.clone()));
        }
    }
    Ok(None)
}

// ── Renderer ───────────────────────────────────────────────────────────────────

/// Render a [`TurnMind`] as a human-readable, multi-line string.
#[must_use]
pub fn render(mind: &TurnMind) -> String {
    let mut out = String::new();

    out.push_str(&format!("╔══ turn: {} ══\n", mind.turn_id));

    // Route section
    out.push_str("║\n║  ROUTE\n");
    if let Some(tier) = &mind.tier {
        out.push_str(&format!("║    tier:       {tier}\n"));
    } else {
        out.push_str("║    tier:       (not recorded)\n");
    }
    if let Some(model) = &mind.model {
        out.push_str(&format!("║    model:      {model}\n"));
    }
    if let Some(lat) = mind.latency_ms {
        out.push_str(&format!("║    latency:    {lat} ms\n"));
    }
    if let Some(raw) = &mind.reason_raw {
        out.push_str(&format!("║    reason:     {raw}\n"));
        if let Some(human) = &mind.reason_human {
            out.push_str(&format!("║                → {human}\n"));
        }
    } else {
        out.push_str("║    reason:     (not recorded)\n");
    }

    // Context section
    out.push_str("║\n║  CONTEXT\n");
    if mind.context_available {
        if let Some(count) = mind.recall_hit_count {
            out.push_str(&format!("║    recall hits: {count}\n"));
        }
        if mind.recall_subjects.is_empty() {
            out.push_str("║    recall:      (no memories injected)\n");
        } else {
            out.push_str("║    recall:\n");
            for subj in &mind.recall_subjects {
                out.push_str(&format!("║      - {subj}\n"));
            }
        }
        if let Some(p) = &mind.persona_tier {
            out.push_str(&format!("║    persona:     {p}\n"));
        }
        if let Some(h) = mind.history_turn_count {
            out.push_str(&format!("║    history:     {h} turns in context\n"));
        }
    } else {
        out.push_str("║    (context digest unavailable — turn recorded before wm.brain.context adoption)\n");
    }

    // Tools section
    out.push_str("║\n║  TOOLS\n");
    if mind.tools.is_empty() {
        out.push_str("║    (no tool calls)\n");
    } else {
        for pair in &mind.tools {
            let args_str = serde_json::to_string(&pair.args).unwrap_or_else(|_| "?".to_string());
            match &pair.result {
                Some((ok, body)) => {
                    let status = if *ok { "ok" } else { "err" };
                    let body_str =
                        serde_json::to_string(body).unwrap_or_else(|_| "?".to_string());
                    out.push_str(&format!(
                        "║    {}({}) → [{status}] {body_str}\n",
                        pair.tool, args_str
                    ));
                }
                None => {
                    out.push_str(&format!(
                        "║    {}({}) → (pending/no result recorded)\n",
                        pair.tool, args_str
                    ));
                }
            }
        }
    }

    // Reply section
    out.push_str("║\n║  REPLY\n");
    match &mind.reply_text {
        Some(text) => {
            let kind = if mind.reply_destructive {
                "destructive"
            } else {
                "normal"
            };
            out.push_str(&format!("║    [{kind}] {text}\n"));
        }
        None => {
            out.push_str("║    (no reply recorded)\n");
        }
    }

    out.push_str("╚══\n");
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn decode_reason_known_variants() {
        assert!(decode_reason("command").contains("command/control"));
        assert!(decode_reason("conversational").contains("cloud for quality"));
        assert!(decode_reason("no_key").contains("API key"));
        assert!(decode_reason("offline").contains("offline"));
        assert!(decode_reason("cloud_timeout_fallback").contains("latency budget"));
        assert!(decode_reason("total_failure").contains("canned degrade"));
        assert!(decode_reason("prefer_local_only").contains("local-only"));
        assert!(decode_reason("prefer_cloud_only").contains("cloud-only"));
        assert!(decode_reason("override").contains("swap-model"));
    }

    #[test]
    fn decode_reason_unknown_passthrough() {
        let s = decode_reason("some_future_reason_code");
        assert!(s.contains("some_future_reason_code"));
    }
}
