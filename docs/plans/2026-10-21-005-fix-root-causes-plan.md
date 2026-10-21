---
title: Fix The Root Causes Behind Today's Tool Bugs - Plan
type: fix
date: 2026-10-21
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# Fix The Root Causes Behind Today's Tool Bugs - Plan

## Goal Capsule

- **Objective:** remove the five causes behind the bugs two reviewers and the audit log found in `24a7f0b..cf56e93`, instead of patching each symptom.
- **Authority:** this plan, then `AGENTS.md` and the user's global instructions.
- **Stop conditions:**
  - stop before any change that types into an agent whose state is not known;
  - stop before any change that lets a remote caller act as a local pane;
  - stop before any change that logs mail bodies or pane output in the audit log.
- **Tail ownership:** local commits per unit. A full review runs before the push, not after. The user approves the push.

## Root causes and what they explain

| Cause | Symptoms |
|---|---|
| RC1. The caller is guessed from env, process ancestry and focus, separately in each tool | Inherited `HERDR_PANE_ID`; a tunnel still finding a pane by ancestry; a verified pane with no workspace refused; the KenWoo tab in the focused workspace; mail `no_caller` for ChatGPT |
| RC2. Delivery is judged after typing, from one screen read | Claude sends reported `steered` while it queues; a busy Codex blocks 30 s, then `not_sent`; no Codex queue |
| RC3. A tool can act and still return a plain error | `wait: turn` drops the turn after a delivered prompt |
| RC4. Shutdown is one cancel, not a state | New calls accepted while draining; stop delayed; last reply cut |
| RC5. The contract is hand-written in four places, unchecked | `screen` advertised but not in the schema; "exact label" vs case-insensitive; stale `timeout_ms` and design docs; ChatGPT never sees the server instructions |
| Process | The audit log keeps hand-picked parameters, so it could not show `mode`; fakes encode assumptions; review ran after push |

## Implementation Units

### U1. One caller model (RC1)

- **Goal:** the bridge decides the caller once, from how it was started, and every rule reads that decision.
- **Design:**
  - `CallerModel` set at construction: `Remote { client }` for `--tunnel` and HTTP; `Local` for stdio, resolved per call as today; never ancestry or env for `Remote`.
  - `Inner::resolve_caller` returns an external resolution for `Remote` without probing panes.
  - A verified local caller's workspace comes from Herdr's snapshot when the env lacks it.
  - Placement: explicit `workspace_id`, then the verified caller's workspace. `Remote` with neither gets `destination_required`. A local caller with no verified pane keeps the focused workspace, with a `note`, as before this morning.
  - Mail for `Remote`: an inbox named `remote` (the name inbox mechanism). `herdr_mail_read` and `herdr_mail_wait` read it; workers reply with `herdr_mail_send {to: "remote"}`. The remote instructions and descriptions say so.
- **Files:** `src/server.rs`, `src/caller.rs`, `src/serve.rs`, `src/place.rs`, `src/tools/mail.rs`, `src/instructions_remote.md`; tests in `tests/tools_run_close.rs`, `tests/tunnel_bridge.rs`, `tests/mailbox.rs`.
- **Tests:**
  - a tunnel bridge whose ancestry contains a pane still resolves as external;
  - a verified caller with no workspace env gets its workspace from the snapshot;
  - a local caller with no verified pane gets the focused workspace and a note; a remote one gets `destination_required`;
  - remote mail: a worker mails `remote`, the remote caller reads it.

### U2. Decide delivery before typing (RC2)

- **Goal:** `herdr_send` knows what will happen before it types, and says so when it cannot.
- **Design:**
  - The provider table gets a queue capability per kind: `None`, or `Native { marker }` (Claude: typing while working queues), or `Key { key, marker }` (Codex, if the live probe confirms a queue key).
  - Auto on a working agent:
    - a blocked agent, or a known dialog on screen, is refused before typing;
    - `Native`: type, then check the screen up to 1.5 s for the marker; `queued` when seen, else `delivery: unconfirmed` with a note;
    - `Key`: type the text without Enter, press the queue key, confirm the marker the same way;
    - `None`: return `busy` at once, with nothing typed. `after_idle` stays the explicit way to wait.
  - Steer mode keeps its meaning and gets the same multi-read confirmation.
