# Herdr MCP design (draft)

Status: proposal, 2026-10-20. Nothing here is implemented or probed live.

Evidence labels:
- **[src]**: verified in the Herdr checkout (143bf5fd, Cargo 0.9.5) or the T3 checkout (b5a0696a).
- **[0.9.3]**: verified against the installed binary.
- **[proposal]**: a design choice made here.
- **[unverified]**: plausible, but needs a probe before anyone relies on it.

Decisions taken:
- 2026-10-20: agents that the bridge did not launch get Herdr MCP through an **opt-in global install** (§6.3).

## Summary

Ship `herdr-mcp`, a stdio MCP server, inside a Herdr plugin. It is stateless, and each MCP client spawns its own copy. It exposes nine tools on the existing Herdr socket, modelled on T3 Code's orchestrator v2 thread tools.

- An agent inside Herdr gets the server from a user-approved global entry in its Claude or Codex config. The bridge process inherits the pane's `HERDR_SOCKET_PATH` and `HERDR_PANE_ID`, so it knows its session and its caller without a token.
- External clients use the same entry from outside Herdr.
- No daemon, no HTTP endpoint, no credentials, no task database.

## How it works

```
 Herdr session (one 0600 unix socket, newline-delimited JSON)
 ┌────────────────────────────────────────────────────────────────┐
 │ workspace w1                                                   │
 │  tab w1:t1   pane w1:p1  claude ──stdio──▶ herdr-mcp ─┐        │
 │  tab w1:t2   pane w1:p2  codex  ──stdio──▶ herdr-mcp ─┤ socket │
 │              pane w1:p3  npm run dev   (plain process)│        │
 │  tab w1:t3   pane w1:p4  tail -f app.log              │        │
 └───────────────────────────────────────────────────────┼────────┘
                                                         ▼
                                       herdr server (agent.*, pane.*, tab.*)
 outside Herdr: codex ──stdio──▶ herdr-mcp ── default socket ──┘
```

Each agent spawns its own `herdr-mcp` as a stdio MCP server. The bridge has no state and talks only to the Herdr socket. Panes stay what they are; the bridge just reads, types into, and waits on them.

**Once: install.**
1. You run the plugin action "Herdr MCP: install". It shows the diff, asks, then runs `claude mcp add --scope user herdr -- ~/.local/bin/herdr-mcp` and the Codex equivalent.
2. From then on, every Claude or Codex start lists a `herdr` MCP server. That includes manual starts, `herdr agent start`, and resume-on-restore.

**Every agent start: connect.**
1. Claude in pane `w1:p1` spawns `herdr-mcp`. The child inherits `HERDR_SOCKET_PATH=…/herdr.sock` and `HERDR_PANE_ID=w1:p1`.
2. The bridge connects to that socket and treats `w1:p1` as the caller. It answers MCP `initialize` with the tools and short instructions.
3. Outside Herdr there is no pane ID. The bridge uses the default socket with no caller, or returns `herdr_not_running`.

**Delegating to another agent.** Claude in `w1:p1` asks for a review.
1. It calls `herdr_launch {kind: codex, name: reviewer, where: {tab}, instructions, wait: turn, timeout_ms: 50000}`. The bridge then:
   - calls `tab.create` in `w1` and gets pane `w1:p5`;
   - calls `agent.start {kind: codex, pane_id: w1:p5}`, which types `codex` into the new shell and waits for it to be ready;
   - reads `agent.get` (idle, `state_change_seq = 41`), sends `agent.prompt {text: instructions}`, and records `turn = {pane: w1:p5, after: 41}`;
   - waits on `events.subscribe` until `completion_seq > 41` or the agent is `blocked`, for at most 50 s.
2. It returns `{agent: {pane: w1:p5, name: reviewer, status: working}, turn, timed_out: true}`.
3. Claude calls `herdr_wait {target: reviewer, turn}` again. It gets `{matched: true, status: done}`.
4. It calls `herdr_read {target: reviewer, lines: 200}` to read the reply. Whether the review is any good is Claude's judgement, not the bridge's.

