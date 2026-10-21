# WHY.md — gateway extraction hardening (in flight)

## Why this tree looks like this

Commits `55a32fa` + `cf335e1` extracted the gateway daemon + cron subsystem
from `gray` core into this standalone plugin repo. The uncommitted working
tree is three rounds of reviewer-driven hardening on top of that port. All
of it is uncommitted on `main`; nothing pushed.

The product goal behind the extraction: gray core keeps only the protocol,
loader, and installer; the always-on daemon (chat apps, cron, wakeups)
lives here so bare installs stay clean.

## Why each edit was made

### Auto-update rework (`gateway/run.rs`)

The first port spawned `gray update --everything` on every ~30s probe.
`update --everything` prompts `[y/N]` on stdin; with `Stdio::null()` it
EOF-cancelled every pass — the feature was dead AND churned a network
fetch + subprocess ~2,880 times/day. Replaced with the host's original
shape: `auto-update.json` stamp gate (6h), `logs/update.lock` try_lock,
`gray update` + `gray plugin update all` (both verified non-interactive
host commands), `GRAY_BARE=1`/`0.0.0`/debug guards.

Deliberate scope reduction vs `--everything`: provider-CLI updates
(npm/brew/`claude update`) are NOT covered — no confirmed non-interactive
path exists for them in the host, and an unattended daemon shouldn't try.
Reviewer accepted this as a flag-for-commit note.

Update children spawn via `gray_bin_abs()` (not bare `"gray"`) so a
minimal service PATH fails legibly instead of ENOENT.

### File locking (`vendor.rs`)

The original daemon took locks the port dropped. Restored, matching core's
conventions (`lock_session_file`, `lock_saved_config_at`):

- `config.lock` around `set_flag` read-modify-write; refuses to overwrite
  malformed/non-object config instead of clobbering it to `{}`.