- **Live probe first:** in a throwaway workspace, start a Codex turn, type a line, try the queue key, read the screen. Record the result in `docs/probes.md`.
- **Files:** `src/providers.rs`, `src/tools/send.rs`, `docs/probes.md`; tests in `tests/tools_launch_send.rs`.
- **Tests:** Claude queued after a late redraw; Claude never shows the marker gives `unconfirmed` and a note; a working agent with no queue gets `busy` immediately with nothing typed; a dialog on screen is refused before typing; Codex queue key (if probed).

### U3. Act, then always succeed with a status (RC3)

- **Goal:** after the first side effect a tool returns success with a status; an error means nothing happened.
- **Design:** `herdr_send` `wait: turn` maps a wait error after delivery to `wait_error: {code, message}` with the turn kept, and keeps any earlier note. A `wait: turn` with no turn returns `note: "no turn to wait on"`.
- **Files:** `src/tools/send.rs`; tests in `tests/tools_launch_send.rs`.

### U4. Shutdown as a state (RC4)

- **Goal:** running, draining and stopped are explicit, and every call checks them.
- **Design:** a `draining` flag shared by every session. The signal handler and `herdr_tunnel_stop` set it; `call_tool` refuses new calls with `shutting_down`; the drain waits for running calls, then 300 ms so the last reply flushes, then cancels.
- **Files:** `src/server.rs`, `src/http.rs`; tests in `tests/http.rs`, `tests/tunnel_bridge.rs`.

### U5. One contract, checked (RC5)

- **Goal:** the guidance ChatGPT needs is in the tool descriptions, and the docs match the schema.
- **Design:**
  - tool descriptions carry the key rules: read leaves `lines` unset; send uses `wait: turn`; launch and run give `workspace_id` when remote; list `tail` for an overview; move fixes placement;
  - `screen` becomes a real `source` value;
  - labels: the docs say case-insensitive;
  - fix the `timeout_ms` doc, `docs/design.md`, and `tail: 0`;
  - `herdr_move` is marked destructive (it can close an emptied workspace);
  - a test checks that every backticked value a description offers for an enum is in that enum.
- **Files:** `src/server.rs`, `src/tools/*.rs`, `README.md`, `docs/design.md`; test in `tests/mcp_server.rs`.

### U6. Audit every parameter by default (process)

- **Goal:** the audit log can answer how a tool was called.
- **Design:** `Note` records each call's parameters from serde, minus a redact list: a mail body keeps only its length. `tunnel_expired` lines get the caller. The send result summary adds the agent `kind`.
- **Files:** `src/audit.rs`, `src/server.rs`; tests in `tests/tunnel_bridge.rs`.

### U7. cwd check on real paths

- **Goal:** the cwd warning compares real paths and never fails a launch.
- **Design:** expand `~`, canonicalize both sides when they exist, keep the lexical check otherwise; a snapshot failure skips the warning instead of failing.
- **Files:** `src/place.rs`; tests in `tests/tools_run_close.rs`.

### U8. Live check before release (process)

- **Goal:** what fakes cannot show is checked against real Herdr before a push.
- **Design:** `probes/live_check.py` makes two throwaway workspaces, runs the bridge as an external caller with an empty ancestry, and checks placement, `destination_required`, list `tail`, read `screen` and `herdr_move`; it closes what it made.
- **Files:** `probes/live_check.py`.

## Verification Contract

| Gate | Command |
|---|---|
| Format | `cargo fmt --check` |
| Lint | `cargo clippy -j 4 --all-targets -- -D warnings` |
| Tests | targeted per unit with `-j 4 -- --test-threads 4`; the full suite once at the end, if `memory_pressure` shows at least 25% free |
| Live | `probes/live_check.py` |
| Review | an independent review of the whole diff before the push |

## Deferred

- A read cursor ("since last read") on `herdr_read`: needs a design for how to anchor scrollback.
- Asking Herdr upstream for a queued-input state, so delivery needs no screen reads.
