# Changelog

## v0.4.0 — 2026-06-05

`lucid watch` live TUI: six-stage pipeline row (wake→capture→stt→dialog→brain→tts) with
per-stage color states, dialog FSM state line, streaming partial transcript, 8s stall detection,
completed-turn scrollback, and `--plain` NDJSON mode. ratatui/crossterm behind optional `tui`
feature; clean terminal teardown on q/Ctrl-C/SIGTERM.

## v0.3.0 — 2026-06-05

Adds `lucid trace <turn_id>` and `lucid last [N]` — reconstruct a single turn as a stage-by-stage latency timeline with stall/death detection naming the missing next stage. The direct answer to "I'm talking and nothing is happening."

## v0.2.0 — 2026-06-05

Add `lucid mind <turn_id>` and `lucid why` subcommands that assemble the
brain's route decision (tier/model/reason/latency), tool calls+results,
and final reply for a recorded turn. Reason strings decoded into plain
English. Graceful degrade when wm.brain.context is absent (pre-adoption
turns). wm.brain.context publish-side deferred to wintermute-brain tick.