**Running a server and waiting for it.**
1. It calls `herdr_run {command: "npm run dev", where: {split: down}, ready: {match: "listening on"}}`. The bridge then:
   - calls `pane.split` to create `w1:p6`, a fresh pane, so no old text can match;
   - calls `pane.send_input {text: "npm run dev"}` to type the command and press Enter;
   - calls `pane.wait_for_output {match: "listening on"}`.
2. It returns `{pane: w1:p6, ready_matched: true, excerpt}`, or `timed_out: true` with the last lines, and the caller decides what to do.

**Stopping work.**
- `herdr_interrupt {target: reviewer}` sends Codex's interrupt key and reports `status_after`.
- `herdr_interrupt {target: w1:p6, method: signal}` sends SIGINT to the pane's foreground process group. The shell and the pane survive.
- Only `herdr_close` removes a pane.

Unproven parts, each probed in §9:
- whether `completion_seq` behaves as above on real turns;
- the interrupt key for each provider;
- the exact config that `claude mcp add` and `codex mcp add` write.

## 1. Constraints from the sources

- **Herdr already has the controls.** [src][0.9.3]
  - Its 102 socket methods include `agent.list/get/read/start/prompt/wait/send_keys/rename`, `pane.split/send_input/send_text/send_keys/read/process_info/wait_for_output/close`, `tab.create`, `workspace.create`, `worktree.list/create/open/remove`, `events.subscribe` and `session.snapshot`.
  - Installed 0.9.3 has the same method set and protocol 22.
- **The socket is newline-delimited JSON on a 0600 unix socket, with no auth.** [src] `src/api/server.rs:29`
- **Pane environment.** [src] Every pane exports `HERDR_ENV=1`, `HERDR_SOCKET_PATH` and `HERDR_BIN_PATH`. Managed panes also export `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID` and `HERDR_PANE_ID` (`src/pane.rs:202-217`).
- **IDs.** [src] Workspaces are `w<N>`, tabs `w1:t<n>`, panes `w1:p<n>`. They are stable per workspace, and a moved pane keeps its old ID as an alias. Agents are also addressable by live name.
- **Agent status** is `idle | working | blocked | done | unknown`. It comes from screen matching plus hook reports. [src]
- **Turn signal.** [src] `AgentInfo.completion_seq` gives a turn signal (`src/app/actions.rs:22-29, 1754-1761`).
  - On each state change, a global counter increments and becomes the agent's `state_change_seq`.
  - `completion_seq` is set to that value only on a `working|blocked → idle` transition, and is cleared on any other change.
- **Waits.** [src]
  - `agent.wait` returns at once if the agent already matches, and has no default timeout.
  - `agent.prompt --wait` can match an earlier turn.
  - `pane.wait_for_output` matches text already on screen.
  - `pane.read` always reports `revision: 0`, so there is no new-output cursor.
- **Reads** are capped at 1000 lines; recent sources default to 80. [src]
- **No interrupt or signal API.** [src] Interruption means `send_keys`. `pane.process_info` exposes `foreground_process_group_id`.
- **`agent.start` types `<agent> <args…>` into an idle shell pane.** [src] Resume-on-restore builds its own command line and drops extra args (`src/agent_resume.rs:200-250`).
- **Plugins.** [src]
  - Startup hooks run once, and there is no supervisor or shutdown hook.
  - Plugin panes are not respawned on restore.
  - Event hooks such as `pane.agent_status_changed` run a plugin command per event, with `HERDR_PLUGIN_EVENT_JSON`.
  - No hook runs before an agent launches.
- **`herdr integration` edits global agent config for hooks**, with markers and version stamps. [src] Herdr writes no MCP entries today.
- **T3 Code.** [src]
  - One HTTP `/mcp` endpoint with a bearer token per thread.
  - It injects through channels it owns: Codex app-server `config`, and Claude stdin `setMcpServers`.
  - Wait timeouts never cancel work, and success comes from recorded run state.

