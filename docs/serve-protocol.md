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
{"type":"model_status","provider":"...","model":"...","reasoning_effort":"...","context_window":200000,"context_limit":160000,"max_output_tokens":32000,"reasoning_efforts":["low","medium","high"],"input_modalities":["text","image"],"state":"ready"}
{"type":"command_list","commands":[{"command":"/help","marker":null,"args":"","description":"..."}]}
{"type":"approval_mode","mode":"manual"}
{"type":"sandbox","mode":"workspace-write","network":true,"enforced":true,"unavailable":null,"directories":[]}
{"type":"reply_style","style":"default","name":"默认","source":"builtin","chosen":false,"missing":false}
{"type":"mcp_servers","servers":[...]}
{"type":"task_board","tasks":[...]}                          // only when the session's task board has tasks
{"type":"trust_prompt","cwd":"...","repo_root":"..."}        // only when an untrusted project has local resources
{"type":"info","message":"..."}                              // session_start hook output, if any
```

`startup.version` is the CLI version; front-ends should record it for
compatibility checks.

The model's commands run in the shell `shell` in config.json names, read
again each turn (see "Shell" in `docs/sandbox.md`). There is no op for it
here: a client changes config.json, and the daemon's `shells` op lists the
shells this computer has.

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

### Rewind

`/rewind` answers with `checkpoint_view` (the user turns of the active
branch). `/rewind <id>` cuts the conversation back to before that turn,
restores files from `.lynshen/checkpoints`, and answers with `transcript` and
an `info` line:

```text
rewound · restored 2 file(s), removed 1 · left 1 file(s) as they are: src/lib.rs (changed since this session last edited it (by another session, the user, or a command))
```

The part after the second `·` is there only when files were left alone.
Several sessions often share one project, so a rewind undoes only this
session's edits:

- Every checkpoint records the session that took it (`session_id`) and, per
  file, a hash of what the edit left there (`after`). The checkpoint is
  written before the edit, with the content before it, so a crash during the
  edit loses nothing; `after` is added once the edit has run. When
  `post_tool_use` hooks run for that tool call, what they leave counts as
  part of the edit (a formatter's rewrite), but only in a file that still
  held what the edit left when they started: a file the user saved
  meanwhile, during a later command of the same reply say, stays a later
  change.
- A merge of a subagent's worktree (`merge_agent` apply, `pick_attempt`) is
  checkpointed as an edit of the session that merges.
- Only files that this session's checkpoints at or after the turn captured
  are restored, each to its content before the session's first edit in that
  range (or deleted when it did not exist then). Files that only other
  sessions edited are not touched. A rename made by `apply_patch` puts the
  old file back and deletes the new one.
- A file is left as it is, and reported as a conflict, when another
  session's checkpoint of it follows this session's first one, or when its
  content is no longer what this session's last edit left there (the user,
  another session or a command changed it). A change made by one of this
  session's own commands after its last edit counts too: bash changes are not
  recorded.
- A checkpoint without a session (from an older version, or taken outside a
  session) is never restored from, since it may be another session's. A file
  it captured gets the content of this session's own first checkpoint of it
  (so the older edit stays); a file only such checkpoints captured, or one
  such a checkpoint captured after this session's first, is reported as a
  conflict.
- The restore is recorded as a checkpoint of this session, so a second rewind
  further back compares with what the first one wrote.

The model's `checkpoint` tool follows the same rules for `restore` (its
result carries `restored`, `removed`, `conflicts: [{path, reason}]`, and
`skipped`/`warning` as before); it refuses another session's checkpoint and
one without a session. `force: true` (not in the tool definition, to keep it
short; a result with conflicts suggests it) also overwrites a change made
since this session's last edit, never another session's checkpointed edit.
In `manual` approval mode a `restore` asks first, like an edit; a `restore`
with `force` asks in every mode but `full-access`, and "always allow" does
not cover it; `list` and `create` never ask.

### `steer`

```json
{"op":"steer"}
```

Sends the next queued message into the in-flight turn; running tools and
subagents keep going. Emits `status:"steering"` and the updated
`pending_messages`.

- While the model's response streams (reasoning, text, or tool-call
  arguments with no tool running yet), the response is cut off as soon as
  the next piece of it arrives and nothing of it is kept: not in the
  conversation the model sees, not in the saved transcript. Emits
  `response_interrupted`, then a `user_message` event for the message, and
  a new request (`connecting`, ...) with the message after the
  conversation so far. A tool call in the cut response never runs. (The
  transport hands over tool-call arguments only once the response is whole:
  a response that was writing tool calls is dropped when it ends, before
  any of them runs.)
- While tools run, the message waits for them (`status:"steering"` stays
  until its `user_message` event); the next request carries it.

If the turn ends before reading it, it runs as the next turn. A queued
message with images still stops the turn and starts a new one with it.
No-op when idle or when the queue is empty.

### `unqueue`

```json
{"op":"unqueue","index":0,"text":"also check the tests"}
```

Takes a queued message back before it runs. Removes the message at `index`
when its text equals `text`, else the first queued message with that text
(the queue may have moved on since the client saw it); without `text`, the
one at `index`. Emits the updated `pending_messages` either way, and
`{"type":"unqueued","text":…}` when a message was taken back (a client
returns its text to the message box; one that already started is not taken).

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
{"op":"approve","call_id":"call_9","decision":"allow","scope":"session","prefix":"npm install"}
{"op":"approve","call_id":"call_9","decision":"allow","hunks":["f0h1"]}
{"op":"approve","call_id":"call_9","decision":"deny","feedback":"Use pnpm instead."}
```

