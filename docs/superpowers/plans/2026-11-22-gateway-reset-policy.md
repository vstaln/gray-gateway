# Gray Gateway Reset Policy Implementation Plan

> For agentic workers: use the executing-plans or subagent-driven-development workflow. Build on maid only and verify with a scratch gateway and Discord bot.

**Goal:** Let the operator inspect and change the incoming-chat reset policy through `/gateway reset-policy` without restarting the gateway.

**Architecture:** `gray-gateway` owns the command, policy validation, durable settings, and reset decision. Each transport owns caller authentication and its transcript storage. Gray core's token-triggered compaction remains separate.

**Tech stack:** Rust, the gateway sidecar and CLI, and the existing JSON settings.

## Gateway command and policy

- [ ] Add `reset-policy` to the existing command dispatcher in `src/sidecar.rs` and gateway CLI in `src/main.rs` and `src/gateway/mod.rs`. Keep bare `/gateway`, status, and on/off semantics. Print the effective mode and applicable daily time and idle duration; never imply the time is UTC unless the daemon is configured that way.
- [ ] Add one validated reset policy in `src/gateway/settings.rs` alongside the gateway settings. Use the existing `none`, `idle`, `daily`, and `both` values, a daily hour of 0–23 in the daemon's local time, and an idle duration of 1–10,080 minutes. Prefer omission over an invented override for installs that have not configured the gateway policy. Use a serialized, atomic update that preserves unrelated settings and refuses to overwrite an unreadable config. Local operator commands can update the settings; a Discord write must arrive through the authenticated bridge, not generic unchecked command forwarding.
- [ ] Move the daily and idle decision to `gray-gateway`. Pass the decision function the execution time, last activity and whether an active session exists. Do not use `Sessions.updated_at` as last activity without changing its tracking: today it changes only when the route or session ID changes.

## Discord adapter

- [ ] Delegate `/gateway reset-policy` to Gray Gateway using the stable gateway home, not the isolated home created for a single Discord conversation. Keep Discord owner checks at every write entry point, including components and `/run`; a generic slash-command forward must not serve as a second, unchecked write path.
- [ ] Ask Gray Gateway for the reset decision when a Discord turn starts, after taking the conversation lock. If there is no central setting, pass the existing Discord policy as the compatibility default; do not rewrite or copy it on every turn. An explicit gateway setting supersedes it. A failed or unreadable decision must not archive the transcript. Discord retains its existing archival path, queue handling, conversation keys, and manual reset behavior.
- [ ] A running turn keeps its original decision. A queued turn reads the effective policy when it begins, and saving does not cancel work. The separate cron path is unchanged in this pass.

## Verification

- [ ] Build the changed packages on maid. Check the actual gateway CLI and run a scratch Discord bot. Verify the owner can save a policy without restarting, non-owners cannot save it through slash or `/run`, and gateway status and on/off still route as before.
- [ ] Change to Manual only while work is queued and verify the next incoming turn preserves the session. Then use a due idle policy, verify that the next turn starts fresh and archives the previous transcript, restart the scratch gateway, and confirm the saved policy and legacy fallback behavior. Do not deploy to grayme as part of this check.

Prompt fingerprinting, provider cache telemetry, and prompt pinning remain separate follow-ups.