## 2. T3 orchestrator v2 tool map

T3 registers about 80 MCP tools across eleven toolkits (`apps/server/src/mcp/toolkits/*/tools.ts`). The table groups them by purpose and gives the Herdr counterpart.
- **Adopt:** take the idea nearly as-is.
- **Adapt:** reshape it to fit panes.
- **Skip:** out of scope, or it needs T3's durable engine.

| T3 tools | What they do in T3 | Herdr verdict |
|---|---|---|
| `orchestrator_capabilities`, `t3_environment_read` | Providers and models, the caller's inherited settings, feature flags | **Adopt** as `herdr_capabilities`: agent kinds with injection support, caller pane/tab/workspace, session, feature flags. |
| `t3_thread_list` | List threads with filters (status, title, settled) and a cursor | **Adapt** as `herdr_list`: the workspace → tab → pane tree, including non-agent panes. |
| `t3_thread_read` | Durable timeline (`messages` / `activity` views), `afterPosition`, `itemId` + `textOffset`, `maxCharsPerItem` | **Adapt** as `herdr_read`: a bounded screen or scrollback tail. A position cursor needs a Herdr change (§7). |
| `t3_thread_launch`, `create_threads` | New top-level thread with `workspaceStrategy` (root, worktree, existing_worktree) and optional first message | **Adapt** as `herdr_launch`: an agent in a new tab, split or workspace, optionally in a new Herdr worktree, with optional first instructions. One per call; no batch. |
| — (T3 has no plain processes) | — | **New** `herdr_run`: servers, tests and log tails in panes. |
| `t3_thread_send` (`auto` / `queue` / `steer` / `restart`, returns `delivery`) | Follow-up input | **Adapt** as `herdr_send` with `auto` / `after_idle` / `steer` / `restart`. A durable `queue` is skipped because it needs a daemon. |
| `t3_thread_wait` (`runId`, `timeoutMs` → `status`, `timedOut`) | Wait for a terminal run state; a timeout never cancels | **Adopt** as `herdr_wait`. The turn token from launch or send plays the role of `runId`. |
| `t3_thread_interrupt` (→ `interrupt_requested` / `no_active_run` / terminal) | Interrupt the running turn | **Adopt** as `herdr_interrupt`, plus process-group signals for non-agent panes. |
| `delegate_task`, `task_status`, `task_cancel` | Durable child tasks; async completion wakes the parent | **Skip the engine.** A synchronous delegation is `herdr_launch {wait: "turn"}`. Async wake-up is the open decision (§10). |
| `t3_pending_request_list/read/respond` | Answer a child's questions; never approvals | **Fold in.** `herdr_wait` reports `blocked` with an excerpt, and `herdr_send {answer_blocked: true}` answers. The same "no approval by default" rule applies. |
| `t3_thread_update`, `t3_thread_organize` | Rename, link PR, pin, snooze, settle | **Later, optional.** `herdr_label` would map to `agent.rename` / `pane.rename` / `tab.rename`. Organizing has no Herdr analogue. |
| `t3_worktree_status/list/handoff` | Worktree binding for a thread | **Fold in.** `herdr_list` shows worktrees, and `herdr_launch where.worktree` creates one. No handoff. |
| `t3_queue_*` (6 tools) | Edit, reorder or promote queued messages | **Skip.** Providers keep their own input queues. |
| `t3_thread_fork`, `t3_thread_merge_back`, `t3_thread_transfers` | Context transfer between threads | **Skip.** This is provider conversation state Herdr cannot see. |
| `t3_thread_configuration/configure` | Switch provider or model | **Skip.** The model is chosen with launch `args`. |
| `t3_thread_search` | Search thread content | **Skip for now.** A `herdr_read` per pane is enough at Herdr's scale. |
| `t3_workflow_stop` | Stop a Claude workflow | **Skip.** |
| `t3_project_*` | Register, clone, update projects | **Skip.** A Herdr workspace is created by `herdr_launch` / `herdr_run`. |
| `schedule_task` (+4), `request_secret`, `preview_*` (20), `device_*`, `html_*`, pull-request tools, attachments | App features | **Out of scope.** |

