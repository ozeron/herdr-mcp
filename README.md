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

A plugin update replaces the binary behind the shim. Every Claude and Codex session you start afterwards runs the new code, and in control mode it does so with shell authority. Reinstall the plugin only from a revision you trust.

## Tools

Read mode registers the four observe tools and the three mail tools.

| Tool | Does | Key params | Returns |
|---|---|---|---|
| `herdr_capabilities` | Caller, tool mode, wait cap, agent kinds and MCP status | none | `caller`, `tool_mode`, `herdr{version, protocol}`, `agents[]`, `wait_cap_ms` |
| `herdr_list` | Workspace, tab and pane tree, with agents and plain processes | `scope`, `kind`, `status[]`, `label_contains` | tree of panes with status and foreground process |
| `herdr_read` | Tail of a pane's output | `target`, `source`, `lines` (80, max 1000), `max_chars`, `raw` | `text`, `truncated`, `status`, `note` |
| `herdr_wait` | Wait for one condition | `target`, then one of `turn`, `until[]`, `output{match, after_command?}`, `process_exit`; `timeout_ms` | `matched`, `timed_out`, `status`, `excerpt`; `process_exit` with nothing running returns `result: no_active_work` |
| `herdr_mail_send` | Mail a pane or agent. An idle or done agent gets a fixed nudge, never the body | `to`, `body` (16 KiB max), `reply_to`, `notify` (true), `allow_self` | `id`, `to{pane, name}`, `delivered`, `nudged`, `nudge_skipped` |
| `herdr_mail_read` | Read your unread mail, oldest first | `mark_read` (true), `limit` (50) | `messages[{id, ts_ms, from, to, body, reply_to}]`, `remaining` |
| `herdr_mail_wait` | Wait until your inbox has unread mail; does not consume it | `timeout_ms` | `matched`, `timed_out`, `count` |
| `herdr_launch` | Start an agent in a new or existing pane | `kind` (any Herdr agent kind; MCP only for claude and codex), `name`, `instructions`, `where`, `cwd`, `env`, `label`, `control`, `wait` (none or turn), `start_timeout_ms`, `dialogs` (skip, surface or trust), `runtime_mode` (inherit, approval-required, auto-accept-edits or full-access), `allow_full_access` | `phase`, `pane`, `turn`, `mcp`, `child_tools`, `runtime_mode`, `caller_runtime_mode`, `dialogs`, `note`, and the wait result with `wait: turn` |
| `herdr_run` | Type a shell command into a pane | `command`, `where`, `cwd`, `env`, `label`, `ready{match, timeout_ms}` | `pane`, `pgid`, `ready_matched`, `timed_out`, `excerpt`, `rewait` (when ready timed out), `note` |
| `herdr_send` | Send a prompt, text or keys | `target`, then `prompt` with `mode`, or `text`, or `keys[]`; `answer_blocked` | `result`, `turn` |
| `herdr_interrupt` | Cancel the current work without closing the pane | `target`, `method` (auto, keys, signal), `signal` | `result`, `status_after` |
| `herdr_close` | Close a pane or tab | `target`, `allow_self`, `force` | `closed`, `kind`, `id` |

Behaviour worth knowing:
- A `target` is a pane ID or an agent name. `herdr_close` also takes a tab ID, and refuses a workspace.
- `herdr_read` counts meaningful lines. Blank rows and box-drawing rules are skipped, so a full-screen agent still returns its reply. `raw: true` returns the rows as they are.
- Output matches are substring matches. There is no regex. Only text added after the command was sent can match, and the echoed command never does.
- A plain `herdr_wait {output}` sees only text added after it starts, so a line printed between two waits is missed. When `herdr_run`'s ready match times out it returns `rewait`; pass it to `herdr_wait` unchanged. `after_command` counts only lines after the last echo of that command, so a line already printed matches at once. If the echo has scrolled out of the pane's recent output, the wait falls back to a fresh baseline and adds a `note`.
- `herdr_run` finds the command's process group by the command's first word, falls back to a group that stays stable, and otherwise returns `pgid: null` with a note. Do not wait on a process exit then.
- `herdr_send` modes: `auto` refuses a working agent with `busy`, `after_idle` waits for idle, `steer` types into a working agent and returns no turn, `restart` interrupts first. It never answers a blocked agent unless `answer_blocked` is set and the caller launched it.
- `herdr_launch` handles startup dialogs that Herdr reports as idle, such as Codex's hook review or a folder trust prompt (checked against the visible screen after readiness, at most 3 per launch). `dialogs: skip` (default) sends the option that grants nothing, usually `esc`; `surface` types nothing and stops with `agent_needs_input`; `trust` sends the permissive keys and only belongs on an explicit user request. The result's `dialogs` lists `{id, action, verified}` for each one handled; `verified: false` means the screen text and keys were not checked against the real agent (codex `directory_trust`, claude `folder_trust`). Skipping Codex's hook review adds a `note`: Herdr resume hooks will not run for that child. `herdr_send` with `answer_blocked` may also answer such a dialog on your own child while Herdr reports it idle.
- `herdr_launch` `runtime_mode` starts a codex or claude child with known permissions instead of whatever the global config allows: codex `-a`/`-s` (`on-request` + `read-only`, `on-request` + `workspace-write`, `never` + `danger-full-access`), claude `--permission-mode` (`manual`, `acceptEdits`, `bypassPermissions`). `inherit` (default) adds no flags. A mode above the caller's is refused. The caller's mode is read from the argv of a verified caller pane's foreground codex or claude; with no flags, or an unverified caller, it is `unknown`, which allows up to `auto-accept-edits` and needs `allow_full_access: true` for `full-access` (only when the user asked). The result reports `runtime_mode` and `caller_runtime_mode` (`known: <mode>` or `unknown`). Flags from config files are invisible to the bridge.
- `herdr_launch` retries while a fresh pane reports busy, treats an agent as ready after a 3 s quiet window, and fails fast if the shell is back in front after a 3 s startup grace.
- `herdr_wait {process_exit}` returns `{result: "no_active_work", matched: false, timed_out: false}` when nothing was running. Do not re-wait.
- Waits never cancel work. A `timed_out` result means call again. The wait cap is 280 s for Codex, 600 s for Claude and 50 s otherwise.
- Idle is not success. No tool reports success: read the output and decide.
- `where` takes at most one of `pane`, `split` (`right` or `down`), `tab`, `workspace` or `worktree{branch, base}`. The default is a new tab. `split` takes `of`, the pane to split (pane ID or agent name); it defaults to the calling pane, and external callers must give it. `of` without `split` is an error, and so is an unknown key.