Structured answer to an `approval_request` event; equivalent to the
`/approve` slash command (`/approve <call-id> <allow|deny>
[once|session|project|always] [--hunks id1,id2]`).

- `call_id` (required): the `call_id` from the request.
- `decision` (required): `"allow"` or `"deny"`.
- `scope` (optional, default `"once"`): how far an allow reaches; one of
  the request's `scopes` (see "Approval requests"), else an error.
  - `once`: this call only.
  - `session`: for `edit` and `tool` requests, that tool stops asking for
    the rest of the session. For a command, commands starting with
    `prefix` run without asking for the rest of the session, also outside
    the sandbox: it is an `allow` rule for the session. For a `command`
    that runs inside an enforced OS sandbox (manual mode on macOS and
    Linux) it applies inside the sandbox only: the same command still asks
    to leave it. The shell is never allowed as a whole tool. A rule that
    asks or forbids the same prefix stays as it is (`error`; the call is
    allowed once).
  - `project`: `prefix` is saved as an `allow` command rule in
    `~/.lynshen/config.json`, under `projects[<workspace>].command_rules`,
    or in the top-level `command_rules` when `always_allow_scope` is
    `"global"` (`docs/sandbox.md` → "Settings"). It applies at once, and
    to later sessions.
- `prefix` (optional): the prefix to remember instead of the request's. It
  must start with the request's `prefix` and may continue only with the
  command's own next words (`npm install lodash` for `npm install lodash
  --save`), word by word: it can only make the rule narrower, never add
  words the user did not see.
- `always` (optional, older clients): the same as `scope: "session"`. On a
  request whose `scopes` are only `["once"]` it allows the call once.
- `feedback` (optional, with `deny`): the user's reason, given to the
  model with the refusal as it is. A client that offers "run it in my
  terminal" for `elevation: "terminal"` fills its terminal with the command
  and denies with feedback such as "The user will run this command in
  their own terminal; wait for them to tell you the result."
- `hunks` (optional): for edit-tool requests that carried a `hunks` list,
  apply only these hunk ids. Omit or `null` for the whole call. Cannot be
  combined with a `scope` beyond once or with `always`.
- `add_dir` (optional, with `allow`): `{"path": "/abs/dir", "persist":
  "session" | "project"}` (default `session`). Allows the call and makes
  the directory read-write, as `add_directory` does; usually the request's
  `dir_suggestion`. The directory must exist, and may not be the root, the
  home directory or one around it, or hold credentials or LynShen's
  settings.

A request with `must_ask` asks every time: no scope and no mode switch
approve the next one. Validation errors emit `error` and leave the request
pending, so the client can retry with a corrected op.

### Approval requests

An `approval_request` event carries, besides the call:

- `kind`: what the user decides.
  - `edit`: a file edit inside the workspace (manual mode).
  - `command`: a command inside the sandbox (manual mode), or any command
    where there is no sandbox to leave. Also what `write_stdin` types into
    a running shell.
  - `escalation`: a command that leaves the sandbox (`escalate: true`).
  - `outside_write`: a write outside the working directory and the
    writable directories, by a file tool or by a command whose target the
    command text shows (`cp a ~/Desktop/`, `> ~/notes.txt`).
  - `elevation`: `sudo`, `su`, `doas`, `pkexec`, `gsudo`, `runas`,
    `Start-Process -Verb RunAs`.
  - `rule`: an `ask` command rule, or the built-in list that always asks
    (`docs/sandbox.md`), also for what `write_stdin` types.
  - `tool`: any other tool (MCP tools, `merge_agent`, a checkpoint restore,
    ...).
- `must_ask`: a person must decide. The approval mode, "always allow" and
  a switch to a looser mode never approve it.
- `justification`: the model's reason (a shell call's `justification`), or
  `null`.
- `cwd`: where the call runs (a command's `workdir`, else the agent's
  directory).
- `command`: the shell command as the model wrote it, or `null`.
- `paths`: the paths outside the working directory and the writable
  directories that the call writes (absolute). Empty when there are none,
  or when the command text does not show them.
- `dir_suggestion`: a directory the client can offer to make writable
  (`approve` with `add_dir`): under the home directory its first directory
  (`~/Desktop` for `~/Desktop/a/b.txt`; two levels under `Library`,
  `AppData`, `.config`, `.local` and OneDrive), elsewhere the file's
  directory; the nearest one that exists. `null` without `paths`, and
  never the home directory or one around it, a root, LynShen's own
  directory or a credentials directory.
- `prefix`: the command prefix a `session` or `project` approval
  remembers: the program as written and its words up to the first one
  that is not an option (`npm install`, `npx -y create-vite`, `make`),
  without wrappers such as `time` or `nohup`; or `null`.
- `scopes`: how far the engine lets an approval reach, a subset of
  `["once", "session", "project"]`.
  - `["once"]` for a request that must be asked, an interpreter (`python`,
    `node`, a shell: `bash -c`, `bash build.sh`, `powershell -Command`,
    ...), a delete (`rm`, `Remove-Item`, ...), an elevation,
    `git push --force`, a command whose parts start differently
    (`make && npm test`), a part with a variable assignment, `env` or
    `xargs` in front, a write outside the workspace by a file tool or a
    command (offer `add_dir` instead) and a forced checkpoint restore.
  - `["once", "session"]` for `edit` and `tool` (`session` stops asking
    for that tool in this session), and for a `command` inside an enforced
    OS sandbox (`session` stops asking for its prefix there only).
  - `["once", "session", "project"]` for other commands, with `prefix`.
- `elevation`: `null`, or for `kind: "elevation"` how the user completes
  it: `"terminal"` (macOS and Linux: the user runs the command in their own
  terminal, where they can type a password) or `"uac"` (Windows: once
  approved, Windows asks through UAC).

### `set_approval_mode`

```json
{"op":"set_approval_mode","mode":"manual"}
```

`mode` is `manual`, `plan`, `auto-edit`, `auto`, or `full-access`. Emits
`status:"approval mode: ..."` and an `approval_mode` event. The change
applies to new turns; an in-flight turn's gating can only loosen. A call
waiting for approval that the new mode does not gate runs at once, except
one with `must_ask` (an `ask` command rule, the built-in list, a worktree
agent leaving its sandbox): that one still waits for `approve`. A waiting
call that leaves the sandbox and runs this way is reported as `escalated`.

The approval mode does not change the sandbox. `full-access` keeps it: a
command still runs in the sandbox, and one that leaves it (`escalate`) or
a write outside the workspace runs without asking and emits `escalated`;
requests with `must_ask` still ask. To turn the sandbox off, use
`set_sandbox` with `full-access`. LynShen Desktop's modes:

| Desktop | `approval_mode` | sandbox |
| --- | --- | --- |
| 谨慎 (careful) | `manual` | `workspace-write` |
| 计划 (plan) | `plan` | `workspace-write` |
| 自动 (auto, the default) | `auto` | `workspace-write` |
| 全自动 (full auto) | `full-access` | `workspace-write`, kept |
| 完全访问 (full access, in settings only) | `full-access` | `full-access` |

`auto` asks the safety model (`safety_model`) about a command or a
crossing before asking you; without one it asks you, as `auto-edit` does.

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
- `discard` deletes the worktree. The state becomes `discarded`; an agent
  that failed (`errored`, `interrupted`, `closed`, `budget_exhausted`) keeps
  that state.

The op asks for no approval: the click is the user's decision. The model's
own `merge_agent` tool asks for approval of `apply` in `manual` mode, like a
file edit; the request's `summary` names the files and line counts
(`merge /root/w: 3 files (+40 -5): a.rs, b.rs, c.rs`). `discard` never asks.

A worktree that is not merged when its turn ends stays on disk. A later turn
or this op can still merge it while the engine runs. Worktrees in
`<cwd>/.lynshen/agents/` older than `agents.keep_worktrees_days` are deleted
when an engine opens the directory.

An agent that ends in error (not stopped by a person) and changed nothing in
its worktree has the worktree deleted before its failure is reported: there
is nothing to merge, and the same task name can be spawned again. Its error
then ends with a line saying so. One with changes keeps its worktree.

Emits `merge_result`; on success or conflict also `subagent_lifecycle` with
the new state; then `agent_runs`. An unknown target, an agent that is still
running or one without a worktree emits `merge_result` with `ok: false` and
`error`.

The worktree registry is saved with the session: after an engine or daemon
restart (the session opened again with `/resume` or `session_open`), the
worktree agents come back as `agent_runs` rows (their state, error and run
time included) and this op still merges them.
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
returns an `error` event. The text command `/plan <id> approve [mode]
[notes]` / `/plan <id> revise <feedback>` does the same (the TUI uses it).

### `set_sandbox`

```json
{"op":"set_sandbox","mode":"workspace-write","network":true}
```

Sets this session's sandbox: `mode` is `read-only`, `workspace-write` or
`full-access`; `network` is optional (unchanged when left out). Not saved:
config.json's `sandbox` and `sandbox_network` set the default. The
session's next engine (a daemon reopening an idle session) keeps it.
Emits `sandbox`. When the OS sandbox cannot run here (Linux without a
working `bwrap`), emits `error` and `sandbox` and changes nothing. On
Windows a mode other than `full-access` (`workspace-write` is the default
there too) is accepted but not enforced (`enforced: false`, see
`docs/sandbox.md`).

### `set_style`

```json
{"op":"set_style","style":"concise"}
{"op":"set_style","style":null}
```

Sets this conversation's reply style (see "Reply styles"): a style id, or
`null` (or `""`) to follow the default for new conversations
(`reply_style` in config.json). It applies from the next user message. The
choice is saved with the session (once it has a message), so reopening it,
`/resume` and a daemon's idle close keep it. Emits `reply_style`. An unknown
id emits `error` and `reply_style` and changes nothing. `/style <id>` does
the same.

### `add_directory` / `remove_directory`

```json
{"op":"add_directory","path":"/Users/me/Desktop","mode":"rw","persist":"session"}
{"op":"remove_directory","path":"/Users/me/Desktop","persist":"session"}
```

Adds a directory to the sandbox, or takes one out, while the session runs.
`path` is an existing absolute directory. `mode` is `rw` (default: commands
and file tools may write it) or `ro` (read-only, also where it sits under
a writable directory). `persist` (default `session`):

- `session`: this session only (kept by its next engine);
- `project`: saved in `~/.lynshen/config.json` under
  `projects[<workspace>].sandbox_directories`, for every session of the
  project;
- `global`: saved in the top-level `sandbox_directories`.

`remove_directory` takes the directory out of that source; it is an error
when it is not there. Directories the daemon adds from the project's own
list (a project's `dirs` in `workspaces_set`) are reported with source `project`; removing one
here lasts until the session's engine is opened again, so remove it from
the project instead. Both ops emit `sandbox`; an invalid path, mode or
`persist` emits `error` and changes nothing.

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
| `model_status` | `provider`, `model`, `reasoning_effort`, `context_window`, `context_limit`, `max_output_tokens`, `reasoning_efforts`, `input_modalities`, `state` | Current model selection; deduplicated, re-emitted on change. `reasoning_efforts` is empty for a model configured with `"reasoning": false`; `input_modalities` lists `text` and/or `image` (both unless the model entry says otherwise). Claude Code sessions add their own fields (`docs/daemon-protocol.md` → Other engines). |
| `command_list` | `commands: [{command, marker, args, description}]` | Available slash commands incl. custom and MCP prompt commands. |
| `approval_mode` | `mode` | Current approval mode; emitted at startup and on change. |
| `sandbox` | `mode`, `network`, `enforced`, `unavailable`, `directories` | The session's sandbox; emitted at startup (and to a client that attaches) and after every change (`set_sandbox`, `/sandbox <mode>`, `add_directory`, `remove_directory`, `approve` with `add_dir`). `enforced`: an OS sandbox applies the mode; false on Windows and in `full-access`. `unavailable`: why the OS sandbox cannot run here (bubblewrap missing, AppArmor blocking it) with the fix, else `null`. `directories`: `[{path, mode, source}]`, the read-write (`rw`) and read-only (`ro`) directories it adds, by where they come from: `global` (config.json `sandbox_directories`, or a daemon agent's directories), `project` (config.json `projects`, and the directories the daemon adds for the project) or `session`. |
| `reply_style` | `style`, `name`, `source`, `chosen`, `missing` | The conversation's reply style (see "Reply styles"): at startup (and to a client that attaches), after `set_style` and `/style <id>`, and when a turn starts with another style than last told (the default changed, a file was edited away). `style` is the id in effect (`default` for none); `name` its name; `source` `builtin`, `user` or `project`; `chosen` is true when the conversation picked it, false when it follows `reply_style` in config.json. `missing: true` (with `source` `missing`): `style` names no style now, and the conversation runs with none. |
| `escalated` | `call_id`, `kind`, `command`, `justification`, `cwd`, `paths`, `by` | A call that leaves the sandbox or writes outside the workspace (`kind` `escalation` or `outside_write`) ran without asking anyone. `by`: `mode` (`full-access`, or a switch to it freed the call), `rule` (an `allow` rule, also one an approval remembered) or `classifier` (the safety model in `auto`). Clients show it as a line in the conversation. |
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
| `response_interrupted` | — | The model response being streamed (its reasoning, text and tool-call arguments so far) was cut off for a steered message (see `steer`) and is not kept. Mark what streamed of it as interrupted; the turn goes on, and the next `reasoning_delta`/`assistant_delta` belong to a new response. |
| `retrying` | `attempt` | The request is being retried after a transient failure. |
| `tool_start` | `call_id`, `name` | Tool call begins. |
| `tool_update` | `call_id`, `name`, `output` | Intermediate tool progress (e.g. long-running bash). |
| `tool_output` | `call_id`, `name`, `output`, `is_error` | Tool call finished. |
| `approval_request` | `call_id`, `name`, `summary`, `subagent_id`, `hunks`, `kind`, `must_ask`, `justification`, `cwd`, `command`, `paths`, `dir_suggestion`, `prefix`, `scopes`, `elevation` | A gated tool call waits for an `approve` op. `hunks` is a list of `{id, file, header, lines}` for partial approval, `null` otherwise. `subagent_id` is set when a subagent issued the call. The other fields are described in "Approval requests". |
| `action_deferred` | `id`, `session_id`, `cwd`, `call_id`, `name`, `arguments`, `summary`, `subagent_id`, `digest`, `created_at`, `kind`, `justification` | An unattended session recorded a gated call instead of prompting; decide it with `decide_action`. `kind` and `justification` are as in `approval_request` (`null` in a record saved by an older version). |
| `action_decided` | `id`, `decision`, `output`, `is_error` | A deferred action was decided; `output` is the tool result when it ran, `null` when declined. |
| `attended` | `attended` | Current attended state, after `set_attended`. |
| `subagent_lifecycle` | `path`, `status`, `message`, `label`, `model`, `tool_use_id`, `role`, `plan_step`, `background`, `attempt_group`, `attempt` | Subagent spawn/progress/finish notices. `status` is a state of `agent_runs` or `message` (a message was queued for it); `pending` with message `resumed` when `resume_agent` runs it again. `role`, `plan_step` and `attempt_group` are strings or `null`, `attempt` a number or `null`, `background` a boolean (as in `agent_runs`). |
| `agent_message` | `from`, `to`, `summary` | An agent sent another one a message with `send_message` (parent, child or sibling). `from` and `to` are agent paths (`/root` is the main agent); `summary` is the message, cut to 200 characters. Also sent when a background agent's result reaches the main agent (`from` the agent, `to` `/root`, `summary` the start of its result) and when a `task_completed` or `agent_idle` hook reports (`from` `hook:task_completed` or `hook:agent_idle`). |
| `task_board` | `tasks: [{id, title, detail, status, owner, depends_on, role, files, result, updated_at}]` | The session's shared task board, whole, after every change. `id` is `t1`, `t2`, …; `status` ∈ `pending`/`claimed`/`completed`/`failed`/`blocked`; `owner` the claiming agent's path or `null`; `depends_on` task ids; `role` and `result` strings or `null`; `detail` a string (empty when none); `files` paths; `updated_at` epoch ms. Part of the startup batch and of a resumed session's events when the board has tasks. See "Agent team" → "Task board". |
| `merge_result` | `target`, `action`, `ok`, `files`, `conflicts`, `error` | `merge_agent` ran, from the model or the op. `files`: what the worktree changes. `ok: true`: applied (`apply`) or deleted (`discard`). `ok: false` with `conflicts`: nothing was written. `ok: false` with `error` (a string, else `null`): the merge could not run. |
| `team_budget` | `used`, `limit` | Input + output tokens the subagents used in the current budget window, against `agents.turn_token_budget`. A window opens with each main turn except one the engine starts for a background result, which continues it. Sent at the first spawn of a window and each time `used` reaches another tenth of `limit`; never without a budget. |
| `usage` | `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_tokens` | Real API usage for the completed turn. |
| `context_usage` | `tokens`, `tokenizer`, `cost` | Tokenizer-counted context size; `cost` is cumulative USD (0 when unpriced). |
| `compaction_start` | — | Context compaction began. |
| `compaction_end` | `summary` | Context compaction finished; the older conversation now reads as `summary`. When the session has a plan, `summary` then has a blank line, `Your plan from update_plan, as it stands:`, one `N. [<status>] <step>` line per step and `Keep it current with update_plan.`, so the model keeps its plan; it is part of the summary, which comes before the kept conversation, so the user's latest request stays the last message. When the session's edit tools changed files, `summary` ends with a blank line, `Files changed in this session:` and one `- <path>` line per file (relative to the session's directory when under it; past 100 files, a last line `- and N more`). Other engines send no `summary`. |
| `compaction_progress` | `output_tokens` | Compaction summary tokens produced so far. |
| `compaction_failed` | `error` | Compaction failed; the session continues uncompacted. |
| `model_view` | `models: [{model, active, context_window, max_output_tokens, reasoning_efforts, input_modalities}]`, `active_effort` | Model picker data (`/model`). |
| `tree_view` | `nodes: [{id, parent_id, label, active}]` | Session branch tree (`/tree`). |
| `resume_view` | `items: [{id, label, active}]` | Session picker data (`/resume`). |
| `checkpoint_view` | `items: [{id, label, detail}]` | Checkpoint picker data. |
| `goal` | `goal: {objective, status, token_budget, tokens_used, time_used_seconds, created_at, updated_at} \| null` | Goal state; `null` clears it. |
| `plan` | `plan: [{step, status, agent, files}]` | Goal/plan step list. `agent` (string or `null`) is the subagent doing the step, as the model named it (path or task name); `files` the files it is expected to write. Saved with the session: part of the startup batch and of a resumed session's events when the session has a plan; `/new` or `/resume` sends an empty `plan` to clear the previous session's plan when it had one and the next session has none. `/rewind`, `/checkout` and `/fork` set the plan the last successful `update_plan` call before that point set (none without one), and send `plan` when it changed. |
| `transcript` | `items: [{role, ...}]` | Rendered conversation (`/transcript`); roles: `user`, `assistant`, `tool` (`name`, `output`), `branch` (`label`), `compaction` (`summary`: where the older conversation was folded). |
| `info` | `message` | Informational line (hook output, notices, update available, loop guard warnings; see "Loop guard"). |
| `error` | `message` | Error line. Also used for malformed commands and unknown ops, and for a turn the loop guard stopped (message starts with `loop guard:`). |
| `status` | `message` | Turn status string: `ready`, `streaming`, `queued: N`, `steering`, `interrupted`, `compacting`, `approval mode: ...`, `trusted ...`, etc. `ready` marks the end of a turn. |

## Reply styles

A reply style sets how the model writes to the user: tone, length,
structure, how much it explains. It changes nothing the engine does: the
rules on safety, tools, approvals, the sandbox and verifying work, and the
project's instructions (AGENTS.md, CLAUDE.md), stay as they are. Code,
comments, commit messages and files follow the project, not the style. A
request in a user message wins over the style for that message.

Built-in styles: `default` (默认: no style text; the prompt of before),
`concise` (简洁), `explanatory` (详细讲解), `learning` (教学引导), `formal`
(正式汇报). Their instructions are Chinese; the model replies in the user's
language.

A style file is Markdown: a frontmatter `name` and `description` (both
optional: the name defaults to the id), then the instructions. The file
name without `.md` is the id: no white space or `/ \ : * ? " < > |`, not
starting with `.` or `-`, at most 64 characters. At most 8 KiB of
instructions are used.