Conventions worth copying from T3:
- **Readonly, destructive and idempotent hints** on every tool. These are MCP `annotations`.
- **Structured failures** returned as results, not exceptions, using codes such as `agent_not_running`, `busy` and `timeout`.
- **Omitted target means the caller**, but only for read-only tools. Destructive tools always need an explicit target.
- **Tool descriptions that teach semantics**, for example "timeout never cancels" and "idle is not success". Each instruction section stays under 4 KB, because Codex truncates longer ones.
- **Server-side wait caps below the client's tool timeout.**

## 3. Tool surface [proposal]

There are nine tools.
- **Targets:** a `target` is a pane ID, a tab ID, or an agent name.
- **Placement:** `where` is one of:
  - `{pane}`: an existing idle shell.
  - `{split: right|down, of?}`.
  - `{tab, workspace?}`.
  - `{workspace, cwd?}`.
  - `{worktree: {branch, base?, workspace?}}`: Herdr `worktree.create`, then a new tab there.

  The default is a new tab in the caller's workspace, or the focused workspace for external callers.

| Tool | Hints | Params → result | Herdr calls |
|---|---|---|---|
| `herdr_capabilities` | readonly | → `caller{pane,tab,workspace}?`, `session`, `herdr_version`, `agents[{kind, mcp: injected\|global\|unsupported, interrupt_keys}]`, `features{turn_tracking, output_cursor, interrupt_api, global_install}` | `ping`, `server.agent_manifests` |
| `herdr_list` | readonly | `scope` (`all`/`workspace`/`tab`), `kind` (`agent`/`process`/`shell`), `status[]`, `label_contains` → tree of workspaces → tabs → panes `{id, kind, agent?, name?, status?, foreground?, cwd, label, worktree?}` | `session.snapshot`, `agent.list`, `pane.process_info`, `worktree.list` |
| `herdr_read` | readonly | `target?` (default caller), `source` (`recent`/`visible`), `lines` (default 80, max 1000), `max_chars` (default 32k) → `text`, `truncated`, `status`, `foreground` | `agent.read` / `pane.read`, `agent.get` |
| `herdr_launch` | destructive | `kind`, `name`, `instructions?`, `where`, `cwd?`, `args[]`, `wait` (`none`/`ready`/`turn`), `timeout_ms` → `agent{pane, name, status}`, `turn?`, `timed_out`, `excerpt?` | `tab.create` / `pane.split` / `worktree.create`, `agent.start`, `agent.prompt`, `agent.wait` |
| `herdr_run` | destructive | `command`, `where`, `cwd?`, `env?`, `label?`, `ready?{match, regex?, timeout_ms}` → `pane`, `ready_matched`, `timed_out`, `excerpt` | `tab.create` / `pane.split`, `pane.send_input`, `pane.wait_for_output` |
| `herdr_send` | destructive | `target`, then `prompt` with `mode` (`auto`/`after_idle`/`steer`/`restart`), or `text`, or `keys[]`; `answer_blocked?` → `delivery` (`started`/`sent_after_idle`/`steered`/`restarted`/`typed`), `turn?` | `agent.prompt`, `agent.wait`, `pane.send_text`, `pane.send_keys` |
| `herdr_wait` | readonly | `target`, then one of `turn` (token), `until[]` (agent states), or `output{match, regex?}`; `timeout_ms` → `matched`, `timed_out`, `status`, `excerpt` | `agent.get`, `events.subscribe`, `agent.wait`, `pane.wait_for_output` |
| `herdr_interrupt` | destructive | `target`, `method` (`auto`/`keys`/`signal`), `signal?` (`INT`/`TERM`) → `result` (`interrupt_requested`/`no_active_work`), `status_after` | `agent.send_keys` / `pane.send_keys`, `pane.process_info`, local `kill(-pgid)` |
| `herdr_close` | destructive | `target` (required), `allow_self?` → `closed` | `pane.close` / `tab.close` |

