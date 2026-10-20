# Herdr MCP agent instructions

Read README.md and docs/brief.md, docs/source-context.md, and docs/handoff.md before working.

## Scope

Build a coherent MCP interface over Herdr's existing multiplexer automation. Preserve workspaces, tabs, panes, agents, and non-agent processes. Reuse the CLI/socket API. Do not narrow this project to subagent delegation or assume a durable task engine is required.

Research and plan first. No implementation, commits, pushes, remote creation, PRs, plugin installation, global agent config changes, or live process interruptions until the user explicitly requests the relevant step. Treat neighboring Herdr/T3 source checkouts as read-only references.

Repository-local Git email must remain ozeron@me.com; do not change global Git identity.

Distinguish verified source behavior, installed-binary behavior, proposals, and unresolved decisions. Never interpret an idle terminal as proof of task success. Do not record credentials or private terminal output in docs.

## Agent feedback

When a skill, doc, script, or instruction misleads you, or the user corrects you, append one entry to ~/.agent-feedback/herdr-mcp/<branch with / as _>.md outside the worktree.
Format: `### <date> · <skill|doc|command> · blocked|slowed|wrong`, then `Hit:` (exact error or correction), `Workaround:`, `Fix:` (file + edit). Friction only; no progress notes or secrets.
