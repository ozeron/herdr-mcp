# Product brief

## User direction

Use T3 Code Orchestrator as the inspiration for structuring Herdr MCP overall. The interface should discover agents, launch work and instructions, read progress, wait, and interrupt. Both external clients (for example Codex) and agents inside Herdr should connect. Internal agents should get automatic MCP configuration where supported.

Herdr is a multiplexer. A workspace or tab can organize server processes and their logs as well as agents. Existing automation is useful but clunky; the goal is a coherent higher-level surface over it.

The user explicitly corrected a proposal that narrowed this to internal versus external delegation and emphasized task tracking. Do not repeat that narrowing or make durable tasks a prerequisite.

## Current proposed direction

Package the MCP bridge as a Herdr plugin, reuse the existing Herdr API, and investigate a narrow provider launch integration for automatic MCP injection. The user accepted creating a new project seeded with these findings. This is a design direction, not proof all required extension hooks exist.

## Required capabilities

- Discover agents and workspace/tab/pane context, including non-agent panes.
- Launch supported agents with instructions; send subsequent instructions.
- Run server/build/test processes in suitable panes and inspect their output.
- Read bounded progress/log output and report process/agent status.
- Wait for agent state or matching output, with clear timeout semantics.
- Interrupt the intended agent/process without conflating interrupt with pane destruction.
- Offer the same service to internal agents and external MCP clients.
- Inject connection configuration for supported internal agent launch paths.

## Open design questions

Choose a compact tool surface, hosting/lifecycle strategy, transport/authentication and caller context, multi-session scope, bounded output semantics, and provider-specific injection/interrupt behavior. Investigate manual agent starts as well as managed launches. Declare any compatibility limits against installed Herdr 0.9.3. A server tab is one hosting option, not a requirement that every tab becomes a separate server.

## Example acceptance workflows

1. External Codex discovers Herdr agents and reads a selected agent's recent progress.
2. An internal agent starts a server in its workspace, waits for a readiness message, and reads output.
3. An agent launches another supported agent in a separate tab with instructions, waits for a state transition, and reads the response.
4. A client interrupts selected work, observes its resulting state, and keeps unrelated panes running.
5. A supported managed agent launch receives the Herdr MCP configuration without manual per-project setup.