```markdown
---
name: 代码评审
description: 指出行号，按严重程度排序
---
评审时先列出问题，按严重程度从高到低排列，每条写出文件和行号。
```

Files load from `~/.lynshen/styles/` and, in a trusted project, from
`<cwd>/.lynshen/styles/` (a `.lynshen/styles` folder makes the engine ask
for trust, as project skills do; a chat loads no project styles). For one
id a project file wins over a personal file, which wins over a built-in.
`default` cannot be replaced: a `default.md` does not load. A file that
cannot load (a bad name, no instructions, unreadable) is listed by the
daemon's `styles_list` with a stable `code` for clients to word, and the
English detail, which the daemon also logs.

Which style a conversation uses: its own choice (`set_style`, `/style
<id>`, `--style <id>` at start), else `reply_style` in config.json (the
default for new conversations, `/style <id> --global`), else `default`.
Both are read again at each turn. A style whose file is gone runs as
`default` and `reply_style` says `missing: true`.

In the system prompt the style is the last section, after the host text:

```text
<reply_style id="concise" name="简洁">
Reply style: the user chose how your messages to them should read. …
<the style's instructions>
</reply_style>
```

Everything before `<env>`, which providers cache across sessions, is the
same with any style, and `default` adds nothing. A session keeps the
system prompt its first turn built: a style chosen or changed later, an
edited style file or a new default reaches the model as a note before the
next user message (`<system-reminder>The reply style changed. From now
on: …`, or that no reply style applies any more), so the provider still
serves the earlier conversation from its cache. Subagents get the prompt
without the style: they report to the main agent, not to the user.

