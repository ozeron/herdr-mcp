# U1 probe results

Run on 2026-10-20 against installed Herdr 0.9.3 (protocol 22) in a scratch session `herdr-mcp-probe`, using an isolated workspace `w2`. Agents: Claude Code 2.1.304 and Codex 0.163.

Herdr detection rules were remote `2026.10.11.1` for both Claude and Codex, reported in `server.agent_manifests` as `active_version`. Herdr's Claude and Codex integrations (hooks) were `not_installed`, so every agent state below comes from screen detection alone.

The helper is `probes/herdr_probe.py`. It speaks NDJSON to the scratch socket only. Installer probes used throwaway `CLAUDE_CONFIG_DIR` and `CODEX_HOME`. No terminal contents or credentials are recorded here.

Labels:
- **[probed]**: observed live.
- **[src]**: read from Herdr source.

## Answers

1. **Turns settle once.** [probed]
   - Every turn produced one `working` → `done` transition, with `completion_seq` equal to the `done` seq:
     - a simple Claude turn;
     - a tool-using Claude turn;
     - a simple Codex turn;
     - a tool-using Codex turn;
     - an Esc-interrupted turn of each.
   - Under rules `2026.10.11.1`, Codex settles to `done`, not `unknown`. KTD6 keeps the `unknown` rule for older bundled rules.
   - A Codex approval prompt went `working` → `blocked` with no `completion_seq`. Esc then gave `blocked` → `working` → `done`, with `completion_seq` on the `done`. This matches KTD6's re-basing rule.
   - Status events (`pane.agent_status_changed`) carry only `agent`, `agent_status`, `pane_id` and `workspace_id`, with no seq fields. Events are wake-ups, as KTD6 says.

2. **Interrupt key.** [probed] One `esc` cancels a working turn in both Claude and Codex, and the agent stays running. The interrupt registers as a completion (`done`), so a result cannot tell an interrupted turn from a finished one.

3. **Installer output.** [probed]
   - **Claude:**
     - `claude mcp add --scope user` writes `mcpServers.herdr = {type: "stdio", command, args, env: {}}` into `$CLAUDE_CONFIG_DIR/.claude.json`, and keeps a timestamped backup under `backups/`.
     - `claude mcp remove herdr -s user` removes it.
     - `claude mcp get` spawns the server to check its status. The classifier should read the JSON directly.
   - **Codex:**
     - `codex mcp add` writes a plain `[mcp_servers.herdr]` table with `command` and `args` into `$CODEX_HOME/config.toml`.
     - Hand-added `env_vars` and `tool_timeout_sec` lines are honoured, as `codex mcp get herdr --json` shows.
     - Re-running `codex mcp add` drops both keys and their comments. This is the drift case.
     - `codex mcp remove herdr` deletes the table.

4. **Claude passes Herdr env to MCP children.** [probed] A stdio server started by Claude saw every `HERDR_*` var:
   - `HERDR_ENV`, `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`;
   - `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`;
   - `HERDR_SESSION`, set in named sessions.

5. **Per-launch injection works.** [probed]
   - Claude `--mcp-config '<inline json>'` spawned the server.
   - Codex `-c mcp_servers.<name>.command=…` / `.args=…` spawned it.
   - Without `env_vars`, the Codex child saw no `HERDR_*` vars. With `env_vars`, it saw exactly the listed ones.
   - Not probed: a per-launch entry colliding with a user-scope entry of the same name. The bridge avoids the collision by skipping per-launch injection when the global entry is `ours` (KTD5).

6. **Provenance tokens.** [probed] `pane.report_metadata {source: "herdr-mcp", tokens: {herdr_mcp_parent: <pane>}}` shows up in `agent.get`/`agent.list` as `tokens`. It survived a full turn, and did not change `agent`, `agent_status` or detection. KTD12 uses it.

7. **Plugin actions get no TTY.** [src] Plugin commands are spawned with piped stdout and stderr and no terminal on stdin (`src/app/api/plugins/runtime.rs` around 120-130). Install and remove must therefore open a plugin pane to prompt.

8. **Restart identity.** [probed] I stopped the scratch server and the user relaunched it.
   - Workspace, tab and pane IDs survived: `w2`, `w2:p1`…`w2:p9`.
   - Every `terminal_id` and `shell_pid` changed. KTD6's `turn_stale` signals both fire.
   - No agent was resumed and every agent name was gone, even though `resume_agents_on_restore` defaults to true (`src/config/model.rs:278`). Resume needs an agent session ref, which Herdr records through its integration hooks (`pane.report_agent_session`), and these were not installed.
   - So the resume-on-restore half of R13 holds only when Herdr's Claude and Codex integrations are installed. Manual starts are covered either way.

