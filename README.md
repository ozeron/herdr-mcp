# Herdr MCP

An MCP server for the Herdr terminal multiplexer. Agents and external MCP clients can list panes, read output, wait, launch agents, run processes, send input and interrupt work. A pane can hold an agent, a server, tests or logs, and all of them are first-class.

It is a stdio bridge: each MCP client spawns its own copy, and Herdr holds all the state. It needs Herdr 0.9.3 or newer. Herdr's protocol number is reported but never checked.

Design: [docs/design.md](docs/design.md). Brief: [docs/brief.md](docs/brief.md). Probe results: [docs/probes.md](docs/probes.md).

## Install

Install the plugin, then run one of its panes. Nothing is written to your Claude or Codex config until you confirm.

The manifest builds with `cargo install --path . --root . --locked`, which puts the binary at `bin/herdr-mcp` in the plugin directory. `herdr plugin install` runs that build. `herdr plugin link` does not, so run it yourself first.

```sh
herdr plugin install ozeron/herdr-mcp    # or, from a checkout: herdr plugin link /path/to/herdr-mcp
herdr plugin pane open --plugin herdr-mcp --entrypoint install           # read tools
herdr plugin pane open --plugin herdr-mcp --entrypoint install-control   # read and control tools
herdr plugin action invoke herdr-mcp.status                              # what is installed
herdr plugin pane open --plugin herdr-mcp --entrypoint remove            # undo
```

The panes run `herdr-mcp install [--control] [--yes]` in a real terminal, because plugin actions have no TTY to prompt on. You can also run the binary directly: `herdr-mcp install [--control] [--status|--remove] [--yes]`.

The installer:
- links `~/.local/bin/herdr-mcp` to the plugin's binary;
- runs `claude mcp add --scope user` and `codex mcp add`, then adds `env_vars` and `tool_timeout_sec = 330` to the Codex entry;
- honours `CLAUDE_CONFIG_DIR` and `CODEX_HOME`;
- refuses to overwrite a `herdr` entry or a shim it did not create;
- prints the plan and asks first. With no terminal and no `--yes`, it prints the plan and exits non-zero.

`--status` reads the config files and writes nothing. Each entry is `absent`, `ours`, `drifted`, `foreign` or `shim_broken`. `drifted` means the entry is ours but lost `env_vars`, the timeout or the right tools mode, or the shim moved. Agents treat a drifted entry as global. `--remove` removes only entries that still point at the shim.

### What read and control expose

Both modes share these exposures:
- `herdr_read` sends any pane's output, human shells included, to the model provider of the agent that calls it.
- Every Claude and Codex session, inside Herdr or not, lists a `herdr` server. Outside Herdr it connects to the default Herdr session whenever one is running.

Then they differ:
- **Read** (default) registers discovery, read and wait tools. It cannot type into or change anything.
- **Control** adds tools that run commands, type into panes and close them. Any connected agent then has shell authority over every pane in the session, with your permissions. Client sandboxes and approval modes do not contain it: Codex runs MCP servers outside its sandbox, and Herdr's shells are unsandboxed.

Agents started by `herdr_launch` get read tools unless the call sets `control: true` and the caller itself runs in control mode. Codex children get the requested mode. Claude children under a global entry get that entry's mode, because Claude cannot be overridden per launch. The result's `child_tools` reports the mode the child got, and a `note` appears when the global entry's mode differs from the request.

### Resume after a Herdr restart

After a restart, Herdr resumes agents only if its own Claude and Codex integration hooks are installed. Without them, restored panes come back without their agents, whether or not this server is installed. `install --status` reports the hook state. This installer never installs the hooks. To do it yourself: `herdr integration install claude` and `herdr integration install codex`.

### Updates

On Herdr 0.9.5 or later, run `herdr plugin update herdr-mcp`. Herdr builds the update into a new directory, and the plugin's last build step re-points `~/.local/bin/herdr-mcp` at it. That step only touches a shim that already points at a Herdr plugin build: it never creates one, never changes client config, and leaves a shim you pointed elsewhere alone. On older Herdr, uninstall and reinstall the plugin. Check the result with `herdr-mcp install --status`.