Semantics:
- **Turn tokens.** [proposal, logic verified in src, behaviour unverified]
  - When `herdr_send` or `herdr_launch` delivers a prompt to an idle agent, it records `turn = {pane, after: state_change_seq}`.
  - `herdr_wait {turn}` matches when `completion_seq > after`, or the agent is `blocked`. This is T3's `runId` pinning, built from Herdr's existing counter.
  - The turn is unambiguous only if the agent was idle at send time.
- **Send modes.** These are the honest version of T3's `auto` / `queue` / `steer` / `restart`.
  - `auto` sends if the agent is idle, and returns `busy` if it is working.
  - `after_idle` waits, bounded, for idle and then sends. This is T3's `queue`, done synchronously.
  - `steer` types into a working agent. The result has `turn: null`, because the next completion may belong to the earlier turn.
  - `restart` interrupts, waits for idle, then sends.
  - A `blocked` agent is refused unless `answer_blocked: true`. Herdr already returns `agent_blocked` for prompts.
- **Success is never inferred.** No tool has a "succeeded" field: results carry `status`, `timed_out` and an `excerpt`. The caller reads the output and decides. Idle only means the agent stopped producing output.
- **Waits never cancel.**
  - Default 30 s; server cap 300 s.
  - The bridge lowers the cap based on the MCP `clientInfo` from initialize. For example, Codex's tool timeout is assumed to be 60 s [unverified], so the bridge caps at 50 s. `timed_out: true` means call again.
- **Fresh output.** `herdr_run` prefers a new pane, so old output cannot match. In a reused pane, the bridge ignores matches already present in the pre-send tail; this is best effort.
- **Bounded output.** Both lines and characters are capped, and results report `truncated`. ANSI is stripped by default. Terminal text goes only to the requesting client and never into bridge logs.
- **Idempotency.** `herdr_launch` refuses a live `name`. Send has no dedupe because there is no storage, so callers check `herdr_read` before retrying after a timeout.
- **Interrupt is not close.** `auto` uses the provider's key from the bridge's table: `esc` for Claude and Codex [unverified], and `ctrl+c` for processes. `signal` hits only the foreground process group, never the shell. Only `herdr_close` destroys, and it refuses the caller's own pane or tab without `allow_self`.

## 4. Hosting: stdio bridge per client [proposal]

| Option | Verdict |
|---|---|
| **Stdio bridge spawned by each MCP client** | **Chosen.** It lives as long as its client, gets caller context from env, and has no secrets to protect. |
| Shared HTTP daemon (T3-style) | Deferred. Herdr cannot supervise or restore it, and per-caller tokens would have to go through argv, env or files. |
| Server tab or pane per workspace | Rejected as the default. It does not survive restore, and it is available only as an optional log pane. |

- **Session.** The socket resolves as `--session`, then `HERDR_SOCKET_PATH`, then `HERDR_SESSION`, then the default socket. Outside a running Herdr, tools return `herdr_not_running` instead of failing at startup.
- **Caller context.** `HERDR_*_ID` env vars are self-asserted. They exist for defaults and guardrails, not security, because the 0600 socket already trusts every process the user runs.
- **State.** None is required. An optional bounded log goes in `<state_dir>/plugins/herdr-mcp/`.
- **Language.** Rust: one static binary, fast startup per agent, and no Node runtime at spawn. Low stakes.

## 5. Plugin packaging [proposal]

