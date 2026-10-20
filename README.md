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

Read mode registers the first four.

| Tool | Does | Key params | Returns |
|---|---|---|---|
| `herdr_capabilities` | Caller, tool mode, wait cap, agent kinds and MCP status | none | `caller`, `tool_mode`, `herdr{version, protocol}`, `agents[]`, `wait_cap_ms` |
| `herdr_list` | Workspace, tab and pane tree, with agents and plain processes | `scope`, `kind`, `status[]`, `label_contains` | tree of panes with status and foreground process |
| `herdr_read` | Tail of a pane's output | `target`, `source`, `lines` (80, max 1000), `max_chars`, `raw` | `text`, `truncated`, `status`, `note` |
| `herdr_wait` | Wait for one condition | `target`, then one of `turn`, `until[]`, `output{match, after_command?}`, `process_exit`; `timeout_ms` | `matched`, `timed_out`, `status`, `excerpt`; `process_exit` with nothing running returns `result: no_active_work` |
| `herdr_launch` | Start an agent in a new or existing pane | `kind` (any Herdr agent kind; MCP only for claude and codex), `name`, `instructions`, `where`, `cwd`, `env`, `label`, `control`, `wait` (none or turn), `start_timeout_ms`, `dialogs` (skip, surface or trust) | `phase`, `pane`, `turn`, `mcp`, `child_tools`, `dialogs`, `note`, and the wait result with `wait: turn` |
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
- `herdr_launch` retries while a fresh pane reports busy, treats an agent as ready after a 3 s quiet window, and fails fast if the shell is back in front after a 3 s startup grace.
- `herdr_wait {process_exit}` returns `{result: "no_active_work", matched: false, timed_out: false}` when nothing was running. Do not re-wait.
- Waits never cancel work. A `timed_out` result means call again. The wait cap is 280 s for Codex, 600 s for Claude and 50 s otherwise.
- Idle is not success. No tool reports success: read the output and decide.
- `where` takes at most one of `pane`, `split` (`right` or `down`), `tab`, `workspace` or `worktree{branch, base}`. The default is a new tab.

Malformed arguments return a JSON-RPC `invalid_params` error. Tool failures return `isError` with a `code` and a `message`. Codes worth knowing:
- `answer_refused`: `herdr_interrupt` would answer a blocked prompt.
- `agent_needs_input`: `herdr_launch` found a startup dialog it did not answer (`dialogs: surface`, one that stayed after the answer, or more than 3). The pane is kept; details carry `pane`, `phase` and `dialog{id, excerpt}`.
- `keys_failed`: `herdr_send` could not send the keys.
- `cancelled`: the client cancelled a `herdr_send` or `herdr_launch`. Launch keeps the pane and reports `pane`, `phase` and `turn`.
- `provenance_failed`: `herdr_launch` could not tag the pane as yours.
- `herdr_timeout`: Herdr did not reply in time.

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