- `sessions/{sid}.lock` around `append_injected_user`: strict per-line
  parse, torn-tail repair (truncate to last complete record + rescan —
  same semantics as core's `scan_entries`), `entry_id`/`parent_id`
  computed under lock, fsync.

Both callers run inside async contexts, so the call sites wrap them in
`tokio::task::spawn_blocking` — `lock_file_at` busy-polls `try_lock` for
up to 30s and must never stall the executor. (An async lock variant was
considered; spawn_blocking keeps `lock_file_at` usable from sync code too
and matches the existing drain-thread pattern in `run.rs`.)

### Sidecar reader (`sidecar.rs`)

Two stacked bugs drove the current shape:

1. Unbounded `BufReader::lines()` → added a 1 MiB line cap.
2. First fix decoded each `read()` chunk with `from_utf8_lossy` →
   corrupted multibyte UTF-8 at chunk boundaries (~8 KiB). Cron prompts
   carry CJK/emoji — this was the real data-loss path.

Final form: accumulate raw bytes, split on `b'\n'`, decode each complete
line once; oversize lines enter a skip-until-newline mode and emit a
`{"id": null, "error": {"code": -32600, ...}}` frame (JSON-RPC invalid
request — consistent with the file's `-32601` convention; a bare
`"invalid"` string code was rejected in review). EOF flushes a final
unterminated frame so `lines()`-era behavior is preserved.

### `/cron` dashboard parity + fixes

`format_cron_dashboard` is a verbatim port of the host's
`repl/cron.rs` renderer — `{:?}` on `Schedule`, raw epoch `next`,
ticker-health line. A reviewer suggested friendlier output
(`schedule_text`, relative times); declined deliberately — the sidecar's
`/cron` must render identically to host `gray cron`, and changing it here
first would break parity. If the host improves its renderer, re-port.

Lookup accepts what the display shows: `j.id.starts_with(query)` for the
8-char prefix, exact `j.name` match, `argv.join(" ")` so space-containing
names resolve. Display uses `chars().take(8)` — a hand-edited non-ASCII
id in `jobs.json` (user-writable, no validation boundary) must not panic
the sidecar.

### Misc hardening

- `find_in_path` checks the unix exec bit; `gray_bin_abs` treats a
  set-but-missing `GRAY_BIN` as an error (a silently-falling-back env var
  would bake a wrong path into `install`'s service unit).
- `cron_fire` Windows branch resolves `bash.exe`/`bash` via PATH via
  `shell_path()` instead of a dead hardcoded `"bash"`.
- `setup_platform` surfaces child stderr (unverified-plugin warning) even
  on success.
- `run_update_cmd` uses `kill_on_drop(true)` — a timed-out update no
  longer orphans mid-install while holding the lock.
- Appended session entries omit `"compaction_boundary": false` — core
  `skip_serializing_if`s it; byte-shape parity with core output.

Left as faithful port (not regressions): `std::process::exit` in
`setup_platform` (host does the same after re-exec); `turn.rs` steering
gap vs `GATEWAY-STEER-CONTRACT.md` (pre-existing upstream gap — daemon
owns the platform socket, worth a separate follow-up).

## Verified

`cargo fmt --check`, `cargo clippy --all-targets -D warnings`,
`cargo build` — all clean.

Live tests against scratch `GRAY_HOME` + stub binaries: UTF-8 across an
8 KiB read boundary, EOF-without-newline, 1 MiB cap frame, `/cron` prefix
and spaced-name lookup, non-ASCII-id render without panic, `on`/`off`
through spawn_blocking, strict `GRAY_BIN` error, `install --print`
absolute path, torn-tail repair + `entry_id`/`parent_id` chain on a
real `cron run` delivery (stub gray emitting a `result` row).

## Still open

- Provider-CLI auto-update coverage (deliberate scope cut — needs a
  non-interactive host path first).
- `turn.rs` steering support vs the steer contract.
- Uncommitted: awaiting re-review after round-3 fixes.

## Review round — auto-update deadlock + hardening follow-ups

- `auto_update_pass` no longer holds `logs/update.lock` across the child spawn.
  flock is per open-file-description: the spawned `gray update` runs its own
  blocking `acquire_update_lock` on the same file, so a held guard deadlocked
  the child for the full 600s every pass. The lock is now a try_lock *probe*
  (released immediately); the child's own lock provides serialization, and a
  contended manual `gray update` just queues it. Verified live: stub gray
  flocking the same file completed instantly, stamp recorded "Updated to 1.2.4".
- `try_acquire_update_lock` TryLockError::Error now skips the pass (host shape),
  not proceed-unlocked.
- `env!("CARGO_PKG_VERSION") == "0.0.0"` removed — it read the *plugin's*
  version. The intended managed-gray check is ported as `gray --version`
  probe; "0.0.0" output skips the pass (verified: no stamp written).
- `run_update_cmd` returns full stdout on success; `output_tail` only appears
  in error strings. Plugin updated/rebuilt/failed counts iterate full stdout,
  not a 300-char tail.
- `set_flag` writes config.tmp with mode 0600 (+ explicit set_permissions for
  stale tmp files) — config.json no longer widens from core's private 0600.
  `lock_file_at` tightens lock files to 0600 too.
- `append_injected_user` skips the first *non-empty* line (scan_entries parity;
  leading blank lines no longer count as the header).
- The finished `auto_task` JoinHandle is now inspected via `now_or_never()` so
  a panicked pass logs instead of vanishing.

## Review round — version token match, stamp-write locking, sidecar cap

- Dev-build probe now compares the version token exactly
  (`split_whitespace().nth(1) == Some("0.0.0")`), matching host
  `env!("CARGO_PKG_VERSION") == "0.0.0"`. `contains` false-positived on
  "10.0.0". Verified: "gray 10.0.0" runs the pass, "gray 0.0.0" skips it.
- The pass's update.lock is still probe-only (a held lock deadlocks the
  `gray update` child — it takes the same file via a blocking f.lock()).
  But the `auto-update.json` write now acquires update.lock briefly via
  spawn_blocking + vendor::lock_file_at(30s), serializing against a host
  REPL `maybe_run` that writes the same stamp under that lock, and writes
  via unique tmp + rename so a torn stamp can't read as spurious-due.
  Overlap between passes themselves remains best-effort — children's own
  locks serialize the real work.
- Sidecar 1 MiB cap now enforced in the Some(newline) branch too — a line
  whose terminator arrives inside the same chunk can no longer slip past
  the cap. Verified: 1MiB+5B in one write -> -32600, next line dispatched.
- set_flag tmp is now `.gray-{uuid}.tmp` (host save_private_json shape),
  unique per write — no stale `config.tmp` after a crash.
- Reverted a stray blank-line diff in cron/store.rs.
