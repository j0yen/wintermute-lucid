# Changelog

## v0.3.0 — 2026-06-05

Adds `lucid trace <turn_id>` and `lucid last [N]` — reconstruct a single turn as a stage-by-stage latency timeline with stall/death detection naming the missing next stage. The direct answer to "I'm talking and nothing is happening."

## v0.2.0 — 2026-06-05

Add `lucid mind <turn_id>` and `lucid why` subcommands that assemble the
brain's route decision (tier/model/reason/latency), tool calls+results,
and final reply for a recorded turn. Reason strings decoded into plain
English. Graceful degrade when wm.brain.context is absent (pre-adoption
turns). wm.brain.context publish-side deferred to wintermute-brain tick.
