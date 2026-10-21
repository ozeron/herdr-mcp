---
title: ChatGPT Remote Admin - Plan
type: feat
date: 2026-10-21
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# ChatGPT Remote Admin - Plan

## Goal Capsule

- **Objective:** drive this Mac's Herdr sessions from ChatGPT (web or phone) through OpenAI's Secure MCP Tunnel, in a read mode and a full-access mode, running in the background, with a complete audit trail.
- **Authority:** this plan, then `AGENTS.md` and the user's global instructions.
- **Stop conditions:** stop and ask before any change that lets the tunnel run full access without the typed disclosure, or that writes pane output into the audit log.
- **Tail ownership:** local commits on `main`. Push and real-ChatGPT testing stay with the user.

---

## Product Contract

### Summary

Add active-session switching for every client, and three things for the tunnel: a background service with start, stop and status; a full-access mode behind a typed disclosure; and an audit log of every tool call. Read mode stays the default.

### Requirements

**Sessions**
- R1. `herdr_sessions` lists Herdr sessions and marks the active one; `herdr_use_session` switches it; every tool acts on the active session.
- R2. The caller pane only counts in the home session; elsewhere the caller is external.

**Tunnel lifecycle**
- R3. `herdr-mcp tunnel start [--full] [--for <duration>]` runs the tunnel in the background as a macOS LaunchAgent that survives Herdr restarts and restarts on crash.
- R4. `herdr-mcp tunnel stop` and `herdr-mcp tunnel status` manage it. Status shows running, mode, start time, expiry and the audit log path.
- R5. A `herdr_tunnel_stop` tool, present only in tunnel mode, lets ChatGPT stop the tunnel.
- R6. There is no time limit by default. `--for` sets one, and the bridge stops the tunnel when it passes.

**Full access**
- R7. `--full` gives ChatGPT the control tools. `start --full` prints the disclosure and requires the user to type `full access`; without it nothing starts.
- R8. Read mode stays the default and never exposes control tools.

**Audit**
- R9. In tunnel mode, every tool call is appended to a 0600 audit log: time, tool, session, target, outcome and duration. Write tools also record the command, text or keys. Pane output and read results are never logged.
- R10. `herdr-mcp tunnel log` prints the audit log.

**Install and docs**
- R11. The plugin manifest offers start (read), start (full access), stop and status. The README walks through install, the one-time OpenAI Platform setup, the ChatGPT setup, daily use and the kill switches.

### Scope Boundaries

- No OAuth server of our own. ChatGPT-side connector auth is checked in the user's smoke test.
- No per-action approval prompts (user decision).
- Linux background service is out of scope; `start` explains that it is macOS only.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **Harden reach, not each action.** (session-settled: user-directed — chosen over per-action local approval: approval at the Mac defeats remote use.) The protections are: the key in the Keychain, the Tunnels Read+Use key scope, a private connector, the typed disclosure, the remote and local kill switches, and the audit log.
- KTD2. **A LaunchAgent, not a pane.** (session-settled: user-directed — chosen over an on-demand pane: a pane is hard to manage.) The label is `dev.herdr-mcp.tunnel`; the plist lives in `~/Library/LaunchAgents/`. `ProgramArguments` runs `herdr-mcp tunnel run` with the mode flags, `KeepAlive` is set on crash only (`SuccessfulExit = false`), and stdout and stderr go to the state dir.
- KTD3. **Tunnel-only behaviour is opt-in via an explicit `--tunnel` bridge flag** in the `--mcp-command`. It enables `herdr_tunnel_stop`, the audit log and expiry. Local Claude and Codex never get them.
- KTD4. **How stop works.** `tunnel run` writes its pid before it execs, so that pid becomes `tunnel-client`'s pid.
  - `tunnel stop` runs `launchctl bootout` for the agent and removes the plist.
  - `herdr_tunnel_stop` and expiry do the same from inside the bridge. To find the agent they confirm, via the pidfile and the process name, that the bridge's parent is that `tunnel-client`.
  - Booting out the agent also stops the bridge's own `tunnel-client`.
