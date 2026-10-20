# Verified source context

Research date: 2026-10-20. These are pinned checkout observations, not live production guarantees.

## References

- Herdr: ../herdr, https://github.com/herdrdev/herdr, commit 143bf5fd4781d164260c63fe28e7dc08054ad32f.
- T3 Code: ../t3code, https://github.com/pingdotgg/t3code, commit b5a0696a989a171530cc3c0ff831a99f51ce6f09.
- Installed binary: `herdr 0.9.3`; source checkout may expose newer capabilities.
- Official plugins guide: https://herdr.dev/docs/plugins/.
- Official CLI reference: https://herdr.dev/docs/cli-reference/.

## Herdr automation

Run `herdr --help`, not `herdr help`. Inspect specific command help before using it.

`src/api/schema.rs` declares agent.list/get/read/start/prompt/wait/send_keys and workspace/tab/pane operations. CLI and API already cover the core requested controls.

- `agent start`: launches a supported interactive agent in an existing pane at a shell prompt; success includes detection and input readiness.
- `agent prompt --wait`: state-transition waiting, explicitly not turn tracking; an already active turn may satisfy completion matching.
- `agent wait`: defaults to idle/done/blocked; supports explicit state and timeout.
- `agent read`: visible, recent, recent-unwrapped, or detection snapshots; text/ANSI and line bounds.
- `pane run`, `pane read`, `pane process-info`, `pane wait-output`: process/log workflows.
- `pane wait-output`: searches the current snapshot immediately, then polls; old matching output can satisfy it.
- No dedicated interrupt command appeared in the inspected help. Sending keys is available; provider-specific interruption needs verification.

Relevant source: src/api/schema/agents.rs, src/api/wait.rs, src/app/api/agents.rs, src/cli/agent.rs, src/integration/env.rs, src/pane.rs.

## Herdr plugin model

Read docs/next/website/src/content/docs/plugins.mdx and the installed-version guide docs/versions/0.9.3/website/src/content/docs/plugins.mdx.

A plugin is a directory with herdr-plugin.toml plus executable commands. It can declare build commands, actions, startup hooks, event hooks, panes, and link handlers. It is out-of-process code with access to the full CLI/socket API, not an embedded SDK or sandbox.

Runtime context includes HERDR_SOCKET_PATH, HERDR_BIN_PATH, HERDR_PLUGIN_CONTEXT_JSON, workspace/tab/pane IDs where available, and dedicated config/state directories. Plugin-owned panes can use tab/split/overlay placement. Popup behavior differs; do not use it as the default persistent service host.

Startup hooks run after restore/socket readiness and are documented as one-shot initialization, not supervised daemons. Automatic service startup needs explicit lifecycle design. Plugin command logs capture bounded stdout/stderr on completion; do not assume live streaming logs from plugin command history. A service pane or plugin-owned log files may be more appropriate.

src/api/schema/events.rs:286 lists supported manifest hooks: workspace/worktree/tab/pane lifecycle plus agent detection and status changes. No pre-agent-launch MCP config mutation hook was found in the inspected plugin schema/runtime. Observe-after-launch hooks do not guarantee configuration before agent startup.

Key implementation files: src/app/api/plugins/{manifest,runtime,panes,env,context}.rs; src/api/schema/plugins.rs; src/cli/plugin.rs; src/plugin_paths.rs.

## T3 Code inspiration

Actual Codex injection is in apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts:1347: per-thread config sets mcp_servers.t3-code with endpoint, timeout, and authorization header. ProviderSessionManager.ts:454-494 handles credential reuse/issuance. Credentials are session/thread scoped; do not put tokens in documentation.

The Codex launch-argument example in docs/orchestration-v2/orchestrator-mcp-server.md is older than the actual adapter path. Prefer source when they disagree.

packages/provider-core/src/server/orchestrationInstructions.ts distinguishes provider-native subagents from app-owned cross-provider delegate_task. The user's phrase "two orchestrators" is ambiguous; do not invent two peer coordinator agents.

T3 exposes orchestration through a shared application service boundary. Borrow its coherent tool vocabulary and provider injection approach without copying its full durable thread/run engine into Herdr.

## Evidence limits

Research inspected code and command help only. No MCP prototype, plugin lifecycle test, provider injection probe, or interruption test has run. T3 nightly installation/channel on the user's machine was not verified.