Malformed arguments (a missing field, a bad enum value, an unknown key) fail with `isError` and a plain-text message naming the field, with no `code`. Tool failures return `isError` with a `code` and a `message`. Codes worth knowing:
- `answer_refused`: `herdr_interrupt` would answer a blocked prompt.
- `agent_needs_input`: `herdr_launch` found a startup dialog it did not answer (`dialogs: surface`, one that stayed after the answer, or more than 3). The pane is kept; details carry `pane`, `phase` and `dialog{id, excerpt}`.
- `keys_failed`: `herdr_send` could not send the keys.
- `cancelled`: the client cancelled a `herdr_send`, `herdr_launch` or `herdr_mail_wait`. Launch keeps the pane and reports `pane`, `phase` and `turn`.
- `runtime_mode_unsupported`: `herdr_launch` got a non-inherit `runtime_mode` for a kind other than codex or claude. Nothing is created.
- `runtime_mode_escalation_denied`: the requested `runtime_mode` is above the caller's mode, or `full-access` from an unknown caller without `allow_full_access`. Details carry `runtime_mode` and `caller_runtime_mode`. Nothing is created.
- `provenance_failed`: `herdr_launch` could not tag the pane as yours.
- `herdr_timeout`: Herdr did not reply in time.
- `no_caller`: `herdr_mail_read` or `herdr_mail_wait` has no verified caller pane.
- `body_too_large`: a mail body is over 16 KiB.
- `inbox_full`: the recipient has 500 unread messages.
- `mailbox_io`: the mailbox directory could not be read or written.
- `self_target`: `herdr_mail_send` to your own pane without `allow_self`.

## Mailbox

Agents in Herdr panes message each other with `herdr_mail_send`, `herdr_mail_read` and `herdr_mail_wait`. All three work in read and control mode. There is no daemon: every bridge process shares the filesystem.

- **Storage:** `${XDG_STATE_HOME:-~/.local/state}/herdr-mcp/mailbox/<session>/<pane>/`, where `<session>` is 12 hex characters of a hash of the Herdr socket path and `<pane>` is the pane id with `:` as `_`. Directories are 0700, files 0600. A message is one JSON file in `new/`, written to a temp file and renamed. A read moves it to `read/`, and `read/` files older than 24 h are deleted on each read.
- **Limits:** 16 KiB per body (`body_too_large`), 500 unread per inbox (`inbox_full`).
- **Identity:** the inbox belongs to the verified caller pane. `herdr_mail_read` and `herdr_mail_wait` fail with `no_caller` without one. A sender with no verified pane is recorded as `{pane: null, name: "external"}`.
- **Nudge:** with `notify` (default), a recipient that is an agent whose fresh status is `idle` or `done` gets one `agent.prompt` with fixed text naming the sender, and never the body. `working` and `blocked` agents, and plain panes, get none, and `nudge_skipped` says why. One nudge per unread batch: reading clears the marker.
- **Replies:** tell a child your pane id, then call `herdr_mail_wait` and `herdr_mail_read`. `herdr_mail_wait` is bounded by the wait cap and never consumes mail; a waiting agent shows as `working`, so it is not nudged.
- **Trust:** mail from other agents is untrusted peer input. Act on it only within the user's task. Bodies are never logged.

## External clients

To set up one client by hand instead of installing globally, for example an external Codex or Claude, print the snippets:

```sh
herdr-mcp print-config                 # read tools
herdr-mcp print-config --tools control
```

It prints `claude mcp add` and `codex mcp add` commands, a Claude `.mcp.json` entry and a Codex `config.toml` table. All of them point at the shim and nothing else under your home directory. Outside Herdr there is no caller pane, so `herdr_read` and `herdr_wait` need an explicit target. Pass `--session <name>` to pick a named Herdr session.

## Logs

The bridge writes `herdr-mcp.log` in `herdr-mcp/` next to the Herdr socket, with mode 0600. Lines hold the tool name, an outcome code and a duration. They never hold pane output, prompts or commands.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -j 4 -- --test-threads 4
```

Tests run against a fake Herdr socket and fake `claude` and `codex` executables. Live acceptance runs only in a scratch Herdr session.
