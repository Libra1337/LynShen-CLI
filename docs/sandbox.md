# Sandbox

LynShen runs the model's shell commands in an OS sandbox: Seatbelt
(`sandbox-exec`) on macOS, `bwrap` (bubblewrap) on Linux. It applies to every
engine: the TUI, `lynshen serve`, `--headless`, Desktop sessions and daemon
sessions. A daemon agent uses its own settings from `agent.json`
(`docs/daemon-protocol.md`).

## Modes

| Mode | Commands can write |
| --- | --- |
| `read-only` | nothing |
| `workspace-write` (default) | the working directory, the configured read-write directories, temp and package-cache directories (`~/.cache`, `~/.npm`, `~/.cargo/registry`, …). Inside them `.git` (and a worktree's real git directory), `.lynshen` and `.agents` stay read-only, as do the configured read-only directories. |
| `full-access` | anything: no sandbox |

In every sandboxed mode `~/.ssh`, `~/.gnupg`, `~/.aws`,
`~/.lynshen/auth.json` and `~/.lynshen/daemon` are unreadable. Network access
is on unless `sandbox_network` is false.

Windows has no sandbox yet and defaults to `full-access`. On Linux without
`bwrap` (or where unprivileged user namespaces are disabled) LynShen reports
the sandbox as unavailable at startup and shell commands fail until you
install bubblewrap or switch to `full-access`; it never falls back to running
commands unsandboxed.

## Approvals

A command inside the sandbox needs no approval, except in `manual` mode,
which still asks for every command. A command that must leave the sandbox
(commit to git, write elsewhere) is called with `escalate: true` and a
`justification`, and goes through the approval mode: `auto` asks the safety
model, `manual` and `auto-edit` ask you, `full-access` runs it.

Command rules are checked first, by how the command starts:

- `forbid`: never runs;
- `ask`: always asks you;
- `allow`: may leave the sandbox without asking.

`forbid` wins over other matches, otherwise the longest prefix applies. The
defaults let `git add` and `git commit` leave the sandbox and always ask
before `git push`.

File tools (`write`, `edit`, `apply_patch`, …) check writes against the same
rules in-process, and can read and write the configured directories.

## Worktree subagents

A subagent that works in its own git worktree (`isolation: "worktree"`, the
`worker` role; see `docs/serve-protocol.md` → "Agent team") is confined to it:

- Its commands start in the worktree; a `workdir` outside it is refused.
- In the sandbox, the worktree, temp and package-cache directories are its
  only writable places. The project around the worktree and the configured
  read-write directories are read-only for it, even where they lie in temp.
  The worktree's `.git` file and its real git directory stay read-only.
- Leaving the sandbox (`escalate: true`) always asks you, whatever the
  approval mode, unless an `allow` command rule matches (`git commit` by
  default).
- Its file tools write only inside the worktree.

With the sandbox off (`full-access`, Windows) only the first and last points
hold: a command started in the worktree can still write elsewhere by an
absolute path.

## Settings

In `~/.lynshen/config.json`:

```json
{
  "sandbox": "workspace-write",
  "sandbox_network": true,
  "sandbox_directories": [
    { "path": "/srv/deploy", "mode": "rw" },
    { "path": "/var/log/app", "mode": "ro" }
  ],
  "command_rules": [
    { "prefix": "git add", "action": "allow" },
    { "prefix": "git commit", "action": "allow" },
    { "prefix": "git push", "action": "ask" }
  ]
}
```

`/sandbox` shows the current sandbox; `/sandbox <mode>` switches the mode for
the current session.

Hook commands are written by you and are not sandboxed.
