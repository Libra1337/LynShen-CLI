use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

/// User-configured shell hooks that run at lifecycle points. Loaded from
/// `~/.lynshen/hooks.json` (global) and `<cwd>/.lynshen/hooks.json` (project,
/// only when the project is trusted). Cheap to clone — shared via `Arc`.
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    inner: Arc<HookSet>,
}

#[derive(Debug, Default)]
struct HookSet {
    session_start: Vec<HookEntry>,
    user_prompt_submit: Vec<HookEntry>,
    pre_tool_use: Vec<HookEntry>,
    post_tool_use: Vec<HookEntry>,
    stop: Vec<HookEntry>,
    task_completed: Vec<HookEntry>,
    agent_idle: Vec<HookEntry>,
}

/// Most characters of a hook's output the main agent receives.
const MAX_REPORT_CHARS: usize = 4000;

/// What a `task_completed` or `agent_idle` hook said, for the main agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub event: &'static str,
    /// The hook exited 0.
    pub ok: bool,
    /// Its stdout, trimmed and capped; for a failure the stderr when stdout
    /// is empty, or the exit code.
    pub text: String,
}

#[derive(Debug, Clone)]
struct HookEntry {
    command: String,
    /// Tool names this hook applies to. `None` means every tool.
    tools: Option<Vec<String>>,
}

impl HookEntry {
    fn matches(&self, tool: &str) -> bool {
        match &self.tools {
            Some(tools) => tools.iter().any(|name| name == tool),
            None => true,
        }
    }
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Hooks {
    pub fn load(profile_dir: &Path, cwd: &Path, project_trusted: bool) -> Self {
        let mut set = HookSet::default();
        merge_file(&mut set, &profile_dir.join("hooks.json"));
        if project_trusted {
            merge_file(&mut set, &cwd.join(".lynshen").join("hooks.json"));
        }
        Self {
            inner: Arc::new(set),
        }
    }

    /// Hooks from one `hooks.json` value (tests).
    #[cfg(test)]
    pub(crate) fn from_value(value: &Value) -> Self {
        let mut set = HookSet::default();
        merge_value(&mut set, value);
        Self {
            inner: Arc::new(set),
        }
    }

    pub fn has_task_completed(&self) -> bool {
        !self.inner.task_completed.is_empty()
    }

    pub fn has_agent_idle(&self) -> bool {
        !self.inner.agent_idle.is_empty()
    }

    /// Runs `task_completed` hooks in `workdir` (the task owner's): stdin
    /// carries `{"event", "task", "workdir"}`, the environment
    /// `LYNSHEN_HOOK_WORKDIR`, `LYNSHEN_TASK_ID` and `LYNSHEN_TASK`.
    pub fn task_completed(&self, task: &Value, workdir: &Path) -> Vec<HookReport> {
        let id = task["id"].as_str().unwrap_or_default().to_string();
        let env = [("LYNSHEN_TASK_ID", id), ("LYNSHEN_TASK", task.to_string())];
        self.report(
            &self.inner.task_completed,
            "task_completed",
            ("task", task),
            workdir,
            &env,
        )
    }

    /// Runs `agent_idle` hooks in `workdir` (the subagent's) once a
    /// subagent finished: stdin carries `{"event", "agent", "workdir"}`, the
    /// environment `LYNSHEN_HOOK_WORKDIR` and `LYNSHEN_AGENT` (its path).
    pub fn agent_idle(&self, agent: &Value, workdir: &Path) -> Vec<HookReport> {
        let path = agent["path"].as_str().unwrap_or_default().to_string();
        self.report(
            &self.inner.agent_idle,
            "agent_idle",
            ("agent", agent),
            workdir,
            &[("LYNSHEN_AGENT", path)],
        )
    }

    fn report(
        &self,
        entries: &[HookEntry],
        event: &'static str,
        (key, value): (&str, &Value),
        workdir: &Path,
        env: &[(&str, String)],
    ) -> Vec<HookReport> {
        let mut reports = Vec::new();
        let workdir_text = workdir.display().to_string();
        let mut env = env.to_vec();
        env.push(("LYNSHEN_HOOK_WORKDIR", workdir_text.clone()));
        for entry in entries {
            let payload = json!({
                "event": event,
                key: value,
                "workdir": workdir_text,
                "cwd": workdir_text,
            });
            let outcome = run_command_with_env(&entry.command, &payload, workdir, &env);
            let ok = outcome.code == 0;
            let text = if ok {
                outcome.stdout.trim().to_string()
            } else {
                block_reason(&outcome, event)
            };
            if ok && text.is_empty() {
                continue;
            }
            reports.push(HookReport {
                event,
                ok,
                text: cap(&text),
            });
        }
        reports
    }

    /// Runs `session_start` hooks and returns any non-empty stdout to surface.
    pub fn session_start(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.session_start, "session_start", json!({}), cwd)
    }

