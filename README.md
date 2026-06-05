# wintermute-lucid

`wm-lucid` — the **flight recorder** for the agorabus bus.

Every thought wintermute has already flows over the agorabus bus — ~120 `wm.*`
topics across the voice stack and action layer — but nothing records it.
`wm-lucid` subscribes to the entire `wm.` prefix and persists every event to a
rotating, turn-keyed structured log that survives daemon and reboot death. It
is the foundational crate of the `lucid` subsystem; the rest of the fleet
(`lucid-explain`, `lucid-live`, `lucid-mind`, `lucid-trace`, `lucid-turn-id`)
reads from the log format this crate establishes.

## What it does

- **Records** every `wm.*` event as a structured row:
  `{v, ts_received, topic, turn_id?, from, raw_payload}` — see
  [`src/record.rs`](src/record.rs), the schema contract.
- **Persists** to an append-only, size-rotating NDJSON log under
  `$XDG_CACHE_HOME/wintermute/lucid/` (default `~/.cache/wintermute/lucid/`),
  bounded so the recorder never fills the disk.
- **Indexes** by `turn_id` (the lucid-turn-id correlation key); events without
  one are bucketed under a synthetic `untagged-<ts>` key and never dropped.
- **Survives restarts**: on start it resumes the latest segment and rebuilds
  the turn index from an append-only sidecar; prior segments are never
  truncated.
- **`wm-lucid tap`** tails live events to stdout as one JSON object per line —
  the first-class replacement for ad-hoc `agorabus subscribe` one-shots. It
  honors `--topic <prefix>` and exits cleanly when its stdout pipe closes
  (SIGPIPE-safe).

## Usage

```sh
# Run the recorder daemon (what the systemd unit runs):
wm-lucid record

# Tail live events, filtered to a topic prefix:
wm-lucid tap --topic wm.brain.

# Pipe-safe:
wm-lucid tap | head
```

Install the systemd-user unit from [`units/wm-lucid.service`](units/wm-lucid.service):

```sh
install -m644 units/wm-lucid.service ~/.config/systemd/user/
systemctl --user enable --now wm-lucid.service
```

## Acceptance criteria

1. Starts, registers as an agorabus peer with a `wm-lucid` intent tag, and
   subscribes to the full `wm.` prefix.
2. A burst of N published `wm.*` events yields N persisted records, each
   carrying `{ts_received, topic, turn_id?, from, raw_payload}`.
3. Records are keyed/indexed by `turn_id` when present; an event lacking one is
   still recorded under a synthetic key and never dropped.
4. Log rotation bounds total on-disk size; overflowing the cap rotates/prunes
   oldest segments, and prior segments are never truncated.
5. After a restart, records written before the restart are still present and
   readable.
6. `wm-lucid tap` streams live events as one JSON object per line, honors
   `--topic`, and exits cleanly when its stdout pipe closes.
7. A `wm-lucid.service` systemd-user unit installs, enables, and comes up
   `active` with the correct ordering deps.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.
