# Herdr MCP

An MCP orchestration interface for the Herdr terminal multiplexer, inspired by T3 Code's app-owned orchestration and automatic MCP injection.

## Intended experience

Discover agents; launch work with instructions; send follow-ups; read progress and terminal logs; wait for agent states or process readiness; interrupt work. Workspaces, tabs, and panes remain first-class context. A pane may contain an agent, server, tests, or logs.

The same service should be usable by external MCP clients and agents running inside Herdr. Supported agents inside Herdr should receive MCP configuration automatically.

## Project state

Fresh local Git repository, documentation only. No implementation, commits, remote, or PR yet. Repository-local Git email: `ozeron@me.com`.

Start with [the product brief](docs/brief.md), [verified source context](docs/source-context.md), and [the agent handoff](docs/handoff.md).

The draft architecture is in [docs/design.md](docs/design.md). It uses a stdio bridge per client and nine tools modelled on T3's thread tools, with an opt-in global install into Claude and Codex.
