# wintermute-lucid

`wm-lucid` — the flight recorder for the agorabus bus, and the tools that read the recording back.

Every thought wintermute has already flows over agorabus — the voice stack and the action layer publish across the `wm.*` topic namespace — but nothing kept it. When a turn went wrong, the evidence was already gone. `wm-lucid` subscribes to the whole `wm.` prefix and persists every event to a rotating, turn-keyed log that survives daemon and reboot death. The recorder is half of it; the other half is the four read tools that turn that log into an answer to "what just happened" — a latency timeline, the brain's reasoning, a live monitor, and a plain-language narration.

One crate, one binary (`wm-lucid`), with subcommands. `unsafe` is forbidden and `unwrap`/`expect`/`panic` are denied in the crate lints.

## Install

```sh
cargo install --path .
```

The binary is `wm-lucid`. It links `agorabus` as a path dependency, so it expects a sibling `agorabus` checkout (see Cargo.toml). The TUI for `watch` is behind the default `tui` feature; build `--no-default-features` to drop ratatui/crossterm if you only need the recorder and the text tools.

## Run the recorder

```sh
wm-lucid record        # subscribe to wm.* and persist every event (the default subcommand)
```

Records land as append-only, size-rotating NDJSON under `$XDG_CACHE_HOME/wintermute/lucid/` (default `~/.cache/wintermute/lucid/`). `--max-segment-bytes` and `--max-segments` bound the on-disk size; the oldest segments rotate out and prior segments are never truncated. On restart the recorder resumes the latest segment and keeps reading — records written before the restart stay present.

As a systemd-user service:

```sh
install -m644 units/wm-lucid.service ~/.config/systemd/user/
systemctl --user enable --now wm-lucid.service
```

## Read it back

The recording is keyed by `turn_id` — the correlation id that ties one wake-to-reply cycle together. The read tools take a turn id, or resolve "the most recent turn" for you.

```sh
# Live: watch the pipeline stages light up in real time (TUI; --plain for NDJSON)
wm-lucid watch

# Tail raw events to stdout as one JSON object per line (pipe-safe)
wm-lucid tap --topic wm.brain.

# Timeline: reconstruct one turn stage-by-stage with per-stage latency
wm-lucid trace <turn_id>          # --json, --full for raw payloads
wm-lucid last                     # the most recent turn; `last N` for N one-line summaries

# Reasoning: the brain's route decision, injected context, tool calls, final reply
wm-lucid mind <turn_id>           # --json
wm-lucid why                      # same, for the most recent turn

# Narration: a turn explained in plain language
wm-lucid explain <turn_id>        # or --last
wm-lucid explain --last --voice   # also speak it: publishes to wm.tts.say
```

`trace` and `last` exit non-zero when a turn stalled or failed, so they compose in scripts. A stalled turn is named by the stage that never arrived ("stalled after dialog — no brain"); a failed turn by where it broke. That's the direct answer to "I'm talking and nothing is happening."

`explain` is deterministic — it narrates from the recorded `trace` and `mind` fields with no LLM call, which matters because the brain is often the thing that failed. `--persona hearth` gives a warm first-person register; `flat` (the default) is diagnostic.

## How it works

A record is `{v, ts_received, topic, turn_id?, from, raw_payload}` — see [`src/record.rs`](src/record.rs), the schema contract. Events carrying a `turn_id` are indexed by it; events without one are bucketed under a synthetic `untagged-<ts>` key and never dropped. Everything downstream is a query over that log:

- [`store.rs`](src/store.rs) — the rotating NDJSON store and turn index.
- [`trace.rs`](src/trace.rs) — turn → stage timeline, with stall/failure detection.
- [`mind.rs`](src/mind.rs) — turn → the brain's reasoning, decoding route-reason strings into English.
- [`watch.rs`](src/watch.rs) — the live TUI and `--plain` monitor.
- [`explain.rs`](src/explain.rs) — timeline + mind → deterministic narration, optionally spoken.

`mind` degrades gracefully when `wm.brain.context` is absent (turns recorded before that topic existed).

## Where it fits

Part of the wintermute fleet. It sits on top of `agorabus` (the bus) and observes the whole voice-and-action stack without being in its path — a recorder, not a participant, except when `explain --voice` publishes back to `wm.tts.say`.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