When the shim moves to a new build, the same step restarts a running remote service (`dev.herdr-mcp.tunnel` or `dev.herdr-mcp.ngrok`) so it runs the new code too. The service lets running tool calls finish for up to 10 seconds first; a wait cut off by the restart resumes with `herdr_wait` and its turn token. ChatGPT reconnects on its own, but refresh the connector when tool options change.

A plugin update replaces the binary behind the shim. Every Claude and Codex session you start afterwards runs the new code, and in control mode it does so with shell authority. Reinstall the plugin only from a revision you trust.

## Tools

Read mode registers the four observe tools, the three mail tools and the two session tools.

| Tool | Does | Key params | Returns |
|---|---|---|---|
| `herdr_capabilities` | Caller, tool mode, wait cap, agent kinds and MCP status | none | `inbox` (remote only), `caller`, `tool_mode`, `session{name, socket, home}`, `sessions[]`, `herdr{version, protocol}`, `agents[]`, `wait_cap_ms` |
| `herdr_sessions` | List Herdr sessions (`default`, named, and a custom start socket) | none | `sessions[{name, socket, running, active}]` |
| `herdr_use_session` | Switch the session every tool uses. The caller pane exists only in the home session (the one the bridge started in) | `name` | `active`, `socket`, `previous` |
| `herdr_list` | Workspace, tab and pane tree, with agents and plain processes | `scope`, `kind`, `status[]`, `label_contains`, `tail` (last N lines per pane, max 40) | tree of panes with status, foreground process and `tail[]` |
| `herdr_read` | Tail of a pane's output | `target`, `source` (`recent` or `visible`), `lines` (120, max 1000), `max_chars` | `text`, `truncated` (this call cut text), `more_above` (older output exists), `status`, `note` |
| `herdr_wait` | Wait for one condition | `target`, then one of `turn`, `until[]`, `output{match, after_command?}`, `process_exit`; `timeout_ms` | `matched`, `timed_out`, `status`, `excerpt`; `process_exit` with nothing running returns `result: no_active_work` |
| `herdr_mail_send` | Mail a pane or agent. An idle or done agent gets a fixed nudge, never the body | `to` (pane id or agent name), `body` (16 KiB max), `reply_to`, `notify` (true), `allow_self`, `inbox` (remote only: the reply inbox) | `id`, `to{pane, name}` (`pane: null` for an offline name), `delivered`, `nudged`, `nudge_skipped` |
| `herdr_mail_read` | Read your unread mail, oldest first | `from` (agent name or pane id), `mark_read` (true), `limit` (50), `inbox` (remote only) | `messages[{id, ts_ms, from, to, body, reply_to}]`, `remaining` |
| `herdr_mail_wait` | Wait until your inbox has unread mail; does not consume it | `from` (agent name or pane id), `timeout_ms`, `inbox` (remote only) | `matched`, `timed_out`, `count` |
| `herdr_launch` | Start an agent in a new or existing pane | `kind` (any Herdr agent kind; MCP only for claude and codex), `name`, `instructions`, `where`, `cwd`, `env`, `label`, `control`, `wait` (none or turn), `reply` (screen or mail), `start_timeout_ms`, `dialogs` (skip, surface or trust), `runtime_mode` (inherit, approval-required, auto-accept-edits or full-access), `allow_full_access` | `phase`, `pane`, `tab`, `workspace`, `cwd`, `warning`, `turn`, `mcp`, `child_tools`, `runtime_mode`, `caller_runtime_mode`, `dialogs`, `note`, and the wait result with `wait: turn`; with `reply: mail`, `reply` (`{id, body, from, ts_ms}`, `null` or `"pending"`) and `rewait` on timeout |
| `herdr_run` | Type a shell command into a pane | `command`, `where`, `cwd`, `env`, `label`, `ready{match, timeout_ms}` | `pane`, `tab`, `workspace`, `cwd`, `warning`, `pgid`, `ready_matched`, `timed_out`, `excerpt`, `rewait` (when ready timed out), `note` |
| `herdr_send` | Send a prompt, text or keys | `target`, then `prompt` with `mode`, `send_now`, `wait` (`turn` waits for the reply) and `reply` (screen or mail), or `text`, or `keys[]`; `answer_blocked` | `result`, `delivery`, `turn`, `note`; with `wait: turn` also `matched`, `timed_out`, `excerpt`; with `reply: mail`, `reply` (`{id, body, from, ts_ms}`, `null` or `"pending"`) |
| `herdr_interrupt` | Cancel the current work without closing the pane | `target`, `method` (auto, keys, signal), `signal` | `result`, `status_after` |
| `herdr_close` | Close a pane or tab | `target`, `allow_self`, `force` | `closed`, `kind`, `id` |
| `herdr_move` | Move a pane you launched into a new tab of another workspace, keeping its process | `target`, `workspace_id`, `label`, `allow_self`, `force` | `moved`, `previous{pane, tab, workspace}`, `pane`, `tab`, `workspace`, `cwd`, `note` |
| `herdr_file_read` | Read a file an agent wrote, in pages | `path`, `offset` (0), `max_bytes` (50000, max 75000) | `path`, `size`, `offset`, `bytes`, `encoding` (`utf8` or `base64`), `content`, `next_offset`, `eof` |

