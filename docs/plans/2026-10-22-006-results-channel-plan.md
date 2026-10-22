---
title: Give Remote Callers A Results Channel - Plan
type: fix
date: 2026-10-22
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# Give Remote Callers A Results Channel - Plan

## Goal Capsule

- **Objective:** a remote caller gets an agent's answer and its files without digging them out of scrollback.
- **Authority:** this plan, then `AGENTS.md` and the user's global instructions.
- **Stop conditions:** stop before any change that lets a read-mode bridge read files, or that logs file contents or mail bodies.
- **Tail ownership:** local commits per unit; review and live check before the push; the user approves the push.

## Root cause

The audit log of 2026-10-21 (121 ChatGPT calls, old binary) shows one cause behind four symptoms: the only channel from the machine to a remote caller is terminal text.

| Symptom | Evidence |
|---|---|
| Reads of 160 to 1000 lines to find a final answer | 3 reads of 1000 lines, 36k to 39k characters each |
| `truncated: true` on 15 of 17 `recent` reads | It means "older history exists", so the caller asks for more |
| `recent` reads of 3 to 9 s | Herdr pages a full-screen agent by mouse-scrolling it while idle; we ask for 4× the rows |
| A 109k-character base64 export through `herdr_read` | No file channel; the transfer was blocked |

Herdr's own docs give the fix for long answers: have the agent write a file, then read the file.

## Implementation Units

### U1. Honest, cheap reads

- Ask Herdr for the rows wanted plus a margin, not 4×; one top-up read when blank lines leave too few.
- `truncated` only when this call cut text; `more_above` when older output exists.
- `herdr_list` `tail` reads the visible screen, so a listing never scrolls an agent.
- **Files:** `src/tools/read.rs`; tests in `tests/tools_read.rs` or the existing read tests.

### U2. Reply by mail from `herdr_send`, and for remote callers

- `herdr_send` gets `reply: mail` for a prompt: the prompt carries the reply instruction; `wait: turn` returns the mail as `reply`; otherwise `reply: "pending"`.
- A remote caller's reply goes to its inbox (`remote-chatgpt`), on `herdr_launch` too.
- A wait failure after delivery stays a status, never an error.
- **Files:** `src/delegation.rs`, `src/tools/send.rs`, `src/tools/launch.rs`, `src/tools/mail.rs`; tests in `tests/tools_launch_send.rs`, `tests/tunnel_bridge.rs`.

### U3. `herdr_file_read`

- Control mode only. `{target, path, offset, max_bytes}`; text as text, other bytes as base64; returns `size`, `next_offset`, `eof`.
- Reads only under the directory the target pane was opened in, on real paths; a pane in `/` or the home directory reads nothing. The open file's real path is checked again, and files with other hard links are refused.
- A short list of credential names (`.ssh`, `.env` files and others) is refused in any case, as a speed bump: an agent with a shell can read them.
- The audit log keeps the path and byte count, never the content.
- **Files:** `src/tools/file_read.rs`, `src/server.rs`, `src/audit.rs`, README, docs; tests in a new `tests/tools_file_read.rs`.

## Verification Contract

| Gate | Command |
|---|---|
| Format | `cargo fmt --check` |
| Lint | `cargo clippy -j 4 --all-targets -- -D warnings` |
| Tests | targeted per unit with `-j 4 -- --test-threads 4`; the full suite once at the end if `memory_pressure` shows at least 25% free |
| Live | `probes/live_check.py`, extended with a file read |
| Review | an independent review before the push |
