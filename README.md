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
the in-agent interface to the job store — and a `/cron` command. The
daemon, the cron ticker, the schedule tool and `/cron` are always on.

## Chat channels are parts

The fifteen chat channels that used to be separate plugins now live here as
**parts**. Every part is off by default, so a gateway install carries only
the channels you ask for:

```sh
gray plugin install gateway --parts telegram,discord   # at install time
gray plugin enable gateway/telegram                    # later
gray plugin disable gateway/slack                      # turn one off again
```

Enabling a part is what puts the channel's tools, `/commands` and hooks in
the manifest. A disabled part contributes nothing — no tools, no commands,
no hooks — and its service refuses to start:

```
$ gray gateway telegram run
the telegram channel is off, so its service will not start —
`gray plugin enable gateway/telegram` turns it on
```

| part | what it is |
| --- | --- |
| `discord` | Discord: send, rich components, voice-note transcripts |
| `telegram` | Telegram: send, files, voice-note transcripts |
| `slack` | Slack over Socket Mode |
| `signal` | Signal through signal-cli |
| `matrix` | Matrix over /sync |
| `irc` | IRC with TLS |
| `email` | Email over IMAP and SMTP |
| `ntfy` | ntfy subscribe/publish |
| `webchat` | local browser chat on 127.0.0.1 |
| `whatsapp` | WhatsApp Business Cloud API |
| `teams` | Microsoft Teams webhook |
| `twilio` | Twilio SMS |
| `gchat` | Google Chat webhook |
| `mastodon` | Mastodon post, timeline, notifications |
| `mattermost` | Mattermost REST |

Tool names do not change when a channel becomes a part: `telegram_send`,
`telegram_file`, `discord_send`, `discord_send_ui`, `discord_open_modal`,
`discord_file`, `discord_ui_schema`, `slack_send`, `slack_file`,
`signal_send`, `matrix_send`, `irc_send`, `email_send`, `ntfy_send`,
`wa_send`, `wa_inbox`, `teams_send`, `twilio_send`, `twilio_inbox`,
`gchat_send`, `mastodon_post`, `mastodon_notifications`,
`mastodon_timeline`, `mastodon_search`, `mm_post`, `mm_posts`,
`mm_channels`, `mm_users`, `mm_react`, `mm_search`.

## CLI mapping

Every former plugin binary is now a subcommand, with its arguments, flags
and `--help` unchanged:

| before | now |
| --- | --- |
| `gray telegram setup` | `gray gateway telegram setup` |
| `gray discord run` | `gray gateway discord run` |
| `gray slack status` | `gray gateway slack status` |
| `gray signal doctor` | `gray gateway signal doctor` |
| `gray email install` | `gray gateway email install` |
| `gray ntfy sidecar` | `gray gateway ntfy sidecar` |
| `gray webchat` | `gray gateway webchat` |
| `gray mattermost post …` | `gray gateway mattermost post …` |

The eight big channels keep their own long-lived service under the unit
names they already use (`gray-discord-plugin.service`,
`gray-telegram-plugin.service`, `gray-slack-plugin.service`, and
`gray-signal.service`, `gray-matrix.service`, `gray-irc.service`,
`gray-email.service`, `gray-ntfy.service`), now execing
`gray-gateway <channel> run`. The gateway daemon does not absorb them.
`gray gateway <channel> install` writes the unit, and `setup`, `status`,
`stop`, `restart`, `doctor` and `uninstall` behave as they did.

An existing install needs no manual step: when the sidecar starts — once per
session — any runit `run` script or systemd user unit of a channel that
still execs the old standalone binary is repointed at
`gray-gateway <channel> run` with the rest of its arguments kept, and the
supervisor is told to re-read it (`sv restart`, or `systemctl --user
daemon-reload`). A service that is not running is only rewritten, never
started. The rewrite is idempotent.

Config and state paths are unchanged, so an existing setup keeps working:
`~/.config/gray-telegram/config.json`, `~/.config/gray-discord/`,
`~/.config/gray-slack/`, and so on, with each channel's sqlite, lock and
pid files next to its config; the smaller channels keep
`$GRAY_HOME/<channel>/config.json`.

## Wire

- `plugin/manifest`, `plugin/tools`, `tool/call`, `command/run`,
  `prompt/context`, `plugin/shutdown`; unknown methods get `-32601`,
  notifications are silent.
- The schedule tool creates jobs bound to the calling session (deliver:
  origin, platform `repl`), which land in `$GRAY_HOME/cron/inbox/<sid>` and
  are drained by the REPL.
- A `tool/call` for a channel tool is routed to the enabled channel that
  claims the name; a `command/run` for `/telegram` (or the `telegram`
  subcommand) is routed the same way. `prompt/context` concatenates the
  context blocks of every enabled channel that asks for the hook.

## Layout

```
src/main.rs          gateway CLI (daemon, cron, session, channel subcommands)
src/sidecar.rs       the one sidecar loop: gateway tools + enabled channels
src/parts.rs         parts.json loading (off unless enabled)
src/channels/mod.rs  the part table, merged manifest, tool/command routing
src/channels/common/ code shared by every channel: busy, stt, config helpers
src/channels/<part>/ one former plugin each, internals unchanged
```

## Vendored bits

Self-contained: cron store/schedule, gateway lifecycle/pid/socket/state/
service/brain/turn, plus small helpers for `GRAY_HOME`, `config.json` flags,
the session JSONL append format, skill discovery, `gateway.pid`, and
secret redaction (from `gray-core`).