```toml
# herdr-plugin.toml (sketch)
id = "herdr-mcp"
name = "Herdr MCP"
version = "0.1.0"
min_herdr_version = "0.9.3"

[[build]]
command = ["cargo", "build", "--release"]

[[actions]]
id = "status"
title = "Herdr MCP: install status"
command = ["bin/herdr-mcp", "install", "--status"]

[[actions]]
id = "install"
title = "Herdr MCP: install into Claude/Codex (shows diff, asks first)"
command = ["bin/herdr-mcp", "install", "--interactive"]

[[actions]]
id = "uninstall"
title = "Herdr MCP: remove from Claude/Codex"
command = ["bin/herdr-mcp", "install", "--remove", "--interactive"]

[[panes]]
id = "log"
title = "Herdr MCP log"
placement = "split"
command = ["bin/herdr-mcp", "tail-log"]
```

- **No startup hook and no daemon.** The bridge exits when its client closes stdin, and reconnects to the socket lazily.
- **Interactive actions.** Install and uninstall actions [unverified] need a TTY to ask for confirmation. If plugin actions have none, the action opens a plugin pane that runs the interactive installer instead.

## 6. Connection and injection

### 6.1 Global install: the default path for agents inside Herdr (decided)

`herdr-mcp install` follows the pattern of `herdr integration`.
- **Explicit only.** It runs only when the user triggers it: no automatic install, no startup hook.
- **Shows before writing.** It prints the exact change and asks for confirmation; `--yes` skips the prompt for scripted use.
- **Stable shim.** It creates `~/.local/bin/herdr-mcp`, pointing at the current plugin build. Configs reference the shim, so versioned plugin directories never leave stale paths.
- **Claude.** It runs `claude mcp add --scope user herdr -- ~/.local/bin/herdr-mcp`, so the provider's CLI owns its file format instead of hand-editing `~/.claude.json`. It honours `CLAUDE_CONFIG_DIR`.
- **Codex.** It runs `codex mcp add herdr -- ~/.local/bin/herdr-mcp`, then sets `tool_timeout_sec` in `$CODEX_HOME/config.toml` with a marked line edit if the key is supported [unverified].
- **Markers.** It records a version marker so `install --status` can report drift and `--remove` removes exactly what it added.
- **Behaviour outside Herdr.**
  - There are no `HERDR_*_ID` vars, so there is no caller.
  - The bridge talks to the default session if one is running, and returns `herdr_not_running` otherwise.
  - The same entry serves acceptance workflow 1 (external Codex) with no extra setup.
- **Coverage.** Every start path: manual starts, `herdr agent start`, resume-on-restore, and agents launched by `herdr_launch`.
- **Cost.** Every Claude and Codex session, inside Herdr or not, lists the `herdr` server. The tool descriptions say plainly when no Herdr is reachable.

### 6.2 Per-launch injection: the fallback when the global install is absent

- `herdr_launch` checks `install --status`.
- If no global entry is present, it appends a per-process stdio entry:
  - Codex: `-c 'mcp_servers.herdr.command="<abs>/herdr-mcp"'`.
  - Claude: `--mcp-config '{"mcpServers":{"herdr":{"command":"<abs>/herdr-mcp"}}}'`.
- The entry holds no secret, so argv is fine.
- It uses the same server key `herdr`, so it never duplicates a global entry. Whether Claude lets the per-process entry win over a user-scope entry with the same key is [unverified]; skipping per-launch injection when the global entry exists avoids the question.
- Agents started this way lose the server on resume-on-restore. This is the reason the global install is the default.

### 6.3 Instructions

- **Primary:** the MCP initialize `instructions` field plus the tool descriptions. These travel with the server to every client. Whether Codex surfaces `instructions` is [unverified].
- **Content (under 4 KB per section):**
  - Herdr tools manage panes. Provider-native subagents remain fine for same-provider work.
  - Use `herdr_launch` for a separate visible agent.
  - Never treat idle as success.
  - Keep waits bounded, and call again on `timed_out`.
  - Prefer new panes for servers.
  - Never approve a blocked agent's permission prompt unless the user asked.

### 6.4 Other providers

Herdr knows 24 agent kinds (opencode, gemini, pi, …). `herdr_launch` starts them without MCP, and `herdr_capabilities` reports `mcp: unsupported` until each one's config path is verified and added to the installer.

