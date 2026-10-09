# `lynshen serve` Protocol

`lynshen serve` is a persistent bidirectional protocol mode for GUI/IDE
front-ends. The process reads newline-delimited JSON commands on stdin and
emits the engine's `AgentEvent` stream as newline-delimited JSON on stdout —
the same schema `--headless` uses. It runs until stdin closes, an `op:"shutdown"`
arrives, or a `command` op carries `/quit` or `/exit`.

`lynshen serve --chat` starts a chat session instead: it runs in `~/.lynshen/chats`
with the chat prompt (conversation and web research) and without project
instructions or project skills. Any engine started in that directory, or
inside it, is a chat session as well.

This is the richer of LynShen's two embedding protocols. `lynshen acp` is the
standardized subset for ACP clients; see `docs/serve-vs-acp.md` for the split.

## Framing

- One JSON object per line, both directions. No length prefixes, no batching.
- stdout is flushed after every event line; consumers should read incrementally.
- Empty lines are ignored.
- A line that fails to parse produces `{"type":"error","message":"invalid command: ..."}`
  and the process keeps running. An unknown op produces
  `{"type":"error","message":"unknown op: ..."}`. Malformed input never exits
  the process.

## Protocol version

This document describes protocol version 2. The first line on stdout is

```json
{"type":"hello","protocol":2,"version":"0.3.0"}
```

A client that does not support `protocol` must stop instead of guessing.
Every later event carries a `session` field with the engine's current
session id, the same shape the daemon uses to multiplex several sessions on
one connection (`docs/agent-daemon-plan.md`). Ops may carry a `session`
field; `lynshen serve` hosts exactly one session and ignores it.

## Lifecycle

After `hello`, before any command, the engine emits its startup batch:

```jsonl
{"type":"startup","version":"0.2.0","session_id":"...","profile_dir":"...","config_path":"...","cwd":"...","model":"...","context_window":200000}
{"type":"model_status","provider":"...","model":"...","reasoning_effort":"...","context_window":200000,"context_limit":160000,"max_output_tokens":32000,"reasoning_efforts":["low","medium","high"],"state":"ready"}
{"type":"command_list","commands":[{"command":"/help","marker":null,"args":"","description":"..."}]}
{"type":"approval_mode","mode":"manual"}
{"type":"mcp_servers","servers":[...]}
{"type":"task_board","tasks":[...]}                          // only when the session's task board has tasks
{"type":"trust_prompt","cwd":"...","repo_root":"..."}        // only when an untrusted project has local resources
{"type":"info","message":"..."}                              // session_start hook output, if any
```

`startup.version` is the CLI version; front-ends should record it for
compatibility checks.

After startup the loop polls every ~30 ms. `model_status` is deduplicated:
it is re-emitted only when its content changes. All other events are emitted
as they occur — command responses first, then background worker events
(streaming deltas, tool progress, MCP state changes, update notices) as they
arrive.

The process exits with code 0 on `shutdown`, stdin EOF, or `/quit` / `/exit`
sent through the `command` op.

## Commands (stdin)

Every command is a JSON object with an `op` field.

### `user_message`

```json
{"op":"user_message","content":"refactor the parser","images":["/abs/path.png"]}
```

Submits a user turn. `images` is an optional array of local image paths;
unattachable paths produce `info` events and are skipped.

If a turn is already running, the message is queued instead: the engine emits
`pending_messages` (the full queue) and `status:"queued: N"`. Queued messages
start automatically when the current turn ends — no client action needed.

### `command`

```json
{"op":"command","input":"/model gpt-5.5 medium"}
```

Runs any slash command exactly as the TUI would (`/model`, `/resume`,
`/compact`, `/approve`, `/mcp`, custom commands, MCP prompt commands, ...).
Structured views come back as events (`model_view`, `resume_view`,
`tree_view`, ...). `/quit` and `/exit` terminate the process.

### `steer`

```json
{"op":"steer"}
```

