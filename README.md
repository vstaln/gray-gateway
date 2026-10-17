# gray-gateway

The always-on gateway daemon for gray, extracted from core as a plugin.

Installs as `gateway` and provides both CLI surfaces core used to own:

```sh
gray gateway status      # daemon, socket, cron ticker, service
gray gateway run         # foreground daemon (what the service unit execs)
gray gateway install     # runit / systemd user service
gray cron list           # job table
gray cron add "in 10m" "check CI"
```

It owns `$GRAY_HOME/gateway.sock` (the control socket channel plugins talk
to), the `$GRAY_HOME/cron` job store, the outbox under `$GRAY_HOME/gateway`,
and the per-minute cron ticker. Turns run as `gray -p … --json` subprocesses.

The sidecar (NDJSON on stdio, spawned bare) exposes the `schedule` tool —
the in-agent interface to the job store — and a `/cron` command.

## Wire

- `plugin/manifest`, `plugin/tools`, `tool/call`, `command/run`,
  `plugin/shutdown`; unknown methods get `-32601`, notifications are silent.
- The schedule tool creates jobs bound to the calling session (deliver:
  origin, platform `repl`), which land in `$GRAY_HOME/cron/inbox/<sid>` and
  are drained by the REPL.

## Vendored bits

Self-contained: cron store/schedule, gateway lifecycle/pid/socket/state/
service/brain/turn, plus small helpers for `GRAY_HOME`, `config.json` flags,
the session JSONL append format, skill discovery, `gateway.pid`, and
secret redaction (from `gray-core`).