## 7. Plugin today vs Herdr changes

**Achievable entirely in the plugin:**
- All nine tools and the stdio bridge.
- The global install and per-launch fallback.
- Turn tokens from `completion_seq`.
- Readiness waits in fresh panes.
- Interrupt by keys or by process-group signal.

**Narrow Herdr changes that would remove workarounds** (each is optional):

| Gap | Plugin workaround | Proposed Herdr change |
|---|---|---|
| No output cursor (`revision` is always 0) | Prefer fresh panes; diff against the pre-send tail | A real `revision` in `pane.read`, plus `after_revision` on `wait_for_output` and read (T3's `afterPosition`) |
| Turn ambiguity when sending to a working agent | `auto` refuses; `steer` returns `turn: null` | Return the baseline from `agent.prompt` and accept `after_seq` in `agent.wait` |
| No interrupt API | A provider key table in the bridge | `agent.interrupt` from detection manifests; `pane.signal` |
| No pre-launch hook | The global install covers it | Optional `extra_args` / `env` per agent kind, applied on start and resume |
| `herdr integration` has no MCP target | `herdr-mcp install` | An `--mcp` option, if upstream wants to own it |

## 8. Acceptance workflows

1. **External Codex reads progress.** `herdr_list {kind: agent}` → `herdr_read {target, lines: 120}`.
2. **An internal agent starts a server and waits for it.** `herdr_run {command, where: {split: down}, ready: {match: "listening on"}}`. If `timed_out`, call `herdr_read`.
3. **An agent launches another agent and reads the reply.** `herdr_launch {kind: claude, name: reviewer, where: {tab}, instructions, wait: turn}`, repeat `herdr_wait {turn}` while `timed_out`, then `herdr_read`.
4. **Interrupt one piece of work.** `herdr_interrupt {target: reviewer}` returns `interrupt_requested` and `status_after`. Other panes are untouched and nothing is closed.
5. **Automatic MCP config.** After the one-time opt-in install, every Claude or Codex start inside Herdr has the `herdr` tools, including manual starts and resume.

## 9. Probes before implementation

Each probe runs in a scratch session (`--session herdr-mcp-probe`) with scratch config dirs (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), never against the real ones.

- `claude mcp add --scope user` and `codex mcp add` with a stdio command: what do they write, and does `--remove` reverse it cleanly?
- Codex `tool_timeout_sec`: does the key exist and what is its default? Does Codex surface MCP `instructions`?
- Per-launch `-c mcp_servers.herdr.command` and `--mcp-config` entries: do they load, and what happens when the key collides with a global entry?
- `completion_seq`: does it advance exactly once per settled Claude or Codex turn, including tool-heavy turns that pass through `blocked`?
- Interrupt keys per provider: which key cancels a turn without exiting the agent?
- Do plugin actions get a TTY?
- Do pane IDs and agent names survive server restart?

## 10. Unresolved decision

**Should a delegating agent be woken when its child finishes (T3's async `delegate_task`), or must it poll with bounded `herdr_wait`?** This choice decides whether Herdr MCP stays stateless, and how agents are taught to delegate: end the turn and wait to be woken, or keep the turn alive in a loop.
- **Polling only (current design):**
  - It is stateless and simple.
  - The parent spends turns and tokens on wait loops, and cannot do anything else while it waits.
- **Wake-up through a plugin event hook (plugin-only):**
  - `herdr_launch {notify: true}` writes a small subscription file to the plugin state dir.
  - The manifest's `[[events]] on = "pane.agent_status_changed"` hook runs on every status change. When a watched child completes, it `agent.prompt`s the parent with a short notice.
  - Herdr runs the hook, so no daemon is needed.
  - It adds the first piece of persistent state, and it types into the parent's input, which can collide with a human typing there.
- **No wake-up; the user is notified instead.** This uses Herdr's existing notifications and leaves agents unaware.

Recommendation: ship polling first, and prototype the event-hook wake-up behind `notify: true` once the turn-token probe passes.