Sends the next queued message into the in-flight turn: the model reads it
before its next request (once the current tool calls finish), and running
tools and subagents keep going. Emits `status:"steering"` and the updated
`pending_messages`; when the model reads it, a `user_message` event (it is
saved as the user's message). If the turn ends before reading it, it runs as
the next turn. A queued message with images still stops the turn and starts
a new one with it. No-op when idle or when the queue is empty.

### `interrupt`

```json
{"op":"interrupt"}
```

Stops the in-flight turn without touching the queue: aborts the worker,
closes subagents, clears pending approvals. Emits
`info:"request interrupted"` and `status:"interrupted"`. Queued messages still
auto-start as the next turn; send `interrupt` again (or avoid queueing) to
stop those too. No-op when idle.

### `approve`

```json
{"op":"approve","call_id":"call_9","decision":"allow","always":false,"hunks":["f0h1"]}
```

Structured answer to an `approval_request` event; equivalent to the
`/approve` slash command.

- `call_id` (required): the `call_id` from the request.
- `decision` (required): `"allow"` or `"deny"`.
- `always` (optional, default `false`): on allow, add the tool to the
  per-session allowlist so it stops asking.
- `hunks` (optional): for edit-tool requests that carried a `hunks` list,
  apply only these hunk ids. Omit or `null` for the whole call. Cannot be
  combined with `always`.

Validation errors emit `error` and leave the request pending, so the client
can retry with a corrected op.

### `set_approval_mode`

```json
{"op":"set_approval_mode","mode":"manual"}
```

`mode` is `manual`, `plan`, `auto-edit`, `auto`, or `full-access`. Emits
`status:"approval mode: ..."` and an `approval_mode` event. The change
applies to new turns; an in-flight turn's gating can only loosen.

`plan` is plan mode: only read-only tools run (reads, listing, search,
web search/fetch, read-only shell commands, MCP tools marked
`readOnlyHint`, subagents, which inherit the mode). Every other call
returns an error telling the model to plan instead. The model delivers
its plan with the `propose_plan` tool, which emits `proposed_plan` and
ends the turn.

### `agent_runs` / `subagent_transcript`

```json
{"op":"agent_runs"}
{"op":"subagent_transcript","agent_id":"/root/lister"}
```

The agent trace. `agent_runs` answers with the `agent_runs` event (also
sent on its own while subagents work, at most twice a second, and at once
on a lifecycle change). `subagent_transcript` answers with one agent's
work: `{"type":"subagent_transcript","agent_id","items":[...]}`, items
`{"role":"user","content"}` (the task first, then messages the parent
sent), `{"role":"assistant","content"}`, `{"role":"reasoning","content"}`
and `{"role":"tool","call_id","name","input","output","running","is_error"}`.
Each agent keeps at most 300 items and 256 KB of text (older steps are
replaced by one "… earlier steps trimmed" item); tool input and output are
cut to 4 KB (output keeps its end). An unknown id answers with `"error"`
instead of `items`. Agents of earlier turns (the 24 most recent) stay
listed and readable.

### `merge_agent`

```json
{"op":"merge_agent","target":"/root/worker_a","action":"apply"}
{"op":"merge_agent","target":"worker_a","action":"discard"}
```

The merge and discard buttons of a worktree subagent: one started with
`isolation: "worktree"`, as the `worker` role is. `target` is the agent's
path, or its task name for a subagent of the main agent. The agent must
have finished (any state except `pending` and `running`).

- `apply` takes everything the agent changed in its worktree since it
  started (edited, deleted and new files, committed or not) and applies it to
  the directory the agent was started from. A clean `git apply` is tried
  first. When those files changed there in the meantime, each file is merged
  three-way (`git merge-file`). If any file conflicts, nothing is written, the
  worktree stays and the agent's state becomes `conflict`; apply again after
  the conflicting files are fixed, or discard. On success the worktree is
  deleted and the state becomes `merged`.
- `discard` deletes the worktree. The state becomes `discarded`.

The op asks for no approval: the click is the user's decision. The model's
own `merge_agent` tool asks for approval of `apply` in `manual` mode, like a
file edit; the request's `summary` names the files and line counts
(`merge /root/w: 3 files (+40 -5): a.rs, b.rs, c.rs`). `discard` never asks.

A worktree that is not merged when its turn ends stays on disk. A later turn
or this op can still merge it while the engine runs. Worktrees in
`<cwd>/.lynshen/agents/` older than `agents.keep_worktrees_days` are deleted
when an engine opens the directory.

Emits `merge_result`; on success or conflict also `subagent_lifecycle` with
the new state; then `agent_runs`. An unknown target, an agent that is still
running or one without a worktree emits `merge_result` with `ok: false` and
`error`.

The worktree registry is saved with the session: after an engine or daemon
restart (the session opened again with `/resume` or `session_open`), the
worktree agents come back as `agent_runs` rows and this op still merges them.
A worktree whose directory is gone is dropped then; an agent that was still
running when the engine stopped comes back `interrupted`.

### `close_agent`

```json
{"op":"close_agent","target":"/root/scout"}
```

The stop button of a subagent: stops it, foreground or background, and the
foreground agents it started (the same as the model's `close_agent` tool).
`target` is the agent's path, or its task name for a subagent of the main
agent. Emits `subagent_lifecycle` (`closed`) and `agent_runs`; an unknown
target emits `error`. A closed background agent never starts a main turn.
While `agents.team_v2` is off the op emits `error` and stops nothing (see
"Agent team" → "Team v2 switch").

### `pick_attempt`

```json
{"op":"pick_attempt","group":"fix","target":"fix_a2"}
```

Keeps one attempt of a best-of-N group (see "Agent team" → "Best of N"):
`target` is the attempt's task name, path or number (`"2"`). Attempts still
running are closed first; the picked one must have finished. It is applied
as `merge_agent` `apply` does, then every other attempt is discarded. Emits
one `merge_result` per agent (the apply first), their `subagent_lifecycle`
events and `agent_runs`. When the apply conflicts nothing is written and the
other attempts are kept (one `merge_result`). An unknown group or target
emits `error`. Like `merge_agent`, the op asks for no approval; the model's
`pick_attempt` tool asks in `manual` mode. While `agents.team_v2` is off the
op emits `error` and changes nothing.

### `approve_plan`

```json
{"op":"approve_plan","id":"plan-1a2b3c4d5e6f7a8b","decision":"approve","mode":"auto-edit"}
{"op":"approve_plan","id":"plan-1a2b3c4d5e6f7a8b","decision":"revise","feedback":"Also add a test"}
```

`approve` re-emits the plan with `status:"approved"`, switches the
approval mode to `mode` (default `auto-edit`; `plan` is not allowed) and
starts a turn that implements the plan (optional `feedback` is passed
along as notes). `revise` re-emits it with `status:"revising"`, keeps
plan mode, and starts a turn asking for a complete revised plan with the
`feedback`, which is required. An unknown or already approved `id`
returns an `error` event.

### `set_attended`

```json
{"op":"set_attended","attended":false}
```

Marks whether a client is watching the session (default `true`). While
unattended, a tool call that would emit `approval_request` is recorded as a
deferred action instead: the engine emits `action_deferred`, the model gets a
"submitted for confirmation" result, and the turn continues without waiting.
Switching to `false` also converts calls already waiting on an
`approval_request` into deferred actions. Emits `attended`.

An identical call (same tool, arguments and working directory) reuses the
open deferred action, or the decision already made for it in this engine.

### `decide_action`

```json
{"op":"decide_action","action":"act-1727500000000-3f9a1c2b","decision":"allow"}
```

Decides the deferred action whose id is `action`. `allow` runs the call with its original arguments
in the background; `deny` does not run it. Either way the engine emits
`action_decided` and sends the outcome to the session as a user message,
which starts a turn (or queues behind the running one). Unknown ids emit
`error`.

### MCP ops

`mcp_list`, `mcp_set`, `mcp_remove`, `mcp_toggle` manage configured MCP
servers and emit the `mcp_servers` view. See `docs/mcp.md` → "Serve protocol
ops" for the full shapes.

### `shutdown`

```json
{"op":"shutdown"}
```

Exits the process with code 0. Closing stdin has the same effect.

## Events (stdout)

Every line is `{"type": <name>, ...}`. All types emitted by the engine:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `startup` | `version`, `session_id`, `profile_dir`, `config_path`, `cwd`, `model`, `context_window` | First event; identifies the session. |
| `model_status` | `provider`, `model`, `reasoning_effort`, `context_window`, `context_limit`, `max_output_tokens`, `reasoning_efforts`, `state` | Current model selection; deduplicated, re-emitted on change. Claude Code sessions add their own fields (`docs/daemon-protocol.md` → Other engines). |
| `command_list` | `commands: [{command, marker, args, description}]` | Available slash commands incl. custom and MCP prompt commands. |
| `approval_mode` | `mode` | Current approval mode; emitted at startup and on change. |
| `agent_runs` | `workflows: []`, `agents: [{id, label, model, effort, state, started_at, duration_ms, tokens, tool_calls, prompt, result, error, type: "subagent", tool_use_id, activity, role, plan_step, isolation, workdir, files_changed, background, attempt_group, attempt}]` | Every subagent of the session, oldest first (the 24 most recent finished ones, and every one with a worktree to merge). `id` is its path (`/root/<task_name>`), `state` ∈ `pending`/`running`/`completed`/`errored`/`interrupted`/`closed`/`budget_exhausted`/`conflict`/`merged`/`discarded`, `started_at` epoch ms (reset by `resume_agent`), `tool_use_id` the parent's `spawn_agent` call (empty for a reviewer started by `agents.review_on_complete`), `activity` its latest action in a few words ("Running cargo test"). `role` and `plan_step` are strings or `null`. `isolation` is `worktree` or `none`; a worktree agent has `workdir` (its worktree, else `null`) and `files_changed` (paths relative to the worktree, once finished). `background`: it keeps running after its parent's turn ends. `attempt_group` (the task name given to `spawn_agent`) and `attempt` (1–4) are set for best-of-N attempts, else `null`. See "Agent team". |
| `plan_draft` | `id`, `title`, `append` | Plan mode: the plan while the model writes its `propose_plan` call. `append` is the Markdown added since the previous `plan_draft` of the same `id`; `title` is the title so far. The finished plan follows as `proposed_plan` with the same `id`. Not replayed. |
| `proposed_plan` | `id`, `title`, `markdown`, `status` | Plan mode: a plan from `propose_plan`. `status` is `pending`, then `approved` or `revising` after `approve_plan`. Session replay (`transcript`) carries it as `{"role":"plan","id","title","content","status"}` with the latest status. |
| `mcp_servers` | `servers: [{name, transport, state, tools, error?}]` | MCP server states; `state` ∈ `connecting`/`connected`/`failed`/`disabled`. |
| `trust_prompt` | `cwd`, `repo_root` | Project has local resources (skills, commands, hooks) and no stored trust decision; answer via `command` `/trust yes\|no\|repo`. |
| `user_message` | `content` | A user turn was accepted and started (echoes the submitted text). |
| `pending_messages` | `messages` | The queued-message list after it changed. |
| `fill_input` | `content` | Front-end should pre-fill its input box (e.g. after `/checkout`). |
| `connecting` | — | A model request is starting. |
| `thinking_start` | — | Reasoning output begins. |
| `reasoning_delta` | `delta` | Reasoning text chunk. |
| `assistant_start` | — | Assistant reply begins. |
| `assistant_delta` | `delta` | Reply text chunk. |
| `retrying` | `attempt` | The request is being retried after a transient failure. |
| `tool_start` | `call_id`, `name` | Tool call begins. |
| `tool_update` | `call_id`, `name`, `output` | Intermediate tool progress (e.g. long-running bash). |
| `tool_output` | `call_id`, `name`, `output`, `is_error` | Tool call finished. |
| `approval_request` | `call_id`, `name`, `summary`, `subagent_id`, `hunks` | A gated tool call waits for an `approve` op. `hunks` is a list of `{id, file, header, lines}` for partial approval, `null` otherwise. `subagent_id` is set when a subagent issued the call. |
| `action_deferred` | `id`, `session_id`, `cwd`, `call_id`, `name`, `arguments`, `summary`, `subagent_id`, `digest`, `created_at` | An unattended session recorded a gated call instead of prompting; decide it with `decide_action`. |
| `action_decided` | `id`, `decision`, `output`, `is_error` | A deferred action was decided; `output` is the tool result when it ran, `null` when declined. |
| `attended` | `attended` | Current attended state, after `set_attended`. |
| `subagent_lifecycle` | `path`, `status`, `message`, `label`, `model`, `tool_use_id`, `role`, `plan_step`, `background`, `attempt_group`, `attempt` | Subagent spawn/progress/finish notices. `status` is a state of `agent_runs` or `message` (a message was queued for it); `pending` with message `resumed` when `resume_agent` runs it again. `role`, `plan_step` and `attempt_group` are strings or `null`, `attempt` a number or `null`, `background` a boolean (as in `agent_runs`). |
| `agent_message` | `from`, `to`, `summary` | An agent sent another one a message with `send_message` (parent, child or sibling). `from` and `to` are agent paths (`/root` is the main agent); `summary` is the message, cut to 200 characters. Also sent when a background agent's result reaches the main agent (`from` the agent, `to` `/root`, `summary` the start of its result) and when a `task_completed` or `agent_idle` hook reports (`from` `hook:task_completed` or `hook:agent_idle`). |
| `task_board` | `tasks: [{id, title, detail, status, owner, depends_on, role, files, result, updated_at}]` | The session's shared task board, whole, after every change. `id` is `t1`, `t2`, …; `status` ∈ `pending`/`claimed`/`completed`/`failed`/`blocked`; `owner` the claiming agent's path or `null`; `depends_on` task ids; `role` and `result` strings or `null`; `detail` a string (empty when none); `files` paths; `updated_at` epoch ms. Part of the startup batch and of a resumed session's events when the board has tasks. See "Agent team" → "Task board". |
| `merge_result` | `target`, `action`, `ok`, `files`, `conflicts`, `error` | `merge_agent` ran, from the model or the op. `files`: what the worktree changes. `ok: true`: applied (`apply`) or deleted (`discard`). `ok: false` with `conflicts`: nothing was written. `ok: false` with `error` (a string, else `null`): the merge could not run. |
| `team_budget` | `used`, `limit` | Input + output tokens the subagents used in the current budget window, against `agents.turn_token_budget`. A window opens with each main turn except one the engine starts for a background result, which continues it. Sent at the first spawn of a window and each time `used` reaches another tenth of `limit`; never without a budget. |
| `usage` | `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens` | Real API usage for the completed turn. |
| `context_usage` | `tokens`, `tokenizer`, `cost` | Tokenizer-counted context size; `cost` is cumulative USD (0 when unpriced). |
| `compaction_start` / `compaction_end` | — | Context compaction began/finished. |
| `compaction_progress` | `output_tokens` | Compaction summary tokens produced so far. |
| `compaction_failed` | `error` | Compaction failed; the session continues uncompacted. |
| `model_view` | `models: [{model, active, context_window, max_output_tokens, reasoning_efforts}]`, `active_effort` | Model picker data (`/model`). |
| `tree_view` | `nodes: [{id, parent_id, label, active}]` | Session branch tree (`/tree`). |
| `resume_view` | `items: [{id, label, active}]` | Session picker data (`/resume`). |
| `checkpoint_view` | `items: [{id, label, detail}]` | Checkpoint picker data. |
| `goal` | `goal: {objective, status, token_budget, tokens_used, time_used_seconds, created_at, updated_at} \| null` | Goal state; `null` clears it. |
| `plan` | `plan: [{step, status, agent, files}]` | Goal/plan step list. `agent` (string or `null`) is the subagent doing the step, as the model named it (path or task name); `files` the files it is expected to write. |
| `transcript` | `items: [{role, ...}]` | Rendered conversation (`/transcript`); roles: `user`, `assistant`, `tool` (`name`, `output`), `branch` (`label`). |
| `info` | `message` | Informational line (hook output, notices, update available). |
| `error` | `message` | Error line. Also used for malformed commands and unknown ops. |
| `status` | `message` | Turn status string: `ready`, `streaming`, `queued: N`, `steering`, `interrupted`, `compacting`, `approval mode: ...`, `trusted ...`, etc. `ready` marks the end of a turn. |

## Agent team

The main agent can start subagents (`spawn_agent`), wait for them, message
them, and merge the work of those that write in a worktree.

### Settings

`agents` in `~/.lynshen/config.json`. Every key is optional; a config
without the object gets the defaults. Changes apply from the next turn.

```json
{"agents": {"max_live": 4, "max_depth": 2, "turn_token_budget": 0, "fanout": "auto", "keep_worktrees_days": 7, "wake_on_result": true, "review_on_complete": false, "team_v2": true}}
```

| Key | Default | Meaning |
| --- | --- | --- |
| `max_live` | `4` | Subagents pending or running at the same time. |
| `max_depth` | `2` | Levels of nesting. `1`: only the main agent starts subagents. |
| `turn_token_budget` | `0` | Input + output tokens all subagents of one main turn may use together. `0`: no limit. At 80% every running subagent, and the main agent, gets a message to wrap up. At 100% `spawn_agent` refuses, and each running subagent stops before its next model request with state `budget_exhausted`; its output so far is kept. |
| `fanout` | `auto` | `off`: no subagent tools. `plan`: an agent that can write (one without a read-only role) starts only when the latest proposed plan was approved, with `plan_step` naming a step of the `update_plan` checklist (its text or its number from 1); `explorer` and `reviewer` start at any time. `auto`: the model decides. |
| `keep_worktrees_days` | `7` | Subagent worktrees older than this are deleted when an engine opens the project. `0` keeps them. |
| `wake_on_result` | `true` | When a background subagent of the main agent finishes (or a `task_completed` / `agent_idle` hook reports) while the main agent is idle, the engine starts a main turn by itself on that result. Not while the team budget is used up; the result then waits for the next turn. |
| `review_on_complete` | `false` | A board task completed with `role: "worker"` (or owned by a `worker` agent) gets a background `reviewer` subagent of the main agent, `review_<task id>`, on the owner's diff. It counts toward the budget and `max_live`; when it cannot start, the main agent gets a message saying why. |
| `team_v2` | `true` | Agent team v2 (Beta): the task board, background agents, `resume_agent`, best-of-N attempts, wake turns and the `task_completed` / `agent_idle` hooks. `false` turns all of it off and the team works as v1 did; see "Team v2 switch". |

### Team v2 switch

`agents.team_v2: false` (LynShen Desktop writes it from its own switch and
the product owner's remote switch) gives the v1 team:

- Not offered: `task_create`, `task_list`, `task_update`, `resume_agent`,
  `pick_attempt`; `spawn_agent` has no `background` and no `attempts`; the
  `send_message` and `wait_agent` descriptions do not name siblings or
  attempts. The first request is as small as before v2.
- A call of one of these tools anyway returns the error `<tool> is not
  available: agent team v2 (Beta) is switched off (agents.team_v2 is false)`
  and asks for no approval. `spawn_agent` with `background: true` or
  `attempts` above 1 returns the same error for `spawn_agent with
  background` / `spawn_agent with attempts` and starts nothing;
  `background: false` and `attempts: 1` are accepted (they ask for what v1
  does).
- No wake turns, no `task_completed` or `agent_idle` hooks, no reviewer from
  `review_on_complete`.
- The `close_agent` and `pick_attempt` ops emit that error (`close_agent:
  …`, `pick_attempt: …`).
- Everything of v1 works as before: roles, worktrees and `merge_agent`,
  messages to the parent, the `agents.*` limits, budget and fanout, plan
  owners, `agent_runs` and transcripts. Events already sent (`task_board`,
  `background`, `attempt_group`) are not taken back; a saved board stays in
  the session and comes back when v2 is on again.

A turn uses the value it started with, like the other keys. What happens
between turns reads config.json again: a wake, the hooks of an agent that
finishes, the review of a completed task and the two ops. So switching v2 off
while background agents run lets them run to the end, but none of them starts
a main turn or runs a hook: each result waits in the main agent's mail, and
the next turn (the user's) reads it before its first model request. Agents
still running stay listed and the model's `close_agent` tool can stop them.
Switching v2 on again applies from the next turn; a result that already
waited is still delivered by that turn, not by a wake.

### Roles

`spawn_agent` takes an optional `role`. Built-in roles:

| Role | Access | Isolation | Use |
| --- | --- | --- | --- |
| `explorer` | read-only | none | Research: finds code, answers questions. |
| `worker` | inherit | worktree | Writes code in its own git worktree; merged with `merge_agent`. |
| `reviewer` | read-only | none | Reviews a diff with a fresh context and reports problems. It never edits. |

A file `~/.lynshen/roles/<name>.md` (user) or `<cwd>/.lynshen/roles/<name>.md`
(project) replaces the built-in role of the same name or adds a role. Project
roles load only in a trusted project, and a `.lynshen/roles` directory makes
the engine ask for trust. The project file wins over the user file.

```markdown
---
name: tester
description: runs the tests and reports failures
model: gpt-5.4-mini
reasoning_effort: low
access: read-only
isolation: none
max_tool_calls: 40
timeout_secs: 900
---
Run the tests your task names. Report each failure with its output.
```

Every key is optional; `name` defaults to the file name. `access` is
`read-only` or `inherit` (default). `isolation` is `none` (default) or
`worktree`. `model` must be one the subagent may use (`subagent_models`).
The text after the frontmatter is added to the subagent's system prompt.
The `spawn_agent` call's own parameters override the role's values.

A read-only role runs under the rules of plan mode: only read-only tools and
read-only shell commands run, and every other call returns an error. The
subagents of a read-only agent are read-only too.

### Messages

A subagent calls `send_message` with `target: "parent"` to write to the agent
that started it. `target` may also be a sibling: its task name (a name is
looked up among the sender's own subagents first, then among its siblings)
or its path; `/root` is the main agent. The recipient reads the message
before its next model request. A parent that waits in `wait_agent` returns at
once, with the messages in the result: `"messages": [{"from":
"/root/worker_a", "message": "..."}]` (a background result adds `"kind":
"result"` and `"status"`, a hook's output `"kind": "hook"` and `"ok"`). Each
message, in any direction, emits `agent_message`.

### Task board

One board per session, shared by the main agent and every subagent, saved
with the session (it survives an engine or daemon restart) and sent whole as
`task_board` after each change.

| Tool | Arguments | Result |
| --- | --- | --- |
| `task_create` | `title`, optional `detail`, `depends_on` (task ids), `role`, `files` | `{"id": "t1"}` |
| `task_list` | — | `{"tasks": [...]}` (the `task_board` rows) |
| `task_update` | `id`, `action` ∈ `claim`/`release`/`complete`/`fail`/`block`, optional `result`, `note` | the task |

- `claim` takes a `pending` task whose `depends_on` tasks are all
  `completed`; claims are atomic (one owner).
- Only the owner, or the main agent, may `release` (back to `pending`, from
  `claimed` or `blocked`), `complete`, `fail` or `block` a task. The main
  agent may also complete, fail or block a task nobody claimed.
- `fail` blocks every `pending` task that depends on it, directly or not; a
  new task on a failed one starts `blocked`.
- `result` (or `note`) is kept on the task: the outcome, or why it failed or
  is blocked.
- Completing a task runs the `task_completed` hooks; with
  `agents.review_on_complete`, a worker's task gets a reviewer (the
  `task_update` result then names it in `review`).

The board tools are coordination: read-only roles and plan mode may use them.

### Background agents

`spawn_agent` with `background: true` starts an agent that keeps running
when the turn of the agent that started it ends. Without it (the default), a
turn that ends, fails or is interrupted closes its subagents. An interrupt
closes foreground agents only; so does the end of a subagent's own run (its
foreground subagents close with it). Closing an engine, switching sessions
and the `close_agent` op or tool stop background agents too.

When a background subagent of the main agent finishes, its result goes to
the main agent as `<subagent_result path="/root/x" status="completed">…</subagent_result>`:
read before its next request while it works, handed over by `wait_agent`,
or, while it is idle and `agents.wake_on_result` is on, as the input of a
main turn the engine starts by itself (one per batch of results; each
result is delivered once, and an `agent_message` from the agent to `/root`
is emitted). That turn has no `user_message` event, and its input is left
out of the transcript replay like other runtime messages; the reply is in it.
A closed or interrupted agent wakes nobody.

### Resuming an agent

`resume_agent` (`target`, `message`) runs a finished subagent (any state
but `pending` and `running`) again on its own conversation plus `message`,
with the model, effort, role, access and limits it was started with, in its
worktree when that still exists (a new one when it was merged or discarded),
in the background if it was started so. Agents restored after an engine
restart have no conversation and cannot be resumed.

### Best of N

`spawn_agent` with `attempts` (1–4; above 1 only with worktree isolation:
`isolation: "worktree"` or the `worker` role, otherwise refused) starts that
many agents on the same message, `<task_name>_a1` … `<task_name>_aN`, each in
its own worktree, with `attempt_group: "<task_name>"` and `attempt: k`. They
are reserved together: when `max_live` leaves too few slots, none starts.
`wait_agent` with `targets: ["<task_name>"]` returns once every attempt has
finished, with `"attempts": {"<task_name>": [{path, attempt, state, summary,
files, added, removed}]}`, the diff of each against its base, to compare
them. The `pick_attempt` tool (`group`, `target`) or op keeps one.

### Hooks

`task_completed` and `agent_idle` join the other hooks in `hooks.json`
(`~/.lynshen/hooks.json`, and `<cwd>/.lynshen/hooks.json` in a trusted
project), with the same entry shape:

```json
{"task_completed": [{"command": "cargo test 2>&1 | tail -20"}],
 "agent_idle": [{"command": "./scripts/check.sh"}]}
```

- `task_completed` runs, on its own thread, when a board task is completed,
  in the owner's working directory (its worktree; the main agent's directory
  for an unowned task). stdin: `{"event": "task_completed", "task": {…},
  "workdir", "cwd"}`; environment: `LYNSHEN_HOOK_EVENT`,
  `LYNSHEN_HOOK_WORKDIR`, `LYNSHEN_TASK_ID`, `LYNSHEN_TASK` (the task JSON).
- `agent_idle` runs when a subagent's run ends (completed, errored or out of
  budget; not when it was closed), in its working directory, before the
  agent is marked finished. stdin: `{"event": "agent_idle", "agent": {path,
  status, error, summary, workdir, files_changed, role, background},
  "workdir", "cwd"}`; environment: `LYNSHEN_HOOK_EVENT`,
  `LYNSHEN_HOOK_WORKDIR`, `LYNSHEN_AGENT` (its path).

A hook's stdout, trimmed and cut to 4000 characters, goes to the main agent
as `<hook_result hook="task_completed" ok="true">…</hook_result>` and an
`agent_message` from `hook:<event>`; a hook that exits 0 with no output says
nothing. A non-zero exit sends `ok="false"` with its stderr (else stdout, else
the exit code). Hook output wakes an idle main agent like a background
result.

### Worktree confinement

A subagent with a worktree writes only there: its file tools, and its shell,
which starts in the worktree and, in the sandbox, can write only the
worktree, temp and package caches (`docs/sandbox.md` → "Worktree
subagents"). Leaving the sandbox always asks a person unless an `allow`
command rule matches.

### Plan owners

`update_plan` steps take optional `agent` (the subagent's path or task name)
and `files` (the files the step writes). The `plan` event passes them
through. `subagent_lifecycle` carries `plan_step`: the step given to
`spawn_agent`, else the step whose `agent` names the subagent.

## Turn sequence

A normal turn looks like:

```jsonl
{"op":"user_message","content":"fix the test"}
< {"type":"user_message","content":"fix the test"}
< {"type":"connecting"}
< {"type":"thinking_start"}            // reasoning models only
< {"type":"reasoning_delta","delta":"..."}
< {"type":"assistant_start"}
< {"type":"assistant_delta","delta":"..."}
< {"type":"tool_start","call_id":"call_1","name":"bash"}
< {"type":"tool_output","call_id":"call_1","name":"bash","output":"...","is_error":false}
< {"type":"usage","input_tokens":1234,"cached_input_tokens":800,"output_tokens":56,"reasoning_tokens":12}
< {"type":"context_usage","tokens":5678,"tokenizer":"o200k","cost":0.0123}
< {"type":"status","message":"ready"}
```

Approval round-trip:

```jsonl
< {"type":"approval_request","call_id":"call_2","name":"bash","summary":"rm -rf build","subagent_id":null,"hunks":null}
> {"op":"approve","call_id":"call_2","decision":"deny"}
```

Interrupt:

```jsonl
> {"op":"interrupt"}
< {"type":"info","message":"request interrupted"}
< {"type":"status","message":"interrupted"}
```

## Mapping to UI

Suggested rendering (as used by LynShen Desktop):

| Events | UI |
| --- | --- |
| `assistant_start` / `assistant_delta` | streaming message bubble |
| `thinking_start` / `reasoning_delta` | collapsible thinking section |
| `tool_start` / `tool_update` / `tool_output` | tool cards keyed by `call_id` |
| `approval_request` | approval dialog → `approve` op |
| `model_view` / `tree_view` / `resume_view` / `checkpoint_view` | pickers/sidebars |
| `goal` / `context_usage` / `usage` | status bar |
| `agent_runs` / `subagent_lifecycle` / `agent_message` / `team_budget` | agent cards and the team summary |
| `task_board` | the team's task board |
| `merge_result` | result of the merge / discard / pick buttons (`merge_agent`, `pick_attempt` ops) |
| `compaction_*` | compaction progress |
| `status` / `info` / `error` | status line / toasts |
| `pending_messages` | queued-message indicator |
| `fill_input` | pre-fill the input box |
