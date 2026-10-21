# Benchmarks

## Bench 1: herdr skill vs herdr-mcp

Three tasks (classify panes and read the running process's last line; run a server and stop it on ready; launch a Codex child, read its reply, close its tab), 2 reps each, per arm.

| arm | correct | turns | tool calls | duration | cost | cache-create tok | cache-read tok |
|---|---|---|---|---|---|---|---|
| herdr-mcp | 4/6 | 4.2 | 3.2 | 11.1 s | $0.030 | 5.4k | 44k |
| herdr skill (Bash + CLI) | 6/6 | 7.8 | 5.8 | 17.6 s | $0.099 | 19.8k | 128k |

Failure modes:
- MCP, task 3 (both reps): the parent stopped at Codex's hook-trust dialog and never got PONG. The MCP rule is to surface blocked prompts, not answer them. The skill arm answered the dialog itself.
- Skill, tasks 2 and 3: a hidden "unknown command" error from a CLI call, then a retry. This cost turns, not correctness.
- Fixed context: SKILL.md is 9.0 KB; MCP instructions plus tool list are 16.6 KB.

Verdict: MCP is about 3x cheaper and 1.6x faster at equal task shape. The two failures are by design (the dialog is not answered), not tool errors. The skill is fine for ad-hoc work but pays more per task.

## Bench 2 and 3: delegation reply channel

Task D1: the parent launches a Codex child that returns the 8 largest files under `src` as `[{path, size}]`, sorted descending. The parent's final message is only that JSON, and it closes the child's tab. Ground truth is recomputed from `src` at the run's commit.

Arms, 3 reps each:
- **screen**: `herdr_launch {wait: "turn"}`; the parent reads the answer from the launch `excerpt`. Mail tools are disallowed.
- **mail**: the parent writes the mail-back instruction itself, then calls `herdr_mail_wait` and `herdr_mail_read`.
- **launch-mail**: `herdr_launch {reply: "mail", wait: "turn"}` with the task only; the answer is `reply.body`. The prompt does not mention mail-wait or read.

Averages over 3 runs (bench 3, commit ca397f4):

| arm | exact | entries correct | turns | tool calls | duration | cost | output tok | cache-read tok | tool-result bytes |
|---|---|---|---|---|---|---|---|---|---|
| screen | 3/3 | 8/8 | 4 | 3 | 26.5 s | $0.051 | 513 | 55k | 4090 |
| mail | 3/3 | 8/8 | 6 | 5 | 32.8 s | $0.039 | 638 | 86k | 3021 |
| launch-mail | 3/3 | 8/8 | 4 | 3 | 35.0 s | $0.035 | 518 | 59k | 2978 |

Bench 2 (commit 3617464, screen and mail only) gave the same picture: 6/6 exact, screen 4 turns and 4073 B, mail 6 turns and 3024 B.

Payload: the launch result is 2.2 KB on screen (a ~1.5 KB TUI excerpt, with the JSON hard-wrapped at about 130 columns and mixed with chrome) and 1.1 KB on launch-mail (a clean one-line body). Each run also carries a shared 1.8 KB `herdr_capabilities` result. Without it, per-run result bytes are screen 2.3 KB, mail 1.2 KB, launch-mail 1.2 KB.

Verdict: all three channels were exact. `reply: "mail"` keeps the mail path's clean payload and drops its two extra tool calls and turns, so it matches screen on turns and calls at about 27% fewer result bytes. It was the cheapest arm by about $0.004 to $0.016, but that gap is inside the noise (see caveats). Wall time was longest, since the child does a mail round trip before the turn settles. Screen's cost average is lifted by one cold-cache run (rep 1, $0.079; the other two were $0.036). Prefer `reply: "mail"` when the result must be parsed, since the screen excerpt is wrapped and noisy. Screen is fine when a person reads it.

## Method

- Parents: headless `claude -p --model sonnet` runs, each inside its own pane of a scratch Herdr session (`herdr-mcp-probe`), with `--strict-mcp-config`, `--tools ""` and an explicit herdr MCP allow list (screen: no mail tools; mail and launch-mail: all herdr tools).
- Children: Codex agents launched by the parent. Per-run metrics come from the stream-json `result` event and tool-result sizes; fidelity is an exact match of the final message to the ground truth, plus a count of matching entries by position.
- Sample: bench 1 has 2 reps per task and arm; bench 2 and 3 have 3 reps per arm, 2 runs at a time.
- herdr-mcp: bench 1 on an earlier build, bench 2 at 3617464, bench 3 at ca397f4 (release build swapped in as the `herdr-mcp` shim, restored after).
- Versions: Herdr 0.9.3, Codex 0.163.0, Claude Code 2.1.306.
- Harness: shell and Python scripts kept outside the repo in a scratch directory; raw stream-json output is not published.

## Caveats

- n is 2 per cell in bench 1 and 3 in bench 2 and 3. Cost and duration differences under about 20% are noise.
- Parents ran with `bypassPermissions` and restricted tool lists, so approval behavior in real sessions is not measured.
- One task kind (a small, deterministic file listing). Long or noisy child output, failing children and larger payloads are untested.
- Idle and success are not the same: fidelity here is checked against ground truth, not inferred from a quiet pane.