    /// Runs `stop` hooks (turn finished) and returns any non-empty stdout.
    pub fn stop(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.stop, "stop", json!({}), cwd)
    }

    /// Runs `user_prompt_submit` hooks. A non-zero exit blocks the prompt and the
    /// reason is returned as `Err`.
    pub fn user_prompt_submit(&self, prompt: &str, cwd: &Path) -> Result<(), String> {
        let payload = json!({
            "event": "user_prompt_submit",
            "prompt": prompt,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.user_prompt_submit {
            let outcome = run_command(&entry.command, &payload, cwd);
            if outcome.code != 0 {
                return Err(block_reason(&outcome, "user_prompt_submit"));
            }
        }
        Ok(())
    }

    /// Runs `pre_tool_use` hooks for `tool`. Returns `Some(reason)` to block the
    /// tool from executing.
    pub fn pre_tool(&self, tool: &str, arguments: &str, cwd: &Path) -> Option<String> {
        if self.inner.pre_tool_use.is_empty() {
            return None;
        }
        let payload = json!({
            "event": "pre_tool_use",
            "tool": tool,
            "arguments": parse_arguments(arguments),
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.pre_tool_use {
            if !entry.matches(tool) {
                continue;
            }
            let outcome = run_command(&entry.command, &payload, cwd);
            if outcome.code != 0 {
                return Some(block_reason(&outcome, "pre_tool_use"));
            }
        }
        None
    }

    /// Runs `post_tool_use` hooks for `tool`. Best-effort; failures are ignored.
    pub fn post_tool(&self, tool: &str, output: &str, cwd: &Path) {
        if self.inner.post_tool_use.is_empty() {
            return;
        }
        let payload = json!({
            "event": "post_tool_use",
            "tool": tool,
            "output": output,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.post_tool_use {
            if entry.matches(tool) {
                let _ = run_command(&entry.command, &payload, cwd);
            }
        }
    }

    fn notify(&self, entries: &[HookEntry], event: &str, extra: Value, cwd: &Path) -> Vec<String> {
        let mut messages = Vec::new();
        for entry in entries {
            let mut payload = json!({ "event": event, "cwd": cwd.display().to_string() });
            if let (Some(map), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    map.insert(key.clone(), value.clone());
                }
            }
            let outcome = run_command(&entry.command, &payload, cwd);
            let text = outcome.stdout.trim();
            if !text.is_empty() {
                messages.push(format!("hook ({event}): {text}"));
            }
        }
        messages
    }
}

fn block_reason(outcome: &Outcome, event: &str) -> String {
    let stderr = outcome.stderr.trim();
    let stdout = outcome.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("{event} hook exited with code {}", outcome.code)
    }
}

fn cap(text: &str) -> String {
    let mut capped: String = text.chars().take(MAX_REPORT_CHARS).collect();
    if capped.len() < text.len() {
        capped.push_str("\n[hook output cut]");
    }
    capped
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn run_command(command: &str, payload: &Value, cwd: &Path) -> Outcome {
    run_command_with_env(command, payload, cwd, &[])
}

fn run_command_with_env(
    command: &str,
    payload: &Value,
    cwd: &Path,
    env: &[(&str, String)],
) -> Outcome {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let event = payload
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let spawn = Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .env("LYNSHEN_HOOK_EVENT", event)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
            }
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    match child.wait_with_output() {
        Ok(output) => Outcome {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(error) => Outcome {
            code: -1,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

fn merge_file(set: &mut HookSet, path: &Path) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    merge_value(set, &value);
}

fn merge_value(set: &mut HookSet, value: &Value) {
    push_entries(&mut set.session_start, value, "session_start");
    push_entries(&mut set.user_prompt_submit, value, "user_prompt_submit");
    push_entries(&mut set.pre_tool_use, value, "pre_tool_use");
    push_entries(&mut set.post_tool_use, value, "post_tool_use");
    push_entries(&mut set.stop, value, "stop");
    push_entries(&mut set.task_completed, value, "task_completed");
    push_entries(&mut set.agent_idle, value, "agent_idle");
}

fn push_entries(target: &mut Vec<HookEntry>, value: &Value, key: &str) {
    let Some(entries) = value.get(key).and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
        else {
            continue;
        };
        let tools = entry.get("tools").and_then(Value::as_array).map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        target.push(HookEntry {
            command: command.to_string(),
            tools,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_from(value: Value) -> Hooks {
        let mut set = HookSet::default();
        push_entries(&mut set.pre_tool_use, &value, "pre_tool_use");
        Hooks {
            inner: Arc::new(set),
        }
    }

    #[test]
    fn pre_tool_blocks_on_nonzero_exit() {
        let command = if cfg!(windows) {
            "echo denied >&2 & exit /b 1"
        } else {
            "echo denied >&2; exit 1"
        };
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": command, "tools": ["bash"] }]
        }));
        let reason = hooks.pre_tool("bash", "{}", Path::new("."));
        assert_eq!(reason.as_deref(), Some("denied"));
        // Tool filter excludes "read".
        assert!(hooks.pre_tool("read", "{}", Path::new(".")).is_none());
    }

    #[test]
    fn task_completed_hooks_get_the_task_and_run_in_the_workdir() {
        if cfg!(windows) {
            return;
        }
        let workdir = std::env::temp_dir().join(format!(
            "lynshen-hook-task-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&workdir).unwrap();
        let hooks = Hooks::from_value(&json!({
            "task_completed": [
                { "command": "cat > payload.json; printf 'tests ok in %s for %s' \"$(basename \"$PWD\")\" \"$LYNSHEN_TASK_ID\"" },
                { "command": "echo 'quiet hook'  >/dev/null" },
                { "command": "echo broken >&2; exit 3" }
            ],
            "agent_idle": [{ "command": "printf '%s idle' \"$LYNSHEN_AGENT\"" }]
        }));
        assert!(hooks.has_task_completed() && hooks.has_agent_idle());
        let task = json!({ "id": "t2", "title": "Add parser", "status": "completed" });
        let reports = hooks.task_completed(&task, &workdir);
        let name = workdir.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            reports,
            vec![
                HookReport {
                    event: "task_completed",
                    ok: true,
                    text: format!("tests ok in {name} for t2"),
                },
                HookReport {
                    event: "task_completed",
                    ok: false,
                    text: "broken".to_string(),
                },
            ]
        );
        let payload: Value =
            serde_json::from_str(&fs::read_to_string(workdir.join("payload.json")).unwrap())
                .unwrap();
        assert_eq!(payload["event"], "task_completed");
        assert_eq!(payload["task"]["title"], "Add parser");
        assert_eq!(payload["workdir"], workdir.display().to_string());

        let reports = hooks.agent_idle(
            &json!({ "path": "/root/w", "status": "completed" }),
            &workdir,
        );
        assert_eq!(reports[0].text, "/root/w idle");
        assert!(Hooks::default().task_completed(&task, &workdir).is_empty());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn hook_output_is_capped() {
        if cfg!(windows) {
            return;
        }
        let hooks = Hooks::from_value(&json!({
            "agent_idle": [{ "command": "head -c 9000 /dev/zero | tr '\\0' x" }]
        }));
        let reports = hooks.agent_idle(&json!({ "path": "/root/w" }), Path::new("."));
        assert!(reports[0].text.starts_with("xxxx"));
        assert!(reports[0].text.ends_with("[hook output cut]"));
        assert!(reports[0].text.len() < 4100);
    }

    #[test]
    fn pre_tool_allows_on_zero_exit() {
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": "exit 0" }]
        }));
        assert!(hooks.pre_tool("bash", "{}", Path::new(".")).is_none());
    }
}
