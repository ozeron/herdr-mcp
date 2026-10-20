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

- First task done. The draft is in [design.md](design.md).
- Decided: agents that the bridge did not launch get Herdr MCP through an opt-in global install (design §6.1).
- Open: whether a delegating agent is woken when its child finishes, or polls (design §10).
- The implementation plan is [plans/2026-10-20-001-feat-herdr-mcp-v1-bridge-plan.md](plans/2026-10-20-001-feat-herdr-mcp-v1-bridge-plan.md), and it supersedes design.md where they differ. Its review findings are applied: read-only install by default with control opt-in, version-based compatibility, Herdr's prompt activity gate, and bridge-side output matching.
- Next: plan U1, the probes in a scratch Herdr session with scratch `CLAUDE_CONFIG_DIR`/`CODEX_HOME`, once the user asks for that step.