Behaviour worth knowing:
- A `target` is a pane ID or an agent name. `herdr_close` also takes a tab ID, and refuses a workspace.
- `herdr_read` counts meaningful lines. Blank rows and box-drawing rules are skipped, so a full-screen agent still returns its reply.
- Output matches are substring matches. There is no regex. Only text added after the command was sent can match, and the echoed command never does.
- A plain `herdr_wait {output}` sees only text added after it starts, so a line printed between two waits is missed. When `herdr_run`'s ready match times out it returns `rewait`; pass it to `herdr_wait` unchanged. `after_command` counts only lines after the last echo of that command, so a line already printed matches at once. If the echo has scrolled out of the pane's recent output, the wait falls back to a fresh baseline and adds a `note`.
- `herdr_run` finds the command's process group by the command's first word, falls back to a group that stays stable, and otherwise returns `pgid: null` with a note. Do not wait on a process exit then.
- `herdr_send` modes: `auto` queues the prompt into a working agent that queues typed input (Claude Code: `delivery: queued`, or `unconfirmed` when its queue marker never shows) and returns `busy` at once for any other working agent, with nothing typed; a known dialog on screen is refused before typing; `after_idle` waits for idle, `steer` types into a working agent and returns no turn, `restart` interrupts first. It never answers a blocked agent unless `answer_blocked` is set and the caller launched it.
- `herdr_send` with a prompt reports `delivery`: `started` (a new turn on an idle agent), `steered` (typed into a working agent), `queued` (a working Claude Code queued it: the visible screen shows its queued-message marker, read up to three times; `note` explains), `unconfirmed` (the marker never showed) or `sent_now` (queued, then `send_now: true` pressed Claude's `ctrl+enter`, which interrupts the current turn flow). `send_now` is ignored, with a `note`, for kinds without such a key.
- `herdr_list` shows each agent's `title` (its `title`, else its stripped terminal title, at most 120 characters), and `label_contains` also matches it, so a client can find an agent without reading panes.
- `herdr_launch` handles startup dialogs that Herdr reports as idle, such as Codex's hook review or a folder trust prompt (checked against the visible screen after readiness, at most 3 per launch). `dialogs: skip` (default) sends the option that grants nothing, usually `esc`; `surface` types nothing and stops with `agent_needs_input`; `trust` sends the permissive keys and only belongs on an explicit user request. The result's `dialogs` lists `{id, action, verified}` for each one handled; `verified: false` means the screen text and keys were not checked against the real agent (codex `directory_trust`, claude `folder_trust`). Skipping Codex's hook review adds a `note`: Herdr resume hooks will not run for that child. `herdr_send` with `answer_blocked` may also answer such a dialog on your own child while Herdr reports it idle.
- `herdr_launch` `runtime_mode` starts a codex or claude child with known permissions instead of whatever the global config allows: codex `-a`/`-s` (`on-request` + `read-only`, `on-request` + `workspace-write`, `never` + `danger-full-access`), claude `--permission-mode` (`manual`, `acceptEdits`, `bypassPermissions`). `inherit` (default) adds no flags. A mode above the caller's is refused. The caller's mode is read from the argv of a verified caller pane's foreground codex or claude; with no flags, or an unverified caller, it is `unknown`, which allows up to `auto-accept-edits` and needs `allow_full_access: true` for `full-access` (only when the user asked). The result reports `runtime_mode` and `caller_runtime_mode` (`known: <mode>` or `unknown`). Flags from config files are invisible to the bridge.
- `herdr_launch` `reply` picks how the child's result comes back. `screen` (default) leaves it on the child's screen, where it arrives as the wait `excerpt`. `mail` appends one paragraph to the child's instructions telling it to send its complete final result as the body of one `herdr_mail_send` call to you (your agent name if you have one, else your pane id). It is checked before anything is created: the caller must be a verified local caller (else `invalid_params`) or a remote one, whose reply goes to its inbox and the kind must get this MCP server, as `global` or `injected` in `herdr_capabilities` (else `reply_unsupported`). With `wait: turn` the launch also waits for unread mail from the child (its pane id or agent name as sender), up to the rest of the wait cap, and consumes only that message. The result then carries `reply: {id, body, from, ts_ms}` and the child's `status`, with no `excerpt`. If the child's turn settles (done or idle) and no mail arrives within 5 s, `reply` is `null`, `note` says the child finished without mailing, and the usual `excerpt` is returned. On timeout, `timed_out` is true and `rewait` is `{"mail": {"from": "<child name>"}}`: pass its `from` to `herdr_mail_wait` and `herdr_mail_read`. With `wait: none`, `reply` is `"pending"`: end your turn, and the mailbox nudge wakes you once the child mails; a remote caller reads its inbox with `herdr_mail_wait`. `phase` and cleanup are unchanged.
- `herdr_send` takes the same `reply: mail` for a prompt to an existing agent, which must have the herdr mail tool. With `wait: turn` it waits for the agent's mail and its turn like a launch; a prompt with no turn (queued, steered or unconfirmed) and any send without `wait: turn` return `reply: "pending"`. Use it for an agent's whole final answer instead of reading long scrollback.
- Every excerpt (`herdr_launch`, `herdr_wait`, `herdr_run`) comes from Herdr's unwrapped scrollback, so a long line is not split at the pane width.
- `herdr_launch` retries while a fresh pane reports busy, treats an agent as ready after a 3 s quiet window, and fails fast if the shell is back in front after a 3 s startup grace.
- `herdr_wait {process_exit}` returns `{result: "no_active_work", matched: false, timed_out: false}` when nothing was running. Do not re-wait.
- Waits never cancel work. A `timed_out` result means call again. The wait cap is 280 s for Codex, 600 s for Claude and 50 s otherwise.
- Idle is not success. No tool reports success: read the output and decide.
- `where` takes at most one of `pane`, `split` (`right` or `down`), `tab`, `workspace` or `worktree{branch, base}`. The default is a new tab. `split` takes `of`, the pane to split (pane ID or agent name); it defaults to the calling pane, and external callers must give it. `of` without `split` is an error, and so is an unknown key.
- A new tab or worktree goes into `where.workspace_id` (a workspace ID, or a label matched case-insensitively; an ID wins) when given, else the verified calling pane's workspace. A remote caller (a tunnel bridge, such as ChatGPT) never counts as a pane, whatever process started the bridge, and must give `workspace_id`: it gets `destination_required` with the list of workspaces instead of landing in whatever workspace has focus. A local caller with no verified pane keeps the focused workspace, with a `warning` saying so. A `cwd` outside the destination's worktree is allowed and returns a `warning`. Results give the resolved `workspace`, `tab`, `pane` and `cwd`.
- `herdr_move` fixes a misplaced pane without a restart: Herdr moves it, process and all, into a new tab of the named workspace. Its pane ID changes with the workspace; the result gives `previous` and the new IDs.
- `herdr_file_read` is how a caller with no shared filesystem, such as ChatGPT, gets a report or an artifact: a control tool, so read mode has none. The path must resolve, symlinks followed, under the working directory of a pane or agent in the active session, or under the temp directory. A pane in `/` or the home directory opens nothing, and `.ssh`, `.gnupg`, `.aws`, `.netrc` and `.git-credentials` are refused under any root. Text comes back as text, cut on a whole character; other bytes as base64. Pass `next_offset` as `offset` until `eof`. The audit log keeps the path and the byte count, never the content.

Malformed arguments (a missing field, a bad enum value, an unknown key) fail with `isError` and a plain-text message naming the field, with no `code`. Tool failures return `isError` with a `code` and a `message`. Codes worth knowing:
- `answer_refused`: `herdr_interrupt` would answer a blocked prompt.
- `agent_needs_input`: `herdr_launch` found a startup dialog it did not answer (`dialogs: surface`, one that stayed after the answer, or more than 3). The pane is kept; details carry `pane`, `phase` and `dialog{id, excerpt}`.
- `keys_failed`: `herdr_send` could not send the keys.
- `path_not_allowed`, `file_not_found`, `not_a_file`, `file_unreadable`: `herdr_file_read` refused or could not read the path. `path_not_allowed` lists the allowed `roots`.
- `cancelled`: the client cancelled a `herdr_send`, `herdr_launch` or `herdr_mail_wait`. Launch keeps the pane and reports `pane`, `phase` and `turn`.
- `runtime_mode_unsupported`: `herdr_launch` got a non-inherit `runtime_mode` for a kind other than codex or claude. Nothing is created.
- `runtime_mode_escalation_denied`: the requested `runtime_mode` is above the caller's mode, or `full-access` from an unknown caller without `allow_full_access`. Details carry `runtime_mode` and `caller_runtime_mode`. Nothing is created.
- `provenance_failed`: `herdr_launch` could not tag the pane as yours.
- `herdr_timeout`: Herdr did not reply in time.
- `no_caller`: `herdr_mail_read` or `herdr_mail_wait` has no verified caller pane.
- `reply_unsupported`: `herdr_launch` with `reply: mail` for a kind that gets no MCP server here, so the child has no mail tool. Nothing is created. `reply: mail` from an unverified local caller, or a launch without `instructions`, is `invalid_params`.
- `target_not_found`: `herdr_mail_send` to a pane id that does not exist. An unknown agent name is not an error: it is delivered to the name inbox.
- `body_too_large`: a mail body is over 16 KiB.
- `inbox_full`: the recipient has 500 unread messages.
- `mailbox_io`: the mailbox directory could not be read or written.
- `self_target`: `herdr_mail_send` to your own pane without `allow_self`.

## Mailbox

Agents in Herdr panes message each other with `herdr_mail_send`, `herdr_mail_read` and `herdr_mail_wait`. All three work in read and control mode. There is no daemon: every bridge process shares the filesystem.

- **Storage:** `${XDG_STATE_HOME:-~/.local/state}/herdr-mcp/mailbox/<session>/<inbox>/`, where `<session>` is 12 hex characters of a hash of the Herdr socket path and `<inbox>` is a pane inbox (the pane id with `:` as `_`) or a name inbox (`name-<name>`, the name with characters outside `A-Za-z0-9._-` as `_`). Directories are 0700, files 0600. A message is one JSON file in `new/`, written to a temp file and renamed. A read moves it to `read/`, and `read/` files older than 24 h are deleted on each read.
- **Limits:** 16 KiB per body (`body_too_large`), 500 unread per inbox (`inbox_full`).
- **Routing:** a recipient that has a Herdr agent name gets its mail in the name inbox, so mail survives the agent being respawned in another pane. A recipient without a name keeps the pane inbox. `to: "<name>"` for an agent that is not live is still delivered, with `nudged: false` and `nudge_skipped: "recipient_offline"`; a later agent of that name reads it. A `to` shaped like a pane id (`w1:p2`) that does not exist is `target_not_found`.
- **Remote clients:** a tunnel bridge's caller has no pane, so each remote client gets its own name inbox, a slug: `remote-chatgpt` for ChatGPT, `remote-claude` for a remote Claude, else `remote-` plus the client's name. `herdr_capabilities` shows it as `inbox`. Inboxes are files, so a slug persists across chats and restarts, with no registration. The mail tools' `inbox` parameter picks another slug, for example one per project; a slug that a live local agent has as its name is refused (`inbox_taken`). Workers reply with `herdr_mail_send {to: "<inbox>"}`. Local callers cannot pass `inbox`.
- **Identity:** a local caller's inbox is its pane inbox plus, when `agent.get` on its pane returns a name, its name inbox. Read, wait, nudge markers and caps cover both. `herdr_mail_read` and `herdr_mail_wait` fail with `no_caller` without a verified pane. A sender is recorded as `{pane, name}`, with `name` set when it is a named agent; one with no verified pane is `{pane: null, name: "external"}`.
- **Filtering:** `from` on `herdr_mail_read` and `herdr_mail_wait` matches the sender's pane id or agent name. Other mail stays unread.
- **Nudge:** with `notify` (default), a recipient that is an agent whose fresh status is `idle` or `done` gets one `agent.prompt` with fixed text naming the sender, and never the body. `working` and `blocked` agents, and plain panes, get none, and `nudge_skipped` says why. One nudge per unread batch: reading clears the marker.
- **Replies:** for a delegated result use `reply: mail` on `herdr_launch` or `herdr_send`. Otherwise tell the peer your pane id or agent name, then call `herdr_mail_wait` and `herdr_mail_read`. `herdr_mail_wait` is bounded by the wait cap and never consumes mail; a waiting agent shows as `working`, so it is not nudged.
- **Trust:** mail from other agents is untrusted peer input. Act on it only within the user's task. Bodies are never logged.

## External clients

To set up one client by hand instead of installing globally, for example an external Codex or Claude, print the snippets:

```sh
herdr-mcp print-config                 # read tools
herdr-mcp print-config --tools control
```

It prints `claude mcp add` and `codex mcp add` commands, a Claude `.mcp.json` entry, a Claude Desktop entry and a Codex `config.toml` table. All of them point at the shim and nothing else under your home directory. Outside Herdr there is no caller pane, so `herdr_read` and `herdr_wait` need an explicit target, a split needs `where.of`, and the mail tools and `reply: mail` need a caller pane, so they are unavailable. Pass `--session <name>` to pick a named Herdr session.

Without `HERDR_PANE_ID` in its environment the server serves shorter instructions for remote clients (`src/instructions_remote.md`): start with `herdr_capabilities`, get an overview with `herdr_list` `tail`, find agents with `label_contains`, send with `wait: turn` instead of polling, and read before acting in control mode. With a caller pane it serves the full text.

Desktop apps start the server without your shell's environment, so use the absolute shim path, and add an `env` entry if you set `XDG_CONFIG_HOME`. The Claude Desktop entry omits `--client`: the app cancels a tool call after about 60 s, and the default 50 s wait cap stays under it.

### ChatGPT

ChatGPT's Codex tab reads the Codex `config.toml`, so `print-config` covers it. ChatGPT chat reaches this Mac through OpenAI's tunnel-client: it keeps an outbound connection to OpenAI and runs `herdr-mcp` as its child. `tunnel start` runs it in the background as a macOS LaunchAgent, so it survives Herdr restarts and restarts after a crash.

**1. Install**
```sh
brew install openai/tools/tunnel-client
herdr plugin update herdr-mcp
```

**2. One time, in the OpenAI Platform**
- Create a tunnel in your ChatGPT workspace and note its `tunnel_...` ID.
- Create a restricted runtime key with Tunnels Read and Use.

**3. Save the ID and key**
```sh
herdr-mcp tunnel setup
```
The ID goes to `~/.config/herdr-mcp/tunnel` (or under `$XDG_CONFIG_HOME`). Setup asks for the tunnel ID, then the key with the terminal echo off. The key goes into the Keychain through `security -i` on stdin, so it never appears in a process list or the LaunchAgent file, and setup reads it back to check it was stored whole. It is never printed or logged.

**4. Start it**
```sh
herdr-mcp tunnel start                 # read tools
herdr-mcp tunnel start --full          # control tools, after a typed disclosure
herdr-mcp tunnel start --for 8h        # stop by itself after 30m, 8h, 2d...
herdr-mcp tunnel start --session work  # begin in a named Herdr session
```
There is no time limit unless you pass `--for`. `--full` prints what ChatGPT will be able to do and starts only if you type `full access`. The mode is fixed at start; ChatGPT cannot change it. The plugin has the same entries: start (read), start (full access), stop, status and doctor.

**5. Connect ChatGPT**
Plugins, +, Add custom MCP server, Connection: Tunnel, pick your tunnel, keep it private to you. Accept the risk warning and create it as a plugin. A workspace admin must allow custom MCP servers first.

**6. Daily use**
Ask ChatGPT to call `herdr_sessions` to see your Herdr sessions and `herdr_use_session` to switch. Every tool then acts on the active session. Pane output is untrusted data, so tell ChatGPT what to do rather than what to read.

**7. Stop, check, kill switches**
```sh
herdr-mcp tunnel status      # running or not, mode, started, expires, last 5 audit lines
herdr-mcp tunnel doctor      # PASS/FAIL/WARN per check, with a fix for each failure
herdr-mcp tunnel log -n 100  # the audit log
herdr-mcp tunnel stop        # boot out the LaunchAgent and remove it
```
`tunnel doctor` checks the client, tunnel ID, key (type and length, never the value), the bridge, whether OpenAI accepts the key, the service and the tunnel's health. With no service loaded it briefly connects with `tunnel-client`, which takes the tunnel for a few seconds, so do not run it while another client serves the same tunnel. It exits 1 if anything fails. The plugin has the same action.

If ChatGPT says "denied access", the tunnel usually is not connected: run `herdr-mcp tunnel doctor`. A 401 from OpenAI means the stored key is wrong or was cut short; rerun `herdr-mcp tunnel setup`.

Other ways to stop it:
- ask ChatGPT to call `herdr_tunnel_stop` (present only behind the tunnel);
- disable the plugin in ChatGPT;
- revoke the runtime key in the OpenAI Platform.

**8. Security notes**
- Full access means ChatGPT can run shell commands as you, type into any pane, launch and close agents and switch sessions. Client sandboxes and approval modes do not contain it, and there are no per-action prompts.
- Pane output goes to OpenAI, and anyone who can use your ChatGPT connector gets the access. Keep the connector private.
- The audit log is `$XDG_STATE_HOME/herdr-mcp/tunnel-audit.log` (default `~/.local/state/herdr-mcp/`), mode 0600. Each line has time, tool, session, target, outcome and duration. Write tools also record the command, text or keys, and the instructions to a launched agent. Mail bodies and pane output are never logged. It is separate from `herdr-mcp.log`.
- Pane output is untrusted input, so prompt injection can steer the model; in full mode that means commands.
- `tunnel start` writes `~/Library/LaunchAgents/dev.herdr-mcp.tunnel.plist` and logs to `tunnel.out.log` and `tunnel.err.log` in the state directory. `tunnel run --full` refuses to start without the confirmation `start --full` records.
- `tunnel run [--session <name>]` still works in the foreground for debugging; `herdr_tunnel_stop` and `tunnel stop` only manage the background service. It reads `CONTROL_PLANE_API_KEY` if set, else the Keychain.
- The background service is macOS only. On Linux, use `tunnel run`.
- The tunnel-client binary is not notarized: macOS may block it on first run. Allow it in System Settings, Privacy and Security.
- Limits: 120 s per request, 10 MiB per message.

### ChatGPT over ngrok (OAuth)

An alternative to OpenAI's tunnel: ChatGPT reaches `https://<domain>/mcp` over an ngrok static domain. A hosted OAuth provider logs you in, and `herdr-mcp` accepts only its JWTs for the owners you name.

```sh
brew install ngrok && ngrok config add-authtoken <token>
herdr-mcp tunnel ngrok setup                 # domain, issuer, owners, audience -> ~/.config/herdr-mcp/ngrok.toml
herdr-mcp tunnel ngrok start [--full] [--for 8h] [--session work]
herdr-mcp tunnel ngrok status | doctor | stop
```

In ChatGPT: Plugins, +, custom MCP server, URL `https://<domain>/mcp`, Authentication OAuth.

The provider must support:
- dynamic client registration (`registration_endpoint` in its discovery document) and PKCE;
- the redirect `https://chatgpt.com/connector/oauth/*`;
- an audience equal to the resource URL (default `https://<domain>/mcp`);
- restricting sign-in to you, since the owner list is the only check after a token verifies.

`start` runs `tunnel ngrok run` as the LaunchAgent `dev.herdr-mcp.ngrok`: it binds a loopback port, then starts ngrok on it. `--full` needs the same typed disclosure as the OpenAI tunnel. To revoke, end the sessions or rotate the signing keys at the provider; `tunnel ngrok stop` is the local kill switch. Logs are `ngrok.out.log` and `ngrok.err.log`; `status` shows the last `auth denied:` reason.

## Logs

The bridge writes `herdr-mcp.log` in `herdr-mcp/` next to the Herdr socket, with mode 0600. Lines hold the tool name, an outcome code and a duration. They never hold pane output, prompts or commands.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -j 4 -- --test-threads 4
```

Tests run against a fake Herdr socket and fake `claude` and `codex` executables. Live acceptance runs only in a scratch Herdr session.