- KTD5. **No default expiry.** (session-settled: user-directed — chosen over an 8 h default.)
- KTD6. **Mode is fixed by `start`.** Read mode passes `--tools read`, and full passes `--tools control`. ChatGPT cannot change mode.
- KTD7. **The audit log is separate from `herdr-mcp.log`.** The existing log stays output-free and command-free; the audit log deliberately holds commands and keys, and lives in `$XDG_STATE_HOME/herdr-mcp/tunnel-audit.log`, or `~/.local/state/herdr-mcp/` when that is unset.
- KTD8. **The disclosure is typed once per `start --full`.** The agent itself runs without a TTY. `start` records a confirmation marker in the agent's arguments (`--full-confirmed`); `tunnel run --full` refuses to start without it, so a hand-written plist cannot skip the prompt silently.

### Verified Facts (tunnel-client 0.0.16)

- `tunnel-client run` accepts `--mcp-command "<cmd>"`, and `--control-plane.tunnel-id` takes `tunnel_` plus 32 lowercase alphanumerics.
- The key comes from `CONTROL_PLANE_API_KEY`.
- The health server defaults to `127.0.0.1:8080`, so we pass `127.0.0.1:0`.
- herdr-mcp answers `tools/list` without `notifications/initialized`.

---

## Implementation Units

### U1. Active session

- **Requirements:** R1, R2.
- **Files:** `src/sessions.rs`, `src/tools/sessions.rs`, `src/server.rs`, `src/scope.rs`, `src/caller.rs`, `src/tools/capabilities.rs`, `src/tools/list.rs`, `src/instructions.md`, `tests/sessions.rs`.
- **Verification:** `tests/sessions.rs` passes, and the existing suites pass unchanged except for the added `session` fields.

### U2. Bridge tunnel mode: audit, stop, expiry

- **Requirements:** R5, R6, R9.
- **Files:** `src/cli.rs` (`--tunnel`, `--expires-at`), `src/audit.rs`, `src/server.rs` (an audit hook in `finish`, plus the `herdr_tunnel_stop` route only with `--tunnel`), `src/tunnel.rs` (the stop helper), `tests/tunnel_bridge.rs`.
- **Test scenarios:**
  - Each tool call appends one JSON line with tool, session, target and outcome.
  - `herdr_run`, `herdr_send` and `herdr_launch` lines include the command, text or keys and the instructions.
  - No line contains pane output or read text.
  - The file is mode 0600.
  - Without `--tunnel`, there is no audit file and no `herdr_tunnel_stop`.
  - `herdr_tunnel_stop` calls the stop helper, which is fakeable in tests.
  - After `--expires-at`, calls return `tunnel_expired` and the stop helper is called.

### U3. Background service and full mode

- **Requirements:** R3, R4, R7, R8, R10, R11.
- **Files:** `src/tunnel.rs` (start, stop, status and log; the plist writer; the launchctl calls behind a fakeable runner), `src/cli.rs`, `herdr-plugin.toml`, `tests/tunnel.rs`, `tests/manifest.rs`, `README.md`.
- **Test scenarios:**
  - `start` writes a plist with the right arguments, mode, key source and log paths, then calls `launchctl bootstrap`.
  - `start --full` without the typed phrase writes nothing.
  - `stop` boots the agent out and removes the plist.
  - `status` reports running, mode, start time and expiry.
  - `run --full` without `--full-confirmed` refuses.
  - The read-mode command line contains `--tools read --tunnel` and never `control`.
- **Execution note:** fake `launchctl`, `security` and `tunnel-client` on PATH, and keep `HOME` in a temp dir. Never touch the real LaunchAgents dir or Keychain in tests.

### U4. Install and verify on this Mac

- **Requirements:** R11.
- **Approach:**
  - Run the full offline suite and clippy.
  - Install the new build: update the plugin and check the shim.
  - Smoke-test the parts that need no OpenAI key: `tunnel start` and `stop` with a dummy ID, then check that `launchctl` lists the agent and removes it; check `status`; check `log`.
  - The real ChatGPT test needs the user's tunnel ID and runtime key.

---

## Verification Contract

| Gate | Command |
|---|---|
| Format | `cargo fmt --check` |
| Lint | `cargo clippy -j 4 --all-targets -- -D warnings` |
| Tests | `cargo test -j 4 -- --test-threads 4` (check `memory_pressure` first) |

## Definition of Done

- U1 to U3 are committed with the offline suite green.
- The new build is installed on this Mac, and a local start, stop and status smoke test passes.
- The README tells the user exactly what to do in the OpenAI Platform and in ChatGPT.
