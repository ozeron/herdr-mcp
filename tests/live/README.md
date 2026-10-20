# Live acceptance suite (U11)

Drives the real `herdr-mcp` binary over stdio against a running scratch Herdr session, with real Codex agents. Every test is `#[ignore]` and also returns early unless `HERDR_MCP_LIVE=1`, so plain `cargo test` never touches Herdr.

## Run

The session `herdr-mcp-probe` must be running, and `codex` and `claude` must be on `PATH` and logged in.

```sh
HERDR_MCP_LIVE=1 HERDR_MCP_LIVE_CWD=<dir codex already trusts> \
  cargo test --test live -- --ignored --test-threads=1
```

`HERDR_MCP_LIVE_CWD` is the working directory of the test workspaces. Codex asks to trust a new directory, which blocks `herdr_launch` with `agent_not_ready`. It defaults to the repo root. Takes about 70 s.

## What it touches

- Only the session `herdr-mcp-probe`. Each test creates its own workspace and closes it on exit, including on failure. Other workspaces are never typed into or closed.
- One Codex agent per AE3, AE4 and AE5b test, in that test's workspace. Replies are one word or one line.
- `HERDR_*` is stripped from the bridge's environment so the caller is external. Two tests set `HERDR_PANE_ID` on purpose, and one runs the bridge under a pane's own shell.
- AE5a runs `install` with `HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` in temp dirs, and the real `claude` and `codex`. `PATH` holds a `herdr` stub that exits 1, so `--status` never runs `herdr integration status`. Real `~/.claude.json` and `~/.codex` are not written.
- The tests never run `herdr plugin` or `herdr integration`.

## Coverage

| Test | AE |
|---|---|
| `ae1_list_and_read_*` | AE1 tree and bounded tail |
| `ae1_missing_session_*` | AE1 `herdr_not_running`, bridge stays up |
| `ae2_ready_timeout_*` | AE2 `timed_out`, then `herdr_wait` with the returned `rewait` matches, even when the line printed before the wait began |
| `ae2_stale_output_*` | AE2 an earlier `listening on` does not satisfy a new ready match |
| `ae3_launch_turn_*` | AE3 launch with `wait: turn`, re-wait, reply, `herdr_send` follow-up, `completion_seq > turn.after`, no `success` field |
| `ae4_interrupt_*` | AE4 pane count unchanged, target `done`, other statuses unchanged |
| `ae4_interrupting_the_callers_own_pane_*` | AE4 `self_target`, for an unverified env-named caller and a verified caller |
| `ae5a_*` | AE5 install, `--status` shows `ours` for both, remove |
| `ae5b_*` | AE5 child reports a verified caller equal to its own pane through the injected bridge |

The verified-caller case works by typing the bridge command into a pane, with stdio over two FIFOs, so the pane's shell is the bridge's ancestor.

## Skipped

- **Blocked child (AE3).** `herdr_launch` takes no agent args, so it cannot force an approval prompt.
- **Restart and resume (AE5).** Herdr's integration hooks are not installed in the scratch session, so Herdr does not resume agents after a restart. See `docs/probes.md` item 8.
- **Read versus control listing in manual `claude` and `codex` panes (AE5).** Only the injected child is covered. The install itself is checked in scratch dirs.