9. **Readiness.** [probed]
   - Claude: ready at `idle` with `interactive_ready: true`, about 3.8 s after start.
   - Codex: ready at `idle` with `interactive_ready: true`, about 3.7–4.4 s after start. The `unknown` + `interactive_ready` case never occurred.
   - **New:** in 2 of 4 Codex starts, Codex went `idle` → `working` → `done` by itself within about 3 s of first ready, while loading its own MCP servers, with no turn run. A prompt sent in that window would be gated on the startup `working`, and the startup `done` would falsely complete it. Readiness therefore needs a quiet window: no state change for 3 s after first ready.
   - **New:** an agent command that exits immediately (bad flags) leaves Herdr at `launch_pending` / `unknown` with no error, until the start timeout releases the name. Readiness must check `pane.process_info` and fail fast when the shell is back in front.

10. **Prompt activity gate.** [probed] `agent.prompt` with `wait {until: [working, blocked], timeout_ms: 6000}` returned the `working` snapshot, with its `state_change_seq`, as soon as the turn started. KTD6's activity phase works as designed.

11. **Codex MCP servers run outside its sandbox.** [probed] A Codex started with `-s read-only` launched an MCP server that wrote a file outside the workspace. KTD17's read-only default is justified.

12. **Fresh panes report busy.** [probed] `agent.start` on a just-created pane returned `agent_pane_busy` 1–3 times before succeeding about 100–300 ms later. KTD7's pre-start retry is needed.
13. **Codex 0.163 queue key is unconfirmed.** [probed, 2026-10-21] The `?` shortcut overlay lists compose keys but no queue key in a default-size pane, and the binary carries a "Queued message" label without a visible key hint. Codex's own prompt treats a message sent mid-turn as steering. A live mid-turn test was skipped to save the user's weekly Codex quota. Until it is probed, herdr-mcp treats Codex as having no queue: auto on a working Codex returns `busy` at once.
14. **`launchctl kickstart -k` sends SIGTERM.** [probed, 2026-10-21] A throwaway job that traps TERM logged it, and launchd started the job again even after it exited 0. A restart therefore runs the drain.
15. **Process ancestry finds a pane even without env.** [probed, 2026-10-21] A bridge started with the `HERDR_*` pane variables removed, from a shell inside a pane, still resolved that pane as a verified caller.

## Plan impact

- KTD6: confirmed. Keep the `unknown` rule for older detection rules.
- KTD7: add the 3 s quiet window after first ready, and fail fast when the shell is in front during startup.
- KTD9 and KTD14: confirmed. Esc on `blocked` denies; Esc on `working` interrupts.
- KTD3: confirmed. Each call uses its own connection, and `events.subscribe` streams.
- R13 and AE5: resume-on-restore needs Herdr's integration hooks installed. U9's `--status` and the README should say so, and U11's restart check must install them in the scratch setup or drop that case.
- No probe invalidates a load-bearing decision.

## Live acceptance (U11)

Ran `tests/live` once against `herdr-mcp-probe`: 9 passed, 0 failed, about 70 s. Real Codex agents; replies were single words. Each test's workspace was closed afterwards, and `w1` and `w2` were left alone.

- **AE1:** passed. List shows the workspace and pane, and read returns a bounded tail. A bridge on `herdr-mcp-nope` returns `herdr_not_running` twice from `herdr_capabilities`.
- **AE2:** passed. A ready match with a 3 s timeout returns `timed_out`, and a re-wait on `listening on` matches. An earlier `listening on` in the same pane does not satisfy a new ready match.
- **AE3:** passed. `herdr_launch` with `wait: turn` returned a turn, the reply was in the pane, and a `herdr_send` follow-up matched with `completion_seq > turn.after`. No result had a `success` field. The blocked-child case is skipped: launch takes no agent args.
- **AE4:** passed. After an interrupt, the pane count was unchanged, the target was `done`, and no other pane changed status. The self-interrupt is refused with `self_target` for an unverified env-named caller, and for a verified one, with the bridge run under the pane's own shell over FIFOs.
- **AE5a:** passed. Install into temp `HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` showed `ours` for both, and remove left `absent`.
- **AE5b:** passed. A Codex child launched with an injected bridge reported a verified caller equal to its own pane.
- **Not run:** restart and resume, because Herdr's integration hooks are not installed.
- **Finding, fixed:** `herdr_wait {output}` only sees text added after the wait began, so a line printed between two waits was missed (an 8 s stand-in failed this way in two full runs). `herdr_run` now returns `rewait` on a ready timeout, and `output.after_command` counts lines after the last echo of the command. AE2 is back to `sleep 8`.
- **Finding:** Codex in an untrusted directory blocks on a trust prompt, and `herdr_launch` returns `agent_not_ready` and keeps the pane.