`/style` lists the styles and marks the current one. `/style <id>` sets
this conversation's; `/style <id> --global` saves it as `reply_style` and
the conversation follows it.

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

### Model and effort

A subagent runs on its parent's provider and connection; `model` only picks
which of that provider's models (and so its wire protocol). A provider can
refuse a model it lists, for example one the account's plan does not
include: the agent then ends `errored`, and its error starts with the model
and the endpoint the request went to, followed by the provider's whole
answer (`claude-sonnet-5 at https://api.commandcode.ai/provider/v1/messages:
LLM API returned HTTP 403: {"type":"error",...}`). A failed subagent's error
is also written to `~/.lynshen/logs/lynshen.log` (target `subagent`, message
`failed`).

A `reasoning_effort` the model does not offer (from `spawn_agent` or from the
role) runs at the nearest one it does: the next higher tier (`none` <
`minimal` < `low` < `medium` < `high` < `xhigh` < `max` < `ultra`), else the
model's highest; `default` on a model without it gets the model's start
effort. A word that is no effort is refused. The `spawn_agent` result names
the effort the agent runs at (`reasoning_effort`) and has a `note` when it is
not the one asked for. `/model <model> <effort>` follows the same rule and
emits an `info` event naming the effort it set.

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
{"task_completed": [{"command": "cargo test 2>&1 | tail -20", "timeout": 600}],
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

Every hook entry, of any event, may set `timeout` in seconds (at most a
day). The default is 60 for the hooks around each tool call or prompt
(`pre_tool_use`, `post_tool_use`, `user_prompt_submit`) and 600 for the others
(`session_start`, `stop`, `task_completed`, `agent_idle`), which often run a
build or a test suite. A hook still running then is killed with its process
group (its process tree on Windows), so it cannot hold up the turn, and counts
as failed
with the message `<event> hook timed out after <n>s and was stopped:
<command>`: a `pre_tool_use` hook blocks the tool, a `user_prompt_submit`
hook blocks the prompt, `task_completed` and `agent_idle` report it with
`ok="false"`, `session_start` and `stop` show it as `hook (<event>): …`, and
`post_tool_use` ignores it. Once a hook has exited, output that a process it
left running keeps open is read for at most another half second. A hook still
running when the engine process exits is stopped with the tool commands. An
`interrupt` op (ACP: `session/cancel`) also stops a running `session_start`,
`user_prompt_submit` or `stop` hook, which holds the session until it ends; it
reports `<event> hook was stopped by an interrupt: <command>`. The TUI cannot
stop one yet: it reads keys on the thread the hook holds.

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

## Loop guard

The engine stops a turn that goes in circles, so an unattended session
(daemon dispatch, a schedule, goal continuation) does not spend tokens until
someone notices. It counts within one turn, for the main agent and each
subagent alike:

- **The same call again.** The same tool with the same arguments (as JSON,
  spacing and key order ignored) several times in a row. The 3rd and 4th
  calls run, and their results carry a `loop_guard` note that tells the
  model to change its approach; the engine emits an `info` event
  `loop guard: <tool> was called N times in a row ...` (a subagent's path
  follows `loop guard:` in parentheses). The 5th call does not run, nor
  does any other call of the same model response: each one's `tool_output`
  is an error starting `not run: loop guard:`, and the turn ends.
- **Calls that keep failing.** 6 tool rounds (model responses) in a row
  whose calls all failed end the turn after the 6th. A round with any
  successful call starts the count again. A failure is a tool error: bad
  arguments, a missing file, an unknown tool, a command that could not run,
  was killed or timed out. A shell command that ran and exited non-zero
  (grep without a match, a failing test run) is no failure.

Calls whose repetition is normal are not counted, and end a run of
identical calls (running the tests after each worker an orchestrator waited
for is progress): `write_stdin`, `wait_agent`, `list_agents`, `task_list`,
`list_sessions`, `read_session`, and a shell command whose first word is
`sleep` (`Start-Sleep` on Windows).

A turn the guard ends emits `error` with a message that starts with
`loop guard:`, then `status` `ready`, as any failed turn does. An active
goal is paused (`goal` with `status: "paused"`, then `info`), so goal
continuation does not start the loop again; `/goal resume` continues it.
A stopped subagent ends `errored` with that message as its error. The
limits are fixed (`crates/agent-core/src/loop_guard.rs`).

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
< {"type":"approval_request","call_id":"call_2","name":"bash","summary":"rm -rf build","subagent_id":null,"hunks":null,"kind":"command","must_ask":false,"justification":null,"cwd":"/work/app","command":"rm -rf build","paths":[],"dir_suggestion":null,"prefix":null,"scopes":["once"],"elevation":null}
> {"op":"approve","call_id":"call_2","decision":"deny"}
```

Interrupt:

```jsonl
> {"op":"interrupt"}
< {"type":"info","message":"request interrupted"}
< {"type":"status","message":"interrupted"}
```

Steer while the model reasons:

```jsonl
< {"type":"reasoning_delta","delta":"..."}
> {"op":"user_message","content":"use the staging database"}
< {"type":"pending_messages","messages":["use the staging database"]}
> {"op":"steer"}
< {"type":"status","message":"steering"}
< {"type":"pending_messages","messages":[]}
< {"type":"response_interrupted"}
< {"type":"user_message","content":"use the staging database"}
< {"type":"connecting"}
< {"type":"reasoning_delta","delta":"..."}   // the new response
```

## Mapping to UI

Suggested rendering (as used by LynShen Desktop):

| Events | UI |
| --- | --- |
| `assistant_start` / `assistant_delta` | streaming message bubble |
| `response_interrupted` | mark the streaming bubble and thinking section interrupted; later deltas start new ones |
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
