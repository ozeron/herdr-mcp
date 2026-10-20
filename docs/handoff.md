# Agent handoff

## Requested setup

User: "create a code project with empty git and spawn pr-open there and seed with docs and context, also configure project to my personal email ozeron@me.com".

Project: /Users/ozeron/code/herdr-mcp. Use the pr-open workspace/agent launch workflow adapted to a new local project with no commits or remote. No GitHub PR can exist yet, and none is requested by this local setup.

## First task

Read the project brief and source context. Verify the relevant neighboring source paths, then draft a concise architecture proposal in docs/design.md covering:

1. A small, coherent MCP tool surface for agents and generic terminal processes.
2. Mapping to Herdr's existing CLI/socket API and workspace/tab/pane model.
3. Plugin packaging, service startup/shutdown/restart, output and log handling.
4. External client connection and automatic internal-agent injection, starting with concrete Codex/Claude launch paths.
5. Explicit gaps requiring Herdr changes versus features achievable entirely in the plugin.

Preserve existing product scope; choose reasonable technical defaults and explain them. Label proposals and assumptions. Do not implement or mutate live Herdr processes, source checkouts, or global configuration. Do not commit, push, create remotes, or open PRs. Report the draft and the one most consequential unresolved decision.

## Status (2026-10-20)

- Plan units U1 to U11 are done. The code, README and design.md describe the same behaviour.
- U1 probe answers are in [probes.md](probes.md).
- Shipped: the nine tools, read and control modes, the opt-in installer, the plugin manifest and `print-config`.
- Decided: agents the bridge did not launch get Herdr MCP through an opt-in global install, read-only unless `--control` (design §6.1).
- Open: whether a delegating agent is woken when its child finishes, or polls (design §10).
- The implementation plan is [plans/2026-10-20-001-feat-herdr-mcp-v1-bridge-plan.md](plans/2026-10-20-001-feat-herdr-mcp-v1-bridge-plan.md). It supersedes design.md where they differ, and U10 synced the two.
- U11 live acceptance ran: 9/9 passed (probes.md, "Live acceptance").
- The code review ran. Its findings are fixed in commits d1c3f65 plus the review batch after it.
- Not yet verified: restart and resume (needs Herdr's integration hooks), `herdr plugin link` accepting the manifest, and the install and remove panes in a real Herdr pane.
