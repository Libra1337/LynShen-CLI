use crate::{
    board::{Board, NewTask, Task},
    config::AgentsConfig,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_HARVEST_FILES: usize = 200;
/// The error a subagent's turn ends with when the team budget is used up.
pub(crate) const BUDGET_EXHAUSTED: &str = "budget_exhausted";
/// The main agent's path; its subagents are `/root/<task_name>`.
pub(crate) const ROOT_PATH: &str = "/root";
const MESSAGE_SUMMARY_CHARS: usize = 200;

/// An isolated working directory for one subagent. File-tool writes
/// (write/str_replace/hashline_edit/apply_patch) are confined to this root by
/// `write_target_escapes_root`. Its shell commands run in the sandbox with
/// this root as the only writable project directory (`ToolState::confined`);
/// without a sandbox they only start inside it. The parent brings the
/// changes back with `merge_agent`.
#[derive(Debug, Clone)]
pub(crate) struct SubagentWorkspace {
    pub root: PathBuf,
    /// True when the workspace is a detached git worktree of the parent repo
    /// (full file view, isolated writes); false for a fresh empty directory.
    pub from_git: bool,
    /// The commit the worktree started from; None for a plain directory.
    pub base: Option<String>,
}

/// Prepares the isolated workspace for a subagent under
/// `<parent_cwd>/.lynshen/agents/<task>-<millis>`. Inside a git repository this
/// is a detached `git worktree` (the child sees the committed tree and its
/// file-tool writes stay in the worktree); outside a repository it is a fresh
/// empty directory (the child sees nothing of the parent tree by default and
/// must be told which paths to inspect — the same bash caveat applies).
pub(crate) fn prepare_workspace(
    parent_cwd: &Path,
    task_name: &str,
) -> Result<SubagentWorkspace, String> {
    let root = parent_cwd
        .join(".lynshen")
        .join("agents")
        .join(format!("{task_name}-{}", now_ms()));
    if root.exists() {
        return Err(format!(
            "subagent workspace already exists: {}",
            root.display()
        ));
    }
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    if in_git_repository(parent_cwd) {
        let output = Command::new("git")
            .arg("-C")
            .arg(parent_cwd)
            .args(["worktree", "add", "--detach"])
            .arg(&root)
            .output()
            .map_err(|error| format!("failed to run git worktree add: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git worktree add failed for subagent workspace {}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let base = git_text(&root, &["rev-parse", "HEAD"])?;
        Ok(SubagentWorkspace {
            root,
            from_git: true,
            base: Some(base),
        })
    } else {
        std::fs::create_dir_all(&root)
            .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
        Ok(SubagentWorkspace {
            root,
            from_git: false,
            base: None,
        })
    }
}

fn in_git_repository(cwd: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Files the subagent changed inside its workspace, for the parent to harvest.
/// Worktrees ask git (modified + untracked, gitignore-aware); plain directories
/// list every file (the child started from an empty dir). Capped to keep the
/// tool result bounded.
pub(crate) fn changed_files(workspace: &SubagentWorkspace) -> Vec<String> {
    if workspace.from_git {
        let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(&workspace.root)
            .args(["status", "--porcelain", "--no-renames"])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.get(3..).map(str::to_string))
            .take(MAX_HARVEST_FILES)
            .collect()
    } else {
        let mut files = Vec::new();
        collect_files(
            &workspace.root,
            &workspace.root,
            &mut files,
            MAX_HARVEST_FILES,
        );
        files
    }
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>, limit: usize) {
    if files.len() >= limit {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(root, &path, files, limit);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
        if files.len() >= limit {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Interrupted,
    Closed,
    /// Stopped before a model request: the turn's token budget was used up.
    BudgetExhausted,
    /// merge_agent apply found conflicts; the worktree is kept.
    Conflict,
    Merged,
    Discarded,
}

impl SubagentStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Interrupted => "interrupted",
            Self::Closed => "closed",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Conflict => "conflict",
            Self::Merged => "merged",
            Self::Discarded => "discarded",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "errored" => Self::Errored,
            "interrupted" => Self::Interrupted,
            "closed" => Self::Closed,
            "budget_exhausted" => Self::BudgetExhausted,
            "conflict" => Self::Conflict,
            "merged" => Self::Merged,
            "discarded" => Self::Discarded,
            _ => return None,
        })
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    fn is_final(&self) -> bool {
        !self.is_live()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentSpawn {
    pub parent_path: String,
    pub task_name: String,
    pub message: String,
    pub model: String,
    pub reasoning_effort: String,
    pub depth: u64,
    /// The parent's spawn_agent call id (front-ends match the card to it).
    pub tool_use_id: String,
    pub role: Option<String>,
    /// The plan step this agent works on.
    pub plan_step: Option<String>,
    /// It keeps running after its parent's turn ends.
    pub background: bool,
    /// best-of-N: the task name spawn_agent got, and this attempt's number
    /// from 1 (the agent is `<group>_a<attempt>`).
    pub attempt_group: Option<String>,
    pub attempt: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSlot {
    pub path: String,
    pub interrupt_flag: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentRunResult {
    pub summary: String,
    pub partial_output: String,
    pub tool_calls: u64,
    pub tools_used: Vec<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub model: String,
    /// Isolated workspace the agent wrote into (empty when spawn failed before
    /// workspace creation). The parent merges changes from here.
    pub workdir: String,
    /// Workspace-relative paths the agent created or modified.
    pub files_changed: Vec<String>,
}

/// What the team did, for the front-ends (drained by the core).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeamEvent {
    Lifecycle {
        path: String,
        status: String,
        message: String,
    },
    Message {
        from: String,
        to: String,
        summary: String,
    },
    Merge {
        target: String,
        action: String,
        ok: bool,
        files: Vec<String>,
        conflicts: Vec<String>,
        error: Option<String>,
    },
    Budget {
        used: u64,
        limit: u64,
    },
    /// The task board changed (the core sends the whole board).
    Board,
}

/// What a piece of agent mail is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailKind {
    /// send_message, or the budget warning.
    Message,
    /// A background subagent's result for the main agent; it wakes an idle
    /// main agent.
    Result { status: String },
    /// A task_completed or agent_idle hook's output for the main agent; it
    /// wakes an idle main agent too.
    Hook { ok: bool },
}

/// A message waiting for an agent's next model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboxMessage {
    pub from: String,
    pub text: String,
    pub kind: MailKind,
}

impl InboxMessage {
    pub(crate) fn message(from: &str, text: &str) -> Self {
        Self {
            from: from.to_string(),
            text: text.to_string(),
            kind: MailKind::Message,
        }
    }

    /// Mail that starts a turn of an idle main agent.
    pub(crate) fn wakes(&self) -> bool {
        self.kind != MailKind::Message
    }

    /// The text the recipient's model reads.
    pub(crate) fn model_text(&self) -> String {
        match &self.kind {
            MailKind::Message => format!(
                "<subagent_message from=\"{}\">\n{}\n</subagent_message>",
                self.from, self.text
            ),
            MailKind::Result { status } => format!(
                "<subagent_result path=\"{}\" status=\"{status}\">\n{}\n</subagent_result>",
                self.from, self.text
            ),
            MailKind::Hook { ok } => format!(
                "<hook_result hook=\"{}\" ok=\"{ok}\">\n{}\n</hook_result>",
                self.from.trim_start_matches("hook:"),
                self.text
            ),
        }
    }

    fn wait_json(&self) -> Value {
        let mut value = json!({ "from": self.from, "message": self.text });
        match &self.kind {
            MailKind::Message => {}
            MailKind::Result { status } => {
                value["kind"] = json!("result");
                value["status"] = json!(status);
            }
            MailKind::Hook { ok } => {
                value["kind"] = json!("hook");
                value["ok"] = json!(ok);
            }
        }
        value
    }
}

/// Label, model, spawn call, role, plan step and kind of an agent.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentInfo {
    pub label: String,
    pub model: String,
    pub tool_use_id: String,
    pub role: Option<String>,
    pub plan_step: Option<String>,
    pub background: bool,
    pub attempt_group: Option<String>,
    pub attempt: Option<u64>,
}

/// A worktree (or plain workspace) waiting for merge_agent.
#[derive(Debug, Clone)]
struct PendingWorktree {
    workspace: SubagentWorkspace,
    /// The directory the agent was spawned from: changes are applied here.
    parent_cwd: PathBuf,
    /// A merge is running.
    busy: bool,
}

/// Worktrees not yet merged or discarded (so a later turn or a client can
/// still merge them), and the main agent's plan, which `agents.fanout =
/// plan` checks spawns against.
#[derive(Clone, Default)]
pub(crate) struct TeamShared(Arc<Mutex<TeamState>>);

#[derive(Default)]
struct TeamState {
    worktrees: BTreeMap<String, PendingWorktree>,
    plan_approved: bool,
    plan_steps: Vec<String>,
}

impl TeamShared {
    pub(crate) fn set_plan(&self, approved: bool, steps: Vec<String>) {
        let mut state = self.0.lock().unwrap();
        state.plan_approved = approved;
        state.plan_steps = steps;
    }

    pub(crate) fn set_plan_steps(&self, steps: Vec<String>) {
        self.0.lock().unwrap().plan_steps = steps;
    }

    /// Whether the latest proposed plan was approved, and the update_plan steps.
    pub(crate) fn plan(&self) -> (bool, Vec<String>) {
        let state = self.0.lock().unwrap();
        (state.plan_approved, state.plan_steps.clone())
    }

    fn has_worktree(&self, path: &str) -> bool {
        self.0.lock().unwrap().worktrees.contains_key(path)
    }

    fn insert(&self, path: &str, worktree: PendingWorktree) {
        self.0
            .lock()
            .unwrap()
            .worktrees
            .insert(path.to_string(), worktree);
    }

    fn get(&self, path: &str) -> Option<PendingWorktree> {
        self.0.lock().unwrap().worktrees.get(path).cloned()
    }

    fn snapshot(&self) -> Vec<(String, PendingWorktree)> {
        let state = self.0.lock().unwrap();
        state
            .worktrees
            .iter()
            .map(|(path, worktree)| (path.clone(), worktree.clone()))
            .collect()
    }

    /// Takes the worktree of `path` for a merge; Err when there is none or a
    /// merge of it is already running.
    fn begin_merge(&self, path: &str) -> Result<PendingWorktree, String> {
        let mut state = self.0.lock().unwrap();
        let worktree = state
            .worktrees
            .get_mut(path)
            .ok_or_else(|| format!("no worktree to merge for {path}"))?;
        if worktree.busy {
            return Err(format!("a merge of {path} is already running"));
        }
        worktree.busy = true;
        Ok(worktree.clone())
    }

    fn end_merge(&self, path: &str, done: bool) {
        let mut state = self.0.lock().unwrap();
        if done {
            state.worktrees.remove(path);
        } else if let Some(worktree) = state.worktrees.get_mut(path) {
            worktree.busy = false;
        }
    }
}

/// A finished subagent ready to run again (`resume_agent`).
pub(crate) struct Resume {
    pub slot: SubagentSlot,
    pub task_name: String,
    pub spec: crate::llm::ChildSpec,
    /// Its conversation when it stopped.
    pub context: Vec<Value>,
    /// Its worktree, when it still exists.
    pub workspace: Option<SubagentWorkspace>,
}

/// The session's team: every subagent of the session (the 24 most recent
/// finished ones are kept), their mail, the token budget of the current
/// window, the task board and the worktrees to merge. One per session; the
/// core starts each main turn with `begin_turn`.
#[derive(Clone, Default)]
pub(crate) struct SubagentManager {
    inner: Arc<SubagentInner>,
}

#[derive(Default)]
struct SubagentInner {
    state: Mutex<SubagentRegistry>,
    changed: Condvar,
    config: Mutex<AgentsConfig>,
    shared: TeamShared,
}

#[derive(Default)]
struct SubagentRegistry {
    agents: BTreeMap<String, SubagentRecord>,
    events: VecDeque<TeamEvent>,
    /// The user's messages for the running main turn (`steer`), read before
    /// its next model request; kept apart from agent mail.
    user_inbox: VecDeque<String>,
    /// Agent mail by recipient path (`/root` is the main agent), read
    /// before the recipient's next model request or by its wait_agent.
    inboxes: BTreeMap<String, VecDeque<InboxMessage>>,
    /// Token usage of subagents that reached a final state, awaiting fold-in to
    /// the parent's cumulative totals. Drained once via `drain_finished_usage`.
    finished_usage: Vec<SubagentRunResult>,
    /// Input + output tokens of every subagent since the budget window
    /// opened (a user's turn; a turn the engine starts by itself continues
    /// the window).
    tokens_used: u64,
    /// The 80% "wrap up" message went out.
    budget_warned: bool,
    /// The tenth of the budget the last `Budget` event reported.
    budget_tenth: Option<u64>,
    board: Board,
    /// Bumped on every change the session persists (the board, worktree
    /// agents): the core saves when it moves.
    revision: u64,
}

struct SubagentRecord {
    path: String,
    parent_path: String,
    task_name: String,
    message: String,
    model: String,
    reasoning_effort: String,
    depth: u64,
    status: SubagentStatus,
    interrupt_flag: Arc<AtomicBool>,
    result: Option<SubagentRunResult>,
    error: Option<String>,
    started_at_ms: u64,
    completed_at_ms: Option<u64>,
    /// Workspace path once prepared; None until then.
    workdir: Option<String>,
    /// It writes in its own worktree (or plain workspace).
    isolated: bool,
    tool_use_id: String,
    role: Option<String>,
    plan_step: Option<String>,
    background: bool,
    attempt_group: Option<String>,
    attempt: Option<u64>,
    /// How it was started, so resume_agent can run it again.
    spec: Option<crate::llm::ChildSpec>,
    /// Its conversation when it last stopped.
    context: Option<Vec<Value>>,
    /// What it is doing, for the front-ends' agent trace.
    trace: crate::subagent_trace::SubagentTrace,
}

/// Finished agents kept for agent_runs, transcripts and resume_agent.
const MAX_KEPT_AGENTS: usize = 24;

impl SubagentManager {
    pub(crate) fn new(config: AgentsConfig, shared: TeamShared) -> Self {
        Self {
            inner: Arc::new(SubagentInner {
                config: Mutex::new(config),
                shared,
                ..SubagentInner::default()
            }),
        }
    }

    pub(crate) fn config(&self) -> AgentsConfig {
        self.inner.config.lock().unwrap().clone()
    }

    pub(crate) fn shared(&self) -> &TeamShared {
        &self.inner.shared
    }

    /// A main turn starts: the settings it runs with. `new_window` opens a
    /// new budget window (every turn but one the engine starts by itself
    /// for a background result). Old finished agents are forgotten.
    pub(crate) fn begin_turn(&self, config: AgentsConfig, new_window: bool) {
        *self.inner.config.lock().unwrap() = config;
        let mut state = self.inner.state.lock().unwrap();
        if new_window {
            state.tokens_used = 0;
            state.budget_warned = false;
            state.budget_tenth = None;
            for inbox in state.inboxes.values_mut() {
                inbox.retain(|message| message.from != "team_budget");
            }
        }
        let mut finished: Vec<(u64, String)> = state
            .agents
            .values()
            .filter(|agent| agent.status.is_final() && !self.inner.shared.has_worktree(&agent.path))
            // A running background agent's parent stays: it tells what the
            // agent belongs to.
            .filter(|agent| {
                let prefix = format!("{}/", agent.path);
                !state
                    .agents
                    .values()
                    .any(|other| other.status.is_live() && other.path.starts_with(&prefix))
            })
            .map(|agent| {
                (
                    agent.completed_at_ms.unwrap_or(agent.started_at_ms),
                    agent.path.clone(),
                )
            })
            .collect();
        if finished.len() > MAX_KEPT_AGENTS {
            finished.sort();
            let excess = finished.len() - MAX_KEPT_AGENTS;
            for (_, path) in finished.drain(..excess) {
                state.agents.remove(&path);
                state.inboxes.remove(&path);
            }
        }
    }

    pub(crate) fn reserve_spawn(&self, spawn: SubagentSpawn) -> Result<SubagentSlot, String> {
        self.reserve_spawns(vec![spawn])
            .map(|mut slots| slots.remove(0))
    }

    /// Reserves every spawn or none (best-of-N starts its attempts
    /// together). A finished agent of the same path is replaced; a running
    /// one, or a worktree still to merge, refuses.
    pub(crate) fn reserve_spawns(
        &self,
        spawns: Vec<SubagentSpawn>,
    ) -> Result<Vec<SubagentSlot>, String> {
        let config = self.config();
        for spawn in &spawns {
            validate_task_name(&spawn.task_name)?;
            if spawn.depth > config.max_depth {
                return Err("agent depth limit reached. Solve the task yourself.".to_string());
            }
        }
        let mut state = self.inner.state.lock().unwrap();
        for spawn in &spawns {
            let path = child_path(&spawn.parent_path, &spawn.task_name);
            if state
                .agents
                .get(&path)
                .is_some_and(|agent| agent.status.is_live())
            {
                return Err(format!("agent already exists: {path}"));
            }
            if self.inner.shared.has_worktree(&path) {
                return Err(format!(
                    "{path} still has a worktree to merge; merge_agent it first or use another task_name"
                ));
            }
        }
        let live = state
            .agents
            .values()
            .filter(|agent| agent.status.is_live())
            .count();
        if live + spawns.len() > config.max_live {
            return Err(format!(
                "too many live agents ({}); wait for or close an agent first",
                config.max_live
            ));
        }
        if self.budget_exhausted_in(&state) {
            return Err(format!(
                "the subagents' token budget for this turn is used up ({} of {} tokens); do the rest yourself",
                state.tokens_used, config.turn_token_budget
            ));
        }
        let mut slots = Vec::new();
        for spawn in spawns {
            let path = child_path(&spawn.parent_path, &spawn.task_name);
            let interrupt_flag = Arc::new(AtomicBool::new(false));
            let trace = crate::subagent_trace::SubagentTrace::new(&spawn.message);
            state.inboxes.remove(&path);
            state.agents.insert(
                path.clone(),
                SubagentRecord {
                    path: path.clone(),
                    parent_path: spawn.parent_path,
                    task_name: spawn.task_name,
                    message: spawn.message,
                    model: spawn.model,
                    reasoning_effort: spawn.reasoning_effort,
                    depth: spawn.depth,
                    status: SubagentStatus::Pending,
                    interrupt_flag: Arc::clone(&interrupt_flag),
                    result: None,
                    error: None,
                    started_at_ms: now_ms(),
                    completed_at_ms: None,
                    workdir: None,
                    isolated: false,
                    trace,
                    tool_use_id: spawn.tool_use_id,
                    role: spawn.role,
                    plan_step: spawn.plan_step,
                    background: spawn.background,
                    attempt_group: spawn.attempt_group,
                    attempt: spawn.attempt,
                    spec: None,
                    context: None,
                },
            );
            state.push_event(&path, "pending", "reserved");
            slots.push(SubagentSlot {
                path,
                interrupt_flag,
            });
        }
        self.note_budget(&mut state);
        self.inner.changed.notify_all();
        Ok(slots)
    }

    pub(crate) fn set_workdir(&self, path: &str, workdir: &str) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(agent) = state.agents.get_mut(path) {
            agent.workdir = Some(workdir.to_string());
        }
    }

    /// How `path` was started, for resume_agent.
    pub(crate) fn set_spec(&self, path: &str, spec: crate::llm::ChildSpec) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(agent) = state.agents.get_mut(path) {
            agent.spec = Some(spec);
        }
    }

    /// The conversation `path` ended with, for resume_agent.
    pub(crate) fn save_context(&self, path: &str, items: Vec<Value>) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(agent) = state.agents.get_mut(path) {
            agent.context = Some(items);
        }
    }

    /// Records the isolated workspace of `path`, which merge_agent brings
    /// back into `parent_cwd` (or discards).
    pub(crate) fn register_workspace(
        &self,
        path: &str,
        workspace: &SubagentWorkspace,
        parent_cwd: &Path,
    ) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(agent) = state.agents.get_mut(path) {
            agent.workdir = Some(workspace.root.display().to_string());
            agent.isolated = true;
        }
        self.inner.shared.insert(
            path,
            PendingWorktree {
                workspace: workspace.clone(),
                parent_cwd: parent_cwd.to_path_buf(),
                busy: false,
            },
        );
        state.revision += 1;
    }

    pub(crate) fn mark_running(&self, path: &str) {
        let mut state = self.inner.state.lock().unwrap();
        let mut event = None;
        if let Some(agent) = state.agents.get_mut(path) {
            if agent.status == SubagentStatus::Pending {
                agent.status = SubagentStatus::Running;
                event = Some(("running", "started"));
            }
        }
        if let Some((status, message)) = event {
            state.push_event(path, status, message);
        }
        self.inner.changed.notify_all();
    }

    pub(crate) fn finish_ok(&self, path: &str, result: SubagentRunResult) {
        self.finish(path, result, None);
    }

    pub(crate) fn finish_err(&self, path: &str, error: String, partial: SubagentRunResult) {
        self.finish(path, partial, Some(error));
    }

    /// An agent's run ended. Its foreground subagents are closed; a
    /// background agent of the main agent leaves its result in the main
    /// agent's mail, which wakes an idle main agent.
    fn finish(&self, path: &str, result: SubagentRunResult, error: Option<String>) {
        let mut state = self.inner.state.lock().unwrap();
        let Some(agent) = state.agents.get_mut(path) else {
            self.inner.changed.notify_all();
            return;
        };
        if agent.status == SubagentStatus::Closed {
            self.inner.changed.notify_all();
            return;
        }
        let message = match error {
            None => {
                agent.status = SubagentStatus::Completed;
                agent.trace.finish("Done");
                agent.error = None;
                "finished".to_string()
            }
            Some(error) => {
                agent.status =
                    if agent.interrupt_flag.load(Ordering::SeqCst) || error == "interrupted" {
                        SubagentStatus::Interrupted
                    } else if error == BUDGET_EXHAUSTED {
                        SubagentStatus::BudgetExhausted
                    } else {
                        SubagentStatus::Errored
                    };
                let error = if agent.status == SubagentStatus::BudgetExhausted {
                    "stopped: the turn's subagent token budget is used up".to_string()
                } else {
                    error
                };
                agent.trace.finish(match agent.status {
                    SubagentStatus::Interrupted => "Interrupted",
                    SubagentStatus::BudgetExhausted => "Budget used up",
                    _ => "Failed",
                });
                agent.error = Some(error.clone());
                error
            }
        };
        agent.completed_at_ms = Some(now_ms());
        agent.result = Some(result.clone());
        let status = agent.status;
        let wake = agent.background
            && agent.parent_path == ROOT_PATH
            && status != SubagentStatus::Interrupted;
        let report = match &agent.error {
            None => result.summary.clone(),
            Some(error) if result.partial_output.trim().is_empty() => error.clone(),
            Some(error) => format!("{error}\n\nPartial output:\n{}", result.partial_output),
        };
        state.finished_usage.push(result);
        state.push_event(path, status.as_str(), &message);
        let children = foreground_live(&state, path);
        close_paths(&mut state, children, "parent agent finished");
        if wake {
            state
                .inboxes
                .entry(ROOT_PATH.to_string())
                .or_default()
                .push_back(InboxMessage {
                    from: path.to_string(),
                    text: report,
                    kind: MailKind::Result {
                        status: status.as_str().to_string(),
                    },
                });
        }
        self.inner.changed.notify_all();
    }

    /// Queues `message` for a running subagent: a child of the requester,
    /// a sibling (by task name or path), or the requester's parent when
    /// `target` is "parent". It is read before the recipient's next model
    /// request; a parent blocked in wait_agent gets it at once.
    pub(crate) fn send_message(
        &self,
        requester_path: &str,
        target: &str,
        message: &str,
    ) -> Result<Value, String> {
        let mut state = self.inner.state.lock().unwrap();
        let to = if target.trim() == "parent" {
            state
                .agents
                .get(requester_path)
                .map(|agent| agent.parent_path.clone())
                .ok_or_else(|| "the main agent has no parent".to_string())?
        } else {
            resolve_message_target(&state, requester_path, target)?
        };
        if to == requester_path {
            return Err("that is you".to_string());
        }
        if let Some(agent) = state.agents.get_mut(&to) {
            if agent.status.is_final() {
                return Err(format!("agent is not running: {to}"));
            }
            agent.trace.note(message);
        }
        state
            .inboxes
            .entry(to.clone())
            .or_default()
            .push_back(InboxMessage::message(requester_path, message));
        if state.agents.contains_key(&to) {
            state.push_event(&to, "message", "queued message");
        }
        state.events.push_back(TeamEvent::Message {
            from: requester_path.to_string(),
            to: to.clone(),
            summary: summarize(message),
        });
        self.inner.changed.notify_all();
        Ok(json!({
            "target": to,
            "delivered": true,
            "status": "queued"
        }))
    }

    /// A hook's output for the main agent (and an `agent_message` from
    /// `hook:<event>` for the front-ends).
    pub(crate) fn report_hook(&self, report: crate::hooks::HookReport) {
        let mut state = self.inner.state.lock().unwrap();
        let from = format!("hook:{}", report.event);
        state.events.push_back(TeamEvent::Message {
            from: from.clone(),
            to: ROOT_PATH.to_string(),
            summary: summarize(&report.text),
        });
        state
            .inboxes
            .entry(ROOT_PATH.to_string())
            .or_default()
            .push_back(InboxMessage {
                from,
                text: report.text,
                kind: MailKind::Hook { ok: report.ok },
            });
        self.inner.changed.notify_all();
    }

    /// The main agent's mail, drained, when some of it wakes an idle main
    /// agent (a background result, a hook's output); empty otherwise.
    pub(crate) fn take_wake(&self) -> Vec<InboxMessage> {
        let mut state = self.inner.state.lock().unwrap();
        match state.inboxes.get_mut(ROOT_PATH) {
            Some(inbox) if inbox.iter().any(InboxMessage::wakes) => inbox.drain(..).collect(),
            _ => Vec::new(),
        }
    }

    /// A user message for the running main turn: the model reads it before
    /// its next request, while the turn (and its tools) keep running.
    pub(crate) fn steer_main(&self, message: &str) {
        let mut state = self.inner.state.lock().unwrap();
        state.user_inbox.push_back(message.to_string());
    }

    /// Messages the user steered into the main turn, oldest first.
    pub(crate) fn drain_user_inbox(&self) -> Vec<String> {
        let mut state = self.inner.state.lock().unwrap();
        state.user_inbox.drain(..).collect()
    }

    /// Steered messages the turn ended before reading.
    pub(crate) fn take_unread_steers(&self) -> Vec<String> {
        self.drain_user_inbox()
    }

    /// Agent mail for `path` (the main agent is `/root`), oldest first.
    pub(crate) fn drain_messages(&self, path: &str) -> Vec<InboxMessage> {
        let mut state = self.inner.state.lock().unwrap();
        state
            .inboxes
            .get_mut(path)
            .map(|inbox| inbox.drain(..).collect())
            .unwrap_or_default()
    }

    /// Stops `target` (and the foreground agents it started).
    pub(crate) fn close_agent(&self, requester_path: &str, target: &str) -> Result<Value, String> {
        let target = self.resolve_existing_target(requester_path, target)?;
        let mut state = self.inner.state.lock().unwrap();
        let agent = state
            .agents
            .get(&target)
            .ok_or_else(|| format!("agent not found: {target}"))?;
        let previous = status_json(agent);
        if agent.status.is_live() {
            let mut paths = vec![target.clone()];
            paths.extend(foreground_live(&state, &target));
            close_paths(&mut state, paths, "close requested");
        }
        self.inner.changed.notify_all();
        Ok(json!({
            "target": target,
            "previous_status": previous,
            "closed": true
        }))
    }

    /// The main turn was interrupted: its foreground agents stop; background
    /// agents (and what they started) keep running.
    pub(crate) fn close_all(&self) {
        self.close_all_with_message("parent interrupted");
    }

    /// The main turn ended: its foreground agents stop.
    pub(crate) fn close_all_with_message(&self, message: &str) {
        let mut state = self.inner.state.lock().unwrap();
        let paths = foreground_live(&state, ROOT_PATH);
        close_paths(&mut state, paths, message);
        self.inner.changed.notify_all();
    }

    /// Every live agent stops, background ones too (the session ends).
    pub(crate) fn close_everything(&self, message: &str) {
        let mut state = self.inner.state.lock().unwrap();
        let paths = state
            .agents
            .values()
            .filter(|agent| agent.status.is_live())
            .map(|agent| agent.path.clone())
            .collect();
        close_paths(&mut state, paths, message);
        self.inner.changed.notify_all();
    }

    pub(crate) fn list_agents(&self, requester_path: &str, path_prefix: Option<&str>) -> Value {
        let state = self.inner.state.lock().unwrap();
        let prefix = path_prefix
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| self.resolve_path_prefix(requester_path, value));
        let agents = state
            .agents
            .values()
            .filter(|agent| {
                prefix
                    .as_ref()
                    .map(|prefix| agent.path.starts_with(prefix))
                    .unwrap_or(true)
            })
            .map(agent_json)
            .collect::<Vec<_>>();
        json!({ "agents": agents })
    }

    /// Waits until one of `targets` finishes (every attempt, for a best-of-N
    /// group name), or without targets until one of the requester's running
    /// subagents finishes, or until a message for the requester arrives.
    /// Messages are handed over in `messages` and not read again before the
    /// next model request. A group whose attempts all finished comes with
    /// `attempts`: each attempt's diff stats, to compare them.
    pub(crate) fn wait_agents(
        &self,
        requester_path: &str,
        targets: Vec<String>,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        let (singles, groups) = {
            let state = self.inner.state.lock().unwrap();
            let mut singles = Vec::new();
            let mut groups: Vec<(String, Vec<String>)> = Vec::new();
            if targets.is_empty() {
                singles = state
                    .agents
                    .values()
                    .filter(|agent| agent.parent_path == requester_path && agent.status.is_live())
                    .map(|agent| agent.path.clone())
                    .collect();
            }
            for target in &targets {
                match resolve_existing_target_in_state(&state, requester_path, target) {
                    Ok(path) => singles.push(path),
                    Err(error) => {
                        let name = target.trim().rsplit('/').next().unwrap_or_default();
                        let members = group_members(&state, requester_path, name);
                        if members.is_empty() {
                            return Err(error);
                        }
                        groups.push((name.to_string(), members));
                    }
                }
            }
            (singles, groups)
        };
        let nothing_to_wait = targets.is_empty() && singles.is_empty();
        let done = |state: &SubagentRegistry, path: &String| {
            state
                .agents
                .get(path)
                .is_none_or(|agent| agent.status.is_final())
        };
        let woken = |state: &SubagentRegistry| {
            nothing_to_wait
                || singles.iter().any(|path| done(state, path))
                || groups
                    .iter()
                    .any(|(_, members)| members.iter().all(|path| done(state, path)))
                || state
                    .inboxes
                    .get(requester_path)
                    .is_some_and(|inbox| !inbox.is_empty())
        };
        let deadline = Duration::from_millis(timeout_ms);
        let started = SystemTime::now();
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if woken(&state) {
                break;
            }
            let elapsed = started.elapsed().unwrap_or_default();
            if elapsed >= deadline {
                break;
            }
            let remaining = deadline.saturating_sub(elapsed);
            let (next_state, _) = self.inner.changed.wait_timeout(state, remaining).unwrap();
            state = next_state;
        }

        let woke = woken(&state);
        let mut shown = singles.clone();
        if nothing_to_wait {
            shown = state
                .agents
                .values()
                .filter(|agent| agent.parent_path == requester_path)
                .map(|agent| agent.path.clone())
                .collect();
        }
        for (_, members) in &groups {
            shown.extend(members.iter().cloned());
        }
        let statuses = wait_statuses(&state, &shown);
        // A result the statuses carry is not handed over twice.
        let reported: Vec<String> = shown
            .iter()
            .filter(|path| done(&state, path))
            .cloned()
            .collect();
        let messages: Vec<Value> = state
            .inboxes
            .get_mut(requester_path)
            .map(|inbox| inbox.drain(..).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|message| {
                !(matches!(message.kind, MailKind::Result { .. })
                    && reported.contains(&message.from))
            })
            .map(|message| message.wait_json())
            .collect();
        // Each finished group's attempts, compared after the lock is gone
        // (their diffs run git).
        let finished_groups: Vec<(String, Vec<Value>)> = groups
            .iter()
            .chain(attempt_groups_of(&state, &shown).iter())
            .filter(|(_, members)| members.iter().all(|path| done(&state, path)))
            .map(|(name, members)| {
                let rows = members
                    .iter()
                    .filter_map(|path| state.agents.get(path))
                    .map(|agent| {
                        json!({
                            "path": agent.path,
                            "attempt": agent.attempt,
                            "state": agent.status.as_str(),
                            "summary": agent
                                .result
                                .as_ref()
                                .map(|result| summarize(&result.summary))
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
                (name.clone(), rows)
            })
            .collect();
        drop(state);
        let mut result = json!({
            "status": statuses,
            "timed_out": !woke,
        });
        if !messages.is_empty() {
            result["messages"] = Value::Array(messages);
        }
        let mut compared = serde_json::Map::new();
        for (name, mut attempts) in finished_groups {
            if compared.contains_key(&name) {
                continue;
            }
            for row in &mut attempts {
                let path = row["path"].as_str().unwrap_or_default().to_string();
                let Some(pending) = self.inner.shared.get(&path) else {
                    continue;
                };
                match workspace_changes(&pending.workspace) {
                    Ok(changes) => {
                        row["files"] = json!(changes
                            .files
                            .iter()
                            .map(|file| file.path.clone())
                            .collect::<Vec<_>>());
                        row["added"] = json!(changes.added);
                        row["removed"] = json!(changes.removed);
                    }
                    Err(error) => row["diff_error"] = json!(error),
                }
            }
            compared.insert(name, Value::Array(attempts));
        }
        if !compared.is_empty() {
            result["attempts"] = Value::Object(compared);
        }
        Ok(result)
    }

    /// Records one event of a running agent's turn in its trace; model usage
    /// also counts against the turn's token budget.
    pub(crate) fn record(&self, path: &str, event: &crate::llm::StreamEvent) {
        let mut state = self.inner.state.lock().unwrap();
        if let crate::llm::StreamEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } = event
        {
            state.tokens_used = state
                .tokens_used
                .saturating_add(input_tokens.saturating_add(*output_tokens));
            self.note_budget(&mut state);
        }
        if let Some(agent) = state.agents.get_mut(path) {
            agent.trace.record(event);
        }
    }

    /// The turn's budget is used up: running subagents stop before their
    /// next model request and spawn_agent refuses.
    pub(crate) fn budget_exhausted(&self) -> bool {
        let state = self.inner.state.lock().unwrap();
        self.budget_exhausted_in(&state)
    }

    fn budget_exhausted_in(&self, state: &SubagentRegistry) -> bool {
        let limit = self.config().turn_token_budget;
        limit > 0 && state.tokens_used >= limit
    }

    /// Sends the 80% "wrap up" message once, and a `Budget` event each time
    /// usage reaches another tenth of the budget. Nothing without a budget.
    fn note_budget(&self, state: &mut SubagentRegistry) {
        let limit = self.config().turn_token_budget;
        if limit == 0 {
            return;
        }
        let used = state.tokens_used;
        if !state.budget_warned && used.saturating_mul(10) >= limit.saturating_mul(8) {
            state.budget_warned = true;
            let live: Vec<String> = state
                .agents
                .values()
                .filter(|agent| agent.status.is_live())
                .map(|agent| agent.path.clone())
                .collect();
            for path in live {
                state.inboxes.entry(path).or_default().push_back(InboxMessage::message(
                    "team_budget",
                    &format!(
                        "Budget nearly used: this turn's subagents have used {used} of {limit} tokens. Wrap up now: finish the current step and report."
                    ),
                ));
            }
            state
                .inboxes
                .entry(ROOT_PATH.to_string())
                .or_default()
                .push_back(InboxMessage::message(
                    "team_budget",
                    &format!(
                        "Budget nearly used: your subagents have used {used} of {limit} tokens this turn. Start no new agents; collect their results and finish."
                    ),
                ));
            self.inner.changed.notify_all();
        }
        let tenth = (used.saturating_mul(10) / limit).min(10);
        if state.budget_tenth != Some(tenth) {
            state.budget_tenth = Some(tenth);
            state.events.push_back(TeamEvent::Budget { used, limit });
        }
    }

    /// `resume_agent`: makes the requester's finished subagent `target`
    /// pending again with `message` noted in its trace. The caller starts
    /// it on its saved conversation.
    pub(crate) fn reserve_resume(
        &self,
        requester_path: &str,
        target: &str,
        message: &str,
    ) -> Result<Resume, String> {
        let target = target.trim();
        let path = if target.starts_with('/') {
            target.to_string()
        } else {
            child_path(requester_path, target)
        };
        if !path.starts_with(&format!("{}/", requester_path.trim_end_matches('/'))) {
            return Err(format!("{path} is not one of your subagents"));
        }
        let config = self.config();
        let mut state = self.inner.state.lock().unwrap();
        let live = state
            .agents
            .values()
            .filter(|agent| agent.status.is_live())
            .count();
        let budget_exhausted = self.budget_exhausted_in(&state);
        let agent = state
            .agents
            .get_mut(&path)
            .ok_or_else(|| format!("agent not found: {path}"))?;
        if agent.status.is_live() {
            return Err(format!(
                "{path} is still running; use send_message to tell it more"
            ));
        }
        let (Some(spec), Some(context)) = (agent.spec.clone(), agent.context.clone()) else {
            return Err(format!(
                "{path} has no saved conversation (it ran before the engine restarted); spawn a new agent"
            ));
        };
        if live >= config.max_live {
            return Err(format!(
                "too many live agents ({}); wait for or close an agent first",
                config.max_live
            ));
        }
        if budget_exhausted {
            return Err(
                "the subagents' token budget for this turn is used up; do the rest yourself"
                    .to_string(),
            );
        }
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        agent.status = SubagentStatus::Pending;
        agent.interrupt_flag = Arc::clone(&interrupt_flag);
        agent.started_at_ms = now_ms();
        agent.completed_at_ms = None;
        agent.error = None;
        agent.result = None;
        agent.trace.resume(message);
        let task_name = agent.task_name.clone();
        state.push_event(&path, "pending", "resumed");
        self.inner.changed.notify_all();
        drop(state);
        let workspace = self
            .inner
            .shared
            .get(&path)
            .map(|pending| pending.workspace)
            .filter(|workspace| workspace.root.is_dir());
        Ok(Resume {
            slot: SubagentSlot {
                path,
                interrupt_flag,
            },
            task_name,
            spec,
            context,
            workspace,
        })
    }

    /// `merge_agent`: brings the worktree of the requester's subagent
    /// `target` into the directory it was spawned from (`apply`), or deletes
    /// it (`discard`). A conflict writes nothing and keeps the worktree.
    /// Always queues a `Merge` event.
    pub(crate) fn merge_agent(
        &self,
        requester_path: &str,
        target: &str,
        action: &str,
    ) -> Result<Value, String> {
        let target = target.trim();
        let path = if target.starts_with('/') {
            target.to_string()
        } else {
            child_path(requester_path, target)
        };
        let outcome = self.merge_path(requester_path, &path, action);
        let mut state = self.inner.state.lock().unwrap();
        let (ok, files, conflicts, error) = match &outcome {
            Ok(merge) => (
                merge.conflicts.is_empty(),
                merge.files.clone(),
                merge.conflicts.clone(),
                None,
            ),
            Err(error) => (false, Vec::new(), Vec::new(), Some(error.clone())),
        };
        if let Ok(merge) = &outcome {
            let (status, message) = if !merge.conflicts.is_empty() {
                (
                    SubagentStatus::Conflict,
                    format!("conflicts in {}", merge.conflicts.join(", ")),
                )
            } else if action == "discard" {
                (SubagentStatus::Discarded, "worktree discarded".to_string())
            } else {
                let count = merge.files.len();
                let files = if count == 1 { "file" } else { "files" };
                (SubagentStatus::Merged, format!("applied {count} {files}"))
            };
            if let Some(agent) = state.agents.get_mut(&path) {
                agent.status = status;
            }
            state.push_event(&path, status.as_str(), &message);
        }
        state.events.push_back(TeamEvent::Merge {
            target: path.clone(),
            action: action.to_string(),
            ok,
            files: files.clone(),
            conflicts: conflicts.clone(),
            error,
        });
        self.inner.changed.notify_all();
        drop(state);
        let merge = outcome?;
        let mut result = json!({ "target": path, "action": action, "ok": ok });
        if action == "apply" {
            result["files"] = json!(files);
        }
        if !conflicts.is_empty() {
            result["conflicts"] = json!(conflicts);
            result["note"] = json!(format!(
                "Nothing was written. The agent's versions are in {}. Edit the conflicting files yourself, then apply again or discard.",
                merge.workdir
            ));
        }
        Ok(result)
    }

    fn merge_path(
        &self,
        requester_path: &str,
        path: &str,
        action: &str,
    ) -> Result<MergeOutcome, String> {
        if !matches!(action, "apply" | "discard") {
            return Err(format!(
                "action must be \"apply\" or \"discard\", got \"{action}\""
            ));
        }
        if !path.starts_with(&format!("{}/", requester_path.trim_end_matches('/'))) {
            return Err(format!("{path} is not one of your subagents"));
        }
        {
            let state = self.inner.state.lock().unwrap();
            if let Some(agent) = state.agents.get(path) {
                if agent.status.is_live() {
                    return Err(format!(
                        "{path} is still running; wait for it or close it first"
                    ));
                }
                if !agent.isolated {
                    return Err(format!(
                        "{path} worked in your directory; there is no worktree to merge"
                    ));
                }
            }
        }
        let pending = self.inner.shared.begin_merge(path)?;
        let workdir = pending.workspace.root.display().to_string();
        let outcome = if action == "discard" {
            remove_workspace(&pending.workspace, &pending.parent_cwd).map(|()| MergeOutcome {
                files: Vec::new(),
                conflicts: Vec::new(),
                workdir: workdir.clone(),
            })
        } else {
            apply_workspace(&pending.workspace, &pending.parent_cwd).map(|(files, conflicts)| {
                MergeOutcome {
                    files,
                    conflicts,
                    workdir: workdir.clone(),
                }
            })
        };
        let done = outcome
            .as_ref()
            .is_ok_and(|merge| merge.conflicts.is_empty());
        self.inner.shared.end_merge(path, done);
        if done {
            self.inner.state.lock().unwrap().revision += 1;
        }
        outcome
    }

    /// `pick_attempt`: applies attempt `target` of the requester's
    /// best-of-N `group` (as merge_agent apply) and, once it applied,
    /// discards every other attempt; attempts still running are closed
    /// first. Each merge or discard queues its `Merge` event. With a
    /// conflict nothing is written and the other attempts are kept.
    pub(crate) fn pick_attempt(
        &self,
        requester_path: &str,
        group: &str,
        target: &str,
    ) -> Result<Value, String> {
        let group = group
            .trim()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let target = target.trim();
        let (members, picked) = {
            let state = self.inner.state.lock().unwrap();
            let members = group_members(&state, requester_path, &group);
            if members.is_empty() {
                return Err(format!("no best-of-N attempts named {group}"));
            }
            let picked = match target.parse::<u64>() {
                Ok(number) => members
                    .iter()
                    .find(|path| {
                        state
                            .agents
                            .get(*path)
                            .is_some_and(|agent| agent.attempt == Some(number))
                    })
                    .cloned(),
                Err(_) if target.starts_with('/') => Some(target.to_string()),
                Err(_) => Some(child_path(requester_path, target)),
            }
            .filter(|path| members.contains(path))
            .ok_or_else(|| {
                format!(
                    "{target} is not an attempt of {group}; attempts: {}",
                    members.join(", ")
                )
            })?;
            if state
                .agents
                .get(&picked)
                .is_some_and(|agent| agent.status.is_live())
            {
                return Err(format!(
                    "{picked} is still running; wait for it or close it first"
                ));
            }
            (members, picked)
        };
        let others: Vec<String> = members.into_iter().filter(|path| *path != picked).collect();
        for other in &others {
            let live = {
                let state = self.inner.state.lock().unwrap();
                state
                    .agents
                    .get(other)
                    .is_some_and(|agent| agent.status.is_live())
            };
            if live {
                let _ = self.close_agent(requester_path, other);
            }
        }
        let merged = self.merge_agent(requester_path, &picked, "apply")?;
        let mut result = json!({ "group": group, "picked": picked, "merge": merged });
        if merged["ok"] == true {
            let mut discarded = Vec::new();
            for other in &others {
                if self.merge_agent(requester_path, other, "discard").is_ok() {
                    discarded.push(other.clone());
                }
            }
            result["discarded"] = json!(discarded);
        } else {
            result["note"] = json!("Nothing was written and the other attempts are kept.");
        }
        Ok(result)
    }

    /// One line for the approval card of `merge_agent apply`: the files and
    /// line counts that would change.
    pub(crate) fn merge_summary(&self, requester_path: &str, target: &str) -> String {
        let target = target.trim();
        let path = if target.starts_with('/') {
            target.to_string()
        } else {
            child_path(requester_path, target)
        };
        let Some(pending) = self.inner.shared.get(&path) else {
            return format!("merge {path}");
        };
        match workspace_changes(&pending.workspace) {
            Ok(changes) => {
                let mut names: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
                let more = names.len().saturating_sub(5);
                names.truncate(5);
                let mut line = format!(
                    "merge {path}: {} files (+{} -{}): {}",
                    changes.files.len(),
                    changes.added,
                    changes.removed,
                    names.join(", ")
                );
                if more > 0 {
                    line.push_str(&format!(" and {more} more"));
                }
                line
            }
            Err(error) => format!("merge {path} ({error})"),
        }
    }

    /// `task_create` (the board is the session's, shared by every agent).
    pub(crate) fn task_create(&self, task: NewTask) -> Result<Value, String> {
        let mut state = self.inner.state.lock().unwrap();
        let created = state.board.create(task, now_ms())?;
        state.board_changed();
        Ok(json!({ "id": created.id }))
    }

    pub(crate) fn task_list(&self) -> Value {
        let state = self.inner.state.lock().unwrap();
        json!({ "tasks": state.board.tasks_json() })
    }

    /// `task_update` by `requester_path`; the updated task.
    pub(crate) fn task_update(
        &self,
        requester_path: &str,
        id: &str,
        action: &str,
        result: Option<&str>,
    ) -> Result<Task, String> {
        let mut state = self.inner.state.lock().unwrap();
        let task = state
            .board
            .update(requester_path, id, action, result, now_ms())?;
        state.board_changed();
        Ok(task)
    }

    /// The board as `task_board` lists it.
    pub(crate) fn board_json(&self) -> Vec<Value> {
        self.inner.state.lock().unwrap().board.tasks_json()
    }

    /// `path` is pending or running.
    pub(crate) fn is_live(&self, path: &str) -> bool {
        let state = self.inner.state.lock().unwrap();
        state
            .agents
            .get(path)
            .is_some_and(|agent| agent.status.is_live())
    }

    /// An agent's working directory and role.
    pub(crate) fn agent_place(&self, path: &str) -> (Option<String>, Option<String>) {
        let state = self.inner.state.lock().unwrap();
        state
            .agents
            .get(path)
            .map(|agent| (agent.workdir.clone(), agent.role.clone()))
            .unwrap_or_default()
    }

    /// The worktree of `path`, the commit it started from and the directory
    /// it merges into, while it waits to be merged.
    pub(crate) fn worktree_of(&self, path: &str) -> Option<(PathBuf, Option<String>, PathBuf)> {
        self.inner.shared.get(path).map(|pending| {
            (
                pending.workspace.root,
                pending.workspace.base,
                pending.parent_cwd,
            )
        })
    }

    /// A plain message for the main agent's next request.
    pub(crate) fn tell_root(&self, from: &str, text: &str) {
        let mut state = self.inner.state.lock().unwrap();
        state
            .inboxes
            .entry(ROOT_PATH.to_string())
            .or_default()
            .push_back(InboxMessage::message(from, text));
        self.inner.changed.notify_all();
    }

    /// Bumped whenever what the session persists changes (`team_json`).
    pub(crate) fn revision(&self) -> u64 {
        self.inner.state.lock().unwrap().revision
    }

    /// What the session keeps of its team: the board and the worktree
    /// registry (each worktree agent with its row), so merges and the board
    /// survive an engine restart.
    pub(crate) fn team_json(&self) -> Value {
        let worktrees = self.inner.shared.snapshot();
        let state = self.inner.state.lock().unwrap();
        let rows: Vec<Value> = worktrees
            .iter()
            .map(|(path, pending)| {
                let mut row = json!({
                    "path": path,
                    "worktree": {
                        "root": pending.workspace.root.display().to_string(),
                        "from_git": pending.workspace.from_git,
                        "base": pending.workspace.base,
                        "parent_cwd": pending.parent_cwd.display().to_string(),
                    },
                });
                if let Some(agent) = state.agents.get(path) {
                    let result = agent.result.as_ref();
                    row["parent"] = json!(agent.parent_path);
                    row["task_name"] = json!(agent.task_name);
                    row["message"] = json!(summarize_to(&agent.message, 2000));
                    row["model"] = json!(agent.model);
                    row["effort"] = json!(agent.reasoning_effort);
                    row["depth"] = json!(agent.depth);
                    row["state"] = json!(agent.status.as_str());
                    row["role"] = json!(agent.role);
                    row["plan_step"] = json!(agent.plan_step);
                    row["background"] = json!(agent.background);
                    row["attempt_group"] = json!(agent.attempt_group);
                    row["attempt"] = json!(agent.attempt);
                    row["tool_use_id"] = json!(agent.tool_use_id);
                    row["started_at"] = json!(agent.started_at_ms);
                    row["completed_at"] = json!(agent.completed_at_ms);
                    row["summary"] = json!(result
                        .map(|result| summarize_to(&result.summary, 2000))
                        .unwrap_or_default());
                    row["files_changed"] = json!(result
                        .map(|result| result.files_changed.clone())
                        .unwrap_or_default());
                }
                row
            })
            .collect();
        json!({ "board": state.board.to_json(), "worktrees": rows })
    }

    /// The team a session saved (`team_json`): its board, and its worktree
    /// agents as finished rows (one still running when the engine stopped
    /// is `interrupted`). A worktree whose directory is gone is dropped.
    /// Restored agents cannot be resumed: their conversation is not kept.
    pub(crate) fn restore(config: AgentsConfig, value: &Value) -> Self {
        let manager = Self::new(config, TeamShared::default());
        let mut state = manager.inner.state.lock().unwrap();
        state.board = Board::from_json(&value["board"]);
        for row in value["worktrees"].as_array().into_iter().flatten() {
            let text = |key: &str| row[key].as_str().map(str::to_string);
            let Some(path) = text("path").filter(|path| path.starts_with("/root/")) else {
                continue;
            };
            let worktree = &row["worktree"];
            let root = PathBuf::from(worktree["root"].as_str().unwrap_or_default());
            let parent_cwd = PathBuf::from(worktree["parent_cwd"].as_str().unwrap_or_default());
            if !root.is_dir() || !parent_cwd.is_dir() {
                continue;
            }
            let workspace = SubagentWorkspace {
                root,
                from_git: worktree["from_git"].as_bool().unwrap_or(false),
                base: worktree["base"].as_str().map(str::to_string),
            };
            let workdir = workspace.root.display().to_string();
            manager.inner.shared.insert(
                &path,
                PendingWorktree {
                    workspace,
                    parent_cwd,
                    busy: false,
                },
            );
            let status = text("state")
                .and_then(|state| SubagentStatus::parse(&state))
                .filter(SubagentStatus::is_final)
                .unwrap_or(SubagentStatus::Interrupted);
            let message = text("message").unwrap_or_default();
            let mut trace = crate::subagent_trace::SubagentTrace::new(&message);
            trace.finish(if status == SubagentStatus::Interrupted {
                "Interrupted"
            } else {
                "Done"
            });
            let task_name = text("task_name")
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or_default().to_string());
            let files_changed = row["files_changed"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let started_at_ms = row["started_at"].as_u64().unwrap_or_else(now_ms);
            state.agents.insert(
                path.clone(),
                SubagentRecord {
                    path: path.clone(),
                    parent_path: text("parent").unwrap_or_else(|| ROOT_PATH.to_string()),
                    task_name,
                    message,
                    model: text("model").unwrap_or_default(),
                    reasoning_effort: text("effort").unwrap_or_default(),
                    depth: row["depth"].as_u64().unwrap_or(1),
                    status,
                    interrupt_flag: Arc::new(AtomicBool::new(false)),
                    result: Some(SubagentRunResult {
                        summary: text("summary").unwrap_or_default(),
                        workdir: workdir.clone(),
                        files_changed,
                        model: text("model").unwrap_or_default(),
                        ..SubagentRunResult::default()
                    }),
                    error: (status == SubagentStatus::Interrupted)
                        .then(|| "the engine stopped while it ran".to_string()),
                    started_at_ms,
                    completed_at_ms: Some(row["completed_at"].as_u64().unwrap_or(started_at_ms)),
                    workdir: Some(workdir),
                    isolated: true,
                    tool_use_id: text("tool_use_id").unwrap_or_default(),
                    role: text("role"),
                    plan_step: text("plan_step"),
                    background: row["background"].as_bool().unwrap_or(false),
                    attempt_group: text("attempt_group"),
                    attempt: row["attempt"].as_u64(),
                    spec: None,
                    context: None,
                    trace,
                },
            );
        }
        drop(state);
        manager
    }

    /// Sum of the trace revisions: changes whenever any agent did something.
    pub(crate) fn trace_revision(&self) -> u64 {
        let state = self.inner.state.lock().unwrap();
        state
            .agents
            .values()
            .map(|agent| agent.trace.revision.wrapping_add(agent.status as u64))
            .fold(state.agents.len() as u64, u64::wrapping_add)
    }

    /// Every agent of this session as an `agent_runs` row (oldest first).
    pub(crate) fn runs_json(&self) -> Vec<Value> {
        let state = self.inner.state.lock().unwrap();
        let now = now_ms();
        let mut agents: Vec<&SubagentRecord> = state.agents.values().collect();
        agents.sort_by_key(|agent| agent.started_at_ms);
        agents
            .into_iter()
            .map(|agent| {
                let end = agent.completed_at_ms.unwrap_or(now);
                let result = agent.result.as_ref();
                json!({
                    "id": agent.path,
                    "label": agent.task_name,
                    "model": agent.model,
                    "effort": agent.reasoning_effort,
                    "state": agent.status.as_str(),
                    "started_at": agent.started_at_ms,
                    "duration_ms": end.saturating_sub(agent.started_at_ms),
                    "tokens": result
                        .map(|result| result.input_tokens + result.output_tokens)
                        .unwrap_or_else(|| agent.trace.tokens()),
                    "tool_calls": result
                        .map(|result| result.tool_calls)
                        .unwrap_or_else(|| agent.trace.tool_calls()),
                    "prompt": agent.message,
                    "result": result.map(|result| result.summary.clone()).unwrap_or_default(),
                    "error": agent.error.clone().unwrap_or_default(),
                    "type": "subagent",
                    "tool_use_id": agent.tool_use_id,
                    "activity": agent.trace.activity(),
                    "role": agent.role,
                    "plan_step": agent.plan_step,
                    "isolation": if agent.isolated { "worktree" } else { "none" },
                    "workdir": agent.workdir.as_ref().filter(|_| agent.isolated),
                    "files_changed": result
                        .map(|result| result.files_changed.clone())
                        .unwrap_or_default(),
                    "background": agent.background,
                    "attempt_group": agent.attempt_group,
                    "attempt": agent.attempt,
                })
            })
            .collect()
    }

    /// One agent's work for `subagent_transcript`, or None for an unknown id.
    pub(crate) fn transcript_json(&self, path: &str) -> Option<Vec<Value>> {
        let state = self.inner.state.lock().unwrap();
        state.agents.get(path).map(|agent| agent.trace.items_json())
    }

    /// Label, model, spawn call, role, plan step and kind of an agent, for
    /// its lifecycle events.
    pub(crate) fn describe(&self, path: &str) -> Option<AgentInfo> {
        let state = self.inner.state.lock().unwrap();
        state.agents.get(path).map(|agent| AgentInfo {
            label: agent.task_name.clone(),
            model: agent.model.clone(),
            tool_use_id: agent.tool_use_id.clone(),
            role: agent.role.clone(),
            plan_step: agent.plan_step.clone(),
            background: agent.background,
            attempt_group: agent.attempt_group.clone(),
            attempt: agent.attempt,
        })
    }

    pub(crate) fn drain_events(&self) -> Vec<TeamEvent> {
        let mut state = self.inner.state.lock().unwrap();
        state.events.drain(..).collect()
    }

    /// Drains the token usage of subagents that have reached a final state so the
    /// parent can fold it into its cumulative totals. Returns each result once.
    pub(crate) fn drain_finished_usage(&self) -> Vec<SubagentRunResult> {
        let mut state = self.inner.state.lock().unwrap();
        std::mem::take(&mut state.finished_usage)
    }

    fn resolve_existing_target(
        &self,
        requester_path: &str,
        target: &str,
    ) -> Result<String, String> {
        let state = self.inner.state.lock().unwrap();
        resolve_existing_target_in_state(&state, requester_path, target)
    }

    fn resolve_path_prefix(&self, requester_path: &str, value: &str) -> String {
        if value.starts_with('/') {
            value.to_string()
        } else {
            child_path(requester_path, value)
        }
    }
}

impl SubagentRegistry {
    fn push_event(&mut self, path: &str, status: &str, message: &str) {
        self.revision += 1;
        self.events.push_back(TeamEvent::Lifecycle {
            path: path.to_string(),
            status: status.to_string(),
            message: message.to_string(),
        });
    }

    fn board_changed(&mut self) {
        self.revision += 1;
        if !self.events.contains(&TeamEvent::Board) {
            self.events.push_back(TeamEvent::Board);
        }
    }
}

/// Live agents under `ancestor` that run in its turn: none of the agents
/// between it and them (they included) runs in the background.
fn foreground_live(state: &SubagentRegistry, ancestor: &str) -> Vec<String> {
    let prefix = format!("{}/", ancestor.trim_end_matches('/'));
    state
        .agents
        .values()
        .filter(|agent| agent.status.is_live() && agent.path.starts_with(&prefix))
        .filter(|agent| !detached(state, &agent.path, ancestor))
        .map(|agent| agent.path.clone())
        .collect()
}

fn detached(state: &SubagentRegistry, path: &str, ancestor: &str) -> bool {
    let mut current = path;
    while current != ancestor {
        let Some(agent) = state.agents.get(current) else {
            return false;
        };
        if agent.background {
            return true;
        }
        current = &agent.parent_path;
    }
    false
}

fn close_paths(state: &mut SubagentRegistry, paths: Vec<String>, message: &str) {
    for path in paths {
        let Some(agent) = state.agents.get_mut(&path) else {
            continue;
        };
        if !agent.status.is_live() {
            continue;
        }
        agent.interrupt_flag.store(true, Ordering::SeqCst);
        agent.status = SubagentStatus::Closed;
        agent.completed_at_ms = Some(now_ms());
        agent.trace.finish("Closed");
        state.push_event(&path, "closed", message);
    }
}

/// The best-of-N attempts of `group` the requester started, by attempt.
fn group_members(state: &SubagentRegistry, requester_path: &str, group: &str) -> Vec<String> {
    let mut members: Vec<(&SubagentRecord, u64)> = state
        .agents
        .values()
        .filter(|agent| {
            agent.parent_path == requester_path && agent.attempt_group.as_deref() == Some(group)
        })
        .map(|agent| (agent, agent.attempt.unwrap_or_default()))
        .collect();
    members.sort_by_key(|(_, attempt)| *attempt);
    members
        .into_iter()
        .map(|(agent, _)| agent.path.clone())
        .collect()
}

/// The attempt groups some of `paths` belong to, with all their members.
fn attempt_groups_of(state: &SubagentRegistry, paths: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for agent in paths.iter().filter_map(|path| state.agents.get(path)) {
        let Some(group) = &agent.attempt_group else {
            continue;
        };
        if groups.iter().any(|(known, _)| known == group) {
            continue;
        }
        groups.push((
            group.clone(),
            group_members(state, &agent.parent_path, group),
        ));
    }
    groups
}

struct MergeOutcome {
    files: Vec<String>,
    conflicts: Vec<String>,
    workdir: String,
}

/// One file a subagent changed, relative to the repository root (or the
/// workspace, for a plain directory).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileChange {
    path: String,
    deleted: bool,
}

struct WorkspaceChanges {
    /// A binary git patch from the base commit to the workspace as it is
    /// (tracked edits, deletions, untracked files); empty for a plain dir.
    patch: Vec<u8>,
    files: Vec<FileChange>,
    added: u64,
    removed: u64,
}

/// What the agent changed since its worktree was created, committed or not.
/// A throwaway index keeps the worktree's own index untouched; nested
/// subagent workspaces are left out.
fn workspace_changes(workspace: &SubagentWorkspace) -> Result<WorkspaceChanges, String> {
    let Some(base) = &workspace.base else {
        let mut paths = Vec::new();
        collect_files(&workspace.root, &workspace.root, &mut paths, usize::MAX);
        let files = paths
            .into_iter()
            .filter(|path| !path.starts_with(".lynshen/agents/"))
            .map(|path| FileChange {
                path,
                deleted: false,
            })
            .collect();
        return Ok(WorkspaceChanges {
            patch: Vec::new(),
            files,
            added: 0,
            removed: 0,
        });
    };
    let index = std::env::temp_dir().join(format!(
        "lynshen-merge-{}-{}.index",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = staged_diff(&workspace.root, &index, base);
    let _ = std::fs::remove_file(&index);
    let StagedDiff {
        patch,
        status,
        numstat,
    } = result?;
    let mut files = Vec::new();
    let mut fields = status
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).to_string());
    while let (Some(kind), Some(path)) = (fields.next(), fields.next()) {
        files.push(FileChange {
            path,
            deleted: kind.starts_with('D'),
        });
    }
    let (mut added, mut removed) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&numstat).lines() {
        let mut parts = line.split('\t');
        added += parts
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        removed += parts
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
    }
    Ok(WorkspaceChanges {
        patch,
        files,
        added,
        removed,
    })
}

/// The workspace staged on a throwaway index, diffed against `base`.
/// Folders a worker's tools create in its workspace (environments, package
/// installs, caches) that never belong to its change.
const SKIPPED_DIRS: [&str; 11] = [
    ".venv",
    "venv",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".gradle",
    ".next",
    ".turbo",
];

struct StagedDiff {
    patch: Vec<u8>,
    /// `--name-status -z`
    status: Vec<u8>,
    numstat: Vec<u8>,
}

fn staged_diff(root: &Path, index: &Path, base: &str) -> Result<StagedDiff, String> {
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .env("GIT_INDEX_FILE", index)
            .args(args)
            .output()
            .map_err(|error| format!("failed to run git: {error}"))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    };
    // Plain, applicable output whatever the user's git config says.
    let diff = |extra: &[&str]| {
        let mut args = vec![
            "diff",
            "--cached",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ];
        args.extend_from_slice(extra);
        args.push(base);
        git(&args)
    };
    git(&["read-tree", base])?;
    // What a worker installs to run its checks (a virtualenv, node_modules,
    // caches) is not its change, .gitignore or not: thousands of files, and
    // a virtualenv's symlinks made every merge of it conflict.
    let mut add = vec!["add", "-A", "--", ".", ":(exclude).lynshen/agents"];
    let skipped: Vec<String> = SKIPPED_DIRS
        .iter()
        .map(|dir| format!(":(exclude,glob)**/{dir}/**"))
        .collect();
    add.extend(skipped.iter().map(String::as_str));
    git(&add)?;
    Ok(StagedDiff {
        patch: diff(&["--binary", "--src-prefix=a/", "--dst-prefix=b/"])?,
        status: diff(&["--name-status", "-z"])?,
        numstat: diff(&["--numstat"])?,
    })
}

/// Applies a workspace's changes to `parent_cwd`: a clean `git apply` when
/// the patch fits, else a three-way merge of each file against the base.
/// Either way all or nothing is written: with conflicts, `(files, conflicts)`
/// comes back with nothing changed and the workspace kept; otherwise the
/// workspace is removed.
fn apply_workspace(
    workspace: &SubagentWorkspace,
    parent_cwd: &Path,
) -> Result<(Vec<String>, Vec<String>), String> {
    let changes = workspace_changes(workspace)?;
    let files: Vec<String> = changes.files.iter().map(|f| f.path.clone()).collect();
    if !changes.files.is_empty() {
        let target = match &workspace.base {
            Some(_) => PathBuf::from(git_text(parent_cwd, &["rev-parse", "--show-toplevel"])?),
            None => parent_cwd.to_path_buf(),
        };
        let clean = workspace.base.is_some()
            && git_apply(&target, &changes.patch, true).is_ok()
            && git_apply(&target, &changes.patch, false).is_ok();
        if !clean {
            let conflicts = three_way_apply(workspace, &target, &changes.files)?;
            if !conflicts.is_empty() {
                return Ok((files, conflicts));
            }
        }
    }
    if let Err(error) = remove_workspace(workspace, parent_cwd) {
        crate::log_warn!("subagent", "merged worktree not removed", error = error);
    }
    Ok((files, Vec::new()))
}

fn git_apply(target: &Path, patch: &[u8], check: bool) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(target)
        .args(["apply", "--whitespace=nowarn"]);
    if check {
        command.arg("--check");
    }
    let mut child = command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run git apply: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch)
            .map_err(|error| format!("failed to write the patch: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("git apply failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The three-way merge `git apply --3way` falls back to, computed for every
/// file before anything is written: a file the parent did not change since
/// the base takes the agent's version; one both changed is merged with
/// `git merge-file`. Returns the conflicting files; when there are none,
/// every result has been written.
fn three_way_apply(
    workspace: &SubagentWorkspace,
    target: &Path,
    files: &[FileChange],
) -> Result<Vec<String>, String> {
    let mut writes: Vec<(PathBuf, Option<Vec<u8>>, Option<PathBuf>)> = Vec::new();
    let mut conflicts = Vec::new();
    for file in files {
        let ours_path = target.join(&file.path);
        let theirs_path = workspace.root.join(&file.path);
        let is_link = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        };
        if is_link(&ours_path) || is_link(&theirs_path) {
            conflicts.push(file.path.clone());
            continue;
        }
        let ours = std::fs::read(&ours_path).ok();
        let theirs =
            if file.deleted {
                None
            } else {
                Some(std::fs::read(&theirs_path).map_err(|error| {
                    format!("failed to read {}: {error}", theirs_path.display())
                })?)
            };
        let base = workspace
            .base
            .as_ref()
            .and_then(|base| git_blob(&workspace.root, base, &file.path));
        if ours == theirs {
            continue;
        }
        if ours == base {
            let mode_from = theirs.is_some().then(|| theirs_path.clone());
            writes.push((ours_path, theirs, mode_from));
            continue;
        }
        match (base, ours, theirs) {
            (Some(base), Some(ours), Some(theirs)) => match merge_file(&ours, &base, &theirs) {
                Some(merged) => writes.push((ours_path, Some(merged), None)),
                None => conflicts.push(file.path.clone()),
            },
            _ => conflicts.push(file.path.clone()),
        }
    }
    if !conflicts.is_empty() {
        return Ok(conflicts);
    }
    for (path, content, mode_from) in writes {
        match content {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("failed to create {}: {error}", parent.display())
                    })?;
                }
                std::fs::write(&path, content)
                    .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
                if let Some(from) = mode_from {
                    if let Ok(meta) = std::fs::metadata(&from) {
                        let _ = std::fs::set_permissions(&path, meta.permissions());
                    }
                }
            }
            None => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("failed to delete {}: {error}", path.display())),
            },
        }
    }
    Ok(Vec::new())
}

/// `path` as of commit `base`; None when it did not exist there.
fn git_blob(repo: &Path, base: &str, path: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", &format!("{base}:{path}")])
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// `git merge-file` of three versions; None on a conflict or a binary file.
fn merge_file(ours: &[u8], base: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "lynshen-merge-file-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let paths = [dir.join("ours"), dir.join("base"), dir.join("theirs")];
    let written = [ours, base, theirs]
        .iter()
        .zip(&paths)
        .all(|(content, path)| std::fs::write(path, content).is_ok());
    let output = written
        .then(|| {
            Command::new("git")
                .args(["merge-file", "-p"])
                .args(&paths)
                .output()
                .ok()
        })
        .flatten();
    let _ = std::fs::remove_dir_all(&dir);
    let output = output?;
    (output.status.code() == Some(0)).then_some(output.stdout)
}

/// Deletes a workspace no agent ran in (its spawn failed), logging a failure.
pub(crate) fn discard_workspace(workspace: &SubagentWorkspace, parent_cwd: &Path) {
    if let Err(error) = remove_workspace(workspace, parent_cwd) {
        crate::log_warn!("subagent", "unused worktree not removed", error = error);
    }
}

/// Deletes a workspace; a git worktree is then pruned from the repository.
fn remove_workspace(workspace: &SubagentWorkspace, parent_cwd: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(&workspace.root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to remove {}: {error}",
                workspace.root.display()
            ))
        }
    }
    if workspace.from_git {
        git_text(parent_cwd, &["worktree", "prune"])?;
    }
    Ok(())
}

/// Removes subagent workspaces under `<cwd>/.lynshen/agents/` created more
/// than `keep_days` days ago (their names end in the creation time in ms);
/// 0 keeps them all. Returns how many were removed. `~/.lynshen/agents`
/// (the daemon's resident agents) is never touched.
pub(crate) fn remove_stale_workspaces(cwd: &Path, profile_dir: &Path, keep_days: u64) -> usize {
    if keep_days == 0 {
        return 0;
    }
    let lynshen = cwd.join(".lynshen");
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&lynshen, profile_dir) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(lynshen.join("agents")) else {
        return 0;
    };
    let cutoff = now_ms().saturating_sub(keep_days.saturating_mul(86_400_000));
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some((task, millis)) = name.rsplit_once('-') else {
            continue;
        };
        let stale = millis.len() >= 13
            && millis.parse::<u64>().is_ok_and(|created| created < cutoff)
            && validate_task_name(task).is_ok()
            && entry.path().is_dir();
        if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 && in_git_repository(cwd) {
        let _ = git_text(cwd, &["worktree", "prune"]);
    }
    removed
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn summarize(message: &str) -> String {
    summarize_to(message, MESSAGE_SUMMARY_CHARS)
}

fn summarize_to(message: &str, limit: usize) -> String {
    let message = message.trim();
    let mut summary: String = message.chars().take(limit).collect();
    if summary.len() < message.len() {
        summary.push('…');
    }
    summary
}

/// A send_message target: `/root` or an agent path; a name is the
/// requester's child, else its sibling.
fn resolve_message_target(
    state: &SubagentRegistry,
    requester_path: &str,
    target: &str,
) -> Result<String, String> {
    let target = target.trim();
    if target == ROOT_PATH {
        return Ok(ROOT_PATH.to_string());
    }
    if target.starts_with('/') {
        return resolve_existing_target_in_state(state, requester_path, target);
    }
    let child = resolve_existing_target_in_state(state, requester_path, target);
    if child.is_ok() {
        return child;
    }
    match state.agents.get(requester_path) {
        Some(me) => resolve_existing_target_in_state(state, &me.parent_path, target)
            .map_err(|_| format!("no child or sibling named {target}")),
        None => child,
    }
}

fn resolve_existing_target_in_state(
    state: &SubagentRegistry,
    requester_path: &str,
    target: &str,
) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("target is required".to_string());
    }
    let canonical = if target.starts_with('/') {
        target.to_string()
    } else {
        child_path(requester_path, target)
    };
    if state.agents.contains_key(&canonical) {
        Ok(canonical)
    } else {
        Err(format!("agent not found: {canonical}"))
    }
}

fn wait_statuses(state: &SubagentRegistry, targets: &[String]) -> Value {
    let mut statuses = serde_json::Map::new();
    let agents = if targets.is_empty() {
        state.agents.values().collect::<Vec<_>>()
    } else {
        targets
            .iter()
            .filter_map(|target| state.agents.get(target))
            .collect::<Vec<_>>()
    };
    for agent in agents {
        statuses.insert(agent.path.clone(), status_json(agent));
    }
    Value::Object(statuses)
}

fn agent_json(agent: &SubagentRecord) -> Value {
    json!({
        "task_name": agent.path,
        "name": agent.task_name,
        "parent": agent.parent_path,
        "depth": agent.depth,
        "status": status_json(agent),
        "model": agent.model,
        "reasoning_effort": agent.reasoning_effort,
        "role": agent.role,
        "background": agent.background,
        "attempt_group": agent.attempt_group,
        "attempt": agent.attempt,
        "message": agent.message,
        "started_at_ms": agent.started_at_ms,
        "completed_at_ms": agent.completed_at_ms,
        "workdir": agent.workdir,
        "result": agent.result.as_ref().map(result_json),
    })
}

fn status_json(agent: &SubagentRecord) -> Value {
    match agent.status {
        SubagentStatus::Completed => json!({
            "completed": agent.result.as_ref().map(|result| result.summary.clone()).unwrap_or_default()
        }),
        SubagentStatus::Errored | SubagentStatus::BudgetExhausted => {
            let mut status = serde_json::Map::new();
            status.insert(
                agent.status.as_str().to_string(),
                json!(agent
                    .error
                    .clone()
                    .unwrap_or_else(|| "agent failed".to_string())),
            );
            status.insert(
                "partial_output".to_string(),
                json!(agent
                    .result
                    .as_ref()
                    .map(|result| result.partial_output.clone())
                    .unwrap_or_default()),
            );
            Value::Object(status)
        }
        SubagentStatus::Pending
        | SubagentStatus::Running
        | SubagentStatus::Interrupted
        | SubagentStatus::Closed
        | SubagentStatus::Conflict
        | SubagentStatus::Merged
        | SubagentStatus::Discarded => json!(agent.status.as_str()),
    }
}

fn result_json(result: &SubagentRunResult) -> Value {
    json!({
        "summary": result.summary,
        "partial_output": result.partial_output,
        "tool_calls": result.tool_calls,
        "tools_used": result.tools_used,
        "input_tokens": result.input_tokens,
        "cached_input_tokens": result.cached_input_tokens,
        "output_tokens": result.output_tokens,
        "elapsed_ms": result.elapsed_ms,
        "model": result.model,
        "workdir": result.workdir,
        "files_changed": result.files_changed,
    })
}

fn child_path(parent: &str, task_name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    format!("{parent}/{}", task_name.trim_matches('/'))
}

fn validate_task_name(task_name: &str) -> Result<(), String> {
    if task_name.is_empty() {
        return Err("task_name is required".to_string());
    }
    if task_name.len() > 64 {
        return Err("task_name is too long".to_string());
    }
    if task_name == "root"
        || task_name == "parent"
        || !task_name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err("task_name must use lowercase letters, digits, and underscores".to_string());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::StreamEvent;

    fn spawn(task_name: &str) -> SubagentSpawn {
        SubagentSpawn {
            parent_path: "/root".to_string(),
            task_name: task_name.to_string(),
            message: "inspect".to_string(),
            model: "gpt-test".to_string(),
            reasoning_effort: "medium".to_string(),
            depth: 1,
            tool_use_id: String::new(),
            role: None,
            plan_step: None,
            ..SubagentSpawn::default()
        }
    }

    #[test]
    fn subagent_spawn_reserves_and_lists_agent() {
        let manager = SubagentManager::default();
        let slot = manager.reserve_spawn(spawn("worker")).unwrap();
        assert_eq!(slot.path, "/root/worker");
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["task_name"], "/root/worker");
        assert_eq!(listed["agents"][0]["status"], "pending");
    }

    #[test]
    fn subagent_rejects_duplicate_task_name() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        let error = manager.reserve_spawn(spawn("worker")).unwrap_err();
        assert!(error.contains("already exists"));
        assert!(manager.reserve_spawn(spawn("parent")).is_err());
    }

    #[test]
    fn subagent_enforces_live_limit() {
        let manager = SubagentManager::default();
        for index in 0..AgentsConfig::default().max_live {
            manager
                .reserve_spawn(spawn(&format!("worker_{index}")))
                .unwrap();
        }
        let error = manager.reserve_spawn(spawn("extra")).unwrap_err();
        assert!(error.contains("too many live agents"));
    }

    #[test]
    fn live_and_depth_limits_come_from_config() {
        let config = AgentsConfig {
            max_live: 1,
            max_depth: 1,
            ..AgentsConfig::default()
        };
        let manager = SubagentManager::new(config, TeamShared::default());
        manager.reserve_spawn(spawn("first")).unwrap();
        let error = manager.reserve_spawn(spawn("second")).unwrap_err();
        assert!(error.contains("too many live agents (1)"), "{error}");
        let mut nested = spawn("nested");
        nested.depth = 2;
        assert!(manager
            .reserve_spawn(nested)
            .unwrap_err()
            .contains("depth limit"));
    }

    #[test]
    fn subagent_wait_returns_completed_summary() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        manager.finish_ok(
            "/root/worker",
            SubagentRunResult {
                summary: "done".to_string(),
                partial_output: "done".to_string(),
                input_tokens: 1,
                output_tokens: 2,
                elapsed_ms: 3,
                model: "gpt-test".to_string(),
                ..Default::default()
            },
        );
        let result = manager
            .wait_agents("/root", vec!["worker".to_string()], 1)
            .unwrap();
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["status"]["/root/worker"]["completed"], "done");
        assert!(result.get("messages").is_none());
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["result"]["summary"], "done");
        assert_eq!(listed["agents"][0]["result"]["tool_calls"], 0);
    }

    fn run_result(input: u64, output: u64) -> SubagentRunResult {
        SubagentRunResult {
            summary: "done".to_string(),
            partial_output: "done".to_string(),
            input_tokens: input,
            output_tokens: output,
            elapsed_ms: 1,
            model: "gpt-test".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn finished_usage_drains_once_and_skips_closed_agents() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        manager.finish_ok("/root/worker", run_result(100, 7));

        let drained = manager.drain_finished_usage();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].input_tokens, 100);
        assert_eq!(drained[0].output_tokens, 7);
        assert_eq!(drained[0].model, "gpt-test");
        // Drained exactly once: a second drain is empty.
        assert!(manager.drain_finished_usage().is_empty());

        // A closed agent contributes no usage even if its thread later finishes.
        manager.reserve_spawn(spawn("closed_worker")).unwrap();
        manager.mark_running("/root/closed_worker");
        manager.close_agent("/root", "closed_worker").unwrap();
        manager.finish_ok("/root/closed_worker", run_result(999, 999));
        assert!(manager.drain_finished_usage().is_empty());
    }

    #[test]
    fn subagent_close_interrupts_running_agent() {
        let manager = SubagentManager::default();
        let slot = manager.reserve_spawn(spawn("worker")).unwrap();
        manager.mark_running("/root/worker");
        let result = manager.close_agent("/root", "worker").unwrap();
        assert_eq!(result["closed"], true);
        assert!(slot.interrupt_flag.load(Ordering::SeqCst));
        let listed = manager.list_agents("/root", None);
        assert_eq!(listed["agents"][0]["status"], "closed");
    }

    #[test]
    fn subagent_rejects_depth_limit() {
        let manager = SubagentManager::default();
        let mut spawn = spawn("too_deep");
        spawn.depth = AgentsConfig::default().max_depth + 1;
        let error = manager.reserve_spawn(spawn).unwrap_err();
        assert!(error.contains("depth limit"));
    }

    #[test]
    fn subagent_target_not_found_is_clear() {
        let manager = SubagentManager::default();
        let error = manager
            .wait_agents("/root", vec!["missing".to_string()], 1)
            .unwrap_err();
        assert!(error.contains("agent not found: /root/missing"));
    }

    #[test]
    fn a_message_to_the_parent_wakes_its_wait_and_is_handed_over_once() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        manager.mark_running("/root/worker");
        manager.drain_events();

        let child = manager.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            child
                .send_message("/root/worker", "parent", "Which config key?")
                .unwrap()
        });
        let started = std::time::Instant::now();
        let result = manager.wait_agents("/root", Vec::new(), 10_000).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(sender.join().unwrap()["target"], "/root");
        assert_eq!(result["timed_out"], false);
        assert_eq!(
            result["messages"],
            json!([{ "from": "/root/worker", "message": "Which config key?" }])
        );
        assert_eq!(result["status"]["/root/worker"], "running");
        // Handed over by wait_agent: not read again before the next request.
        assert!(manager.drain_messages(ROOT_PATH).is_empty());
        assert!(manager.drain_events().contains(&TeamEvent::Message {
            from: "/root/worker".to_string(),
            to: "/root".to_string(),
            summary: "Which config key?".to_string(),
        }));

        // Without wait_agent the parent reads it before its next request.
        manager
            .send_message("/root/worker", "parent", "Found it")
            .unwrap();
        assert_eq!(
            manager.drain_messages(ROOT_PATH),
            vec![InboxMessage::message("/root/worker", "Found it")]
        );
        assert!(manager
            .send_message(ROOT_PATH, "parent", "hi")
            .unwrap_err()
            .contains("no parent"));
    }

    #[test]
    fn a_nested_agent_messages_its_own_parent() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("lead")).unwrap();
        manager.mark_running("/root/lead");
        let mut helper = spawn("helper");
        helper.parent_path = "/root/lead".to_string();
        helper.depth = 2;
        manager.reserve_spawn(helper).unwrap();
        manager.mark_running("/root/lead/helper");
        manager
            .send_message("/root/lead/helper", "parent", "done with half")
            .unwrap();
        assert_eq!(manager.drain_messages("/root/lead").len(), 1);
        assert!(manager.drain_messages(ROOT_PATH).is_empty());
        // Down: the parent's message reaches the child the same way.
        manager.send_message("/root", "lead", "hurry").unwrap();
        assert_eq!(
            manager.drain_messages("/root/lead")[0],
            InboxMessage::message("/root", "hurry")
        );
    }

    fn usage(input_tokens: u64, output_tokens: u64) -> StreamEvent {
        StreamEvent::Usage {
            input_tokens,
            cached_input_tokens: 0,
            output_tokens,
            reasoning_tokens: 0,
        }
    }

    fn budget_events(manager: &SubagentManager) -> Vec<(u64, u64)> {
        manager
            .drain_events()
            .into_iter()
            .filter_map(|event| match event {
                TeamEvent::Budget { used, limit } => Some((used, limit)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_turn_budget_warns_at_80_percent_and_stops_at_100() {
        let config = AgentsConfig {
            turn_token_budget: 1000,
            ..AgentsConfig::default()
        };
        let manager = SubagentManager::new(config, TeamShared::default());
        manager.reserve_spawn(spawn("a")).unwrap();
        manager.reserve_spawn(spawn("b")).unwrap();
        manager.mark_running("/root/a");
        manager.mark_running("/root/b");
        assert_eq!(budget_events(&manager), vec![(0, 1000)]);

        manager.record("/root/a", &usage(500, 100));
        manager.record("/root/a", &usage(50, 0));
        // One event per tenth reached, not per request.
        assert_eq!(budget_events(&manager), vec![(600, 1000)]);
        assert!(manager.drain_messages("/root/b").is_empty());

        manager.record("/root/b", &usage(200, 0));
        assert_eq!(budget_events(&manager), vec![(850, 1000)]);
        for path in ["/root/a", "/root/b", ROOT_PATH] {
            let messages = manager.drain_messages(path);
            assert_eq!(messages.len(), 1, "{path}");
            assert_eq!(messages[0].from, "team_budget");
            assert!(messages[0].text.contains("850 of 1000"), "{path}");
        }
        assert!(!manager.budget_exhausted());

        manager.record("/root/b", &usage(100, 100));
        assert!(manager.budget_exhausted());
        assert_eq!(budget_events(&manager), vec![(1050, 1000)]);
        // The warning goes out once.
        assert!(manager.drain_messages("/root/a").is_empty());
        let error = manager.reserve_spawn(spawn("c")).unwrap_err();
        assert!(error.contains("used up"), "{error}");

        // A running agent stops with its partial result kept.
        let partial = SubagentRunResult {
            partial_output: "half done".to_string(),
            ..run_result(1, 1)
        };
        manager.finish_err("/root/a", BUDGET_EXHAUSTED.to_string(), partial);
        let rows = manager.runs_json();
        assert_eq!(rows[0]["state"], "budget_exhausted");
        let listed = manager.list_agents("/root", Some("a"));
        assert_eq!(listed["agents"][0]["status"]["partial_output"], "half done");
    }

    #[test]
    fn no_budget_means_no_budget_events_or_limits() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("a")).unwrap();
        manager.record("/root/a", &usage(10_000_000, 0));
        assert!(!manager.budget_exhausted());
        assert!(budget_events(&manager).is_empty());
        assert!(manager.drain_messages(ROOT_PATH).is_empty());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-subagent-ws-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=test"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn workspace_in_git_repo_is_worktree_and_child_writes_stay_out_of_parent() {
        let repo = temp_dir("git");
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("tracked.txt"), "committed content").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        let workspace = prepare_workspace(&repo, "worker").unwrap();
        assert!(workspace.from_git);
        assert!(workspace.base.is_some());
        assert!(workspace
            .root
            .starts_with(repo.join(".lynshen").join("agents")));
        // The child sees the committed tree...
        assert!(workspace.root.join("tracked.txt").exists());

        // ...and its writes do not appear in the parent cwd.
        std::fs::write(workspace.root.join("child_output.txt"), "from child").unwrap();
        std::fs::write(workspace.root.join("tracked.txt"), "child edit").unwrap();
        assert!(!repo.join("child_output.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "committed content"
        );

        let mut changed = changed_files(&workspace);
        changed.sort();
        assert_eq!(changed, vec!["child_output.txt", "tracked.txt"]);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn workspace_outside_git_repo_is_fresh_restricted_directory() {
        let parent = temp_dir("plain");
        std::fs::write(parent.join("parent.txt"), "parent data").unwrap();

        let workspace = prepare_workspace(&parent, "worker").unwrap();
        assert!(!workspace.from_git);
        // Restricted cwd: the child starts from an empty directory.
        assert!(!workspace.root.join("parent.txt").exists());

        std::fs::write(workspace.root.join("report.md"), "findings").unwrap();
        assert!(!parent.join("report.md").exists());
        assert_eq!(changed_files(&workspace), vec!["report.md".to_string()]);

        let _ = std::fs::remove_dir_all(parent);
    }

    const LINES: &str = "one\ntwo\nthree\nfour\nfive\n";

    /// A repository with `a.txt` (five lines) and `b.txt` committed, and a
    /// finished worktree agent `/root/worker` registered for merging.
    fn merge_fixture(tag: &str) -> (PathBuf, SubagentManager, SubagentWorkspace) {
        let repo = temp_dir(tag);
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), ".lynshen/\n").unwrap();
        std::fs::write(repo.join("a.txt"), LINES).unwrap();
        std::fs::write(repo.join("b.txt"), "bee\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("worker")).unwrap();
        let workspace = prepare_workspace(&repo, "worker").unwrap();
        manager.register_workspace("/root/worker", &workspace, &repo);
        manager.mark_running("/root/worker");
        (repo, manager, workspace)
    }

    fn merge_events(manager: &SubagentManager) -> Vec<TeamEvent> {
        manager
            .drain_events()
            .into_iter()
            .filter(|event| matches!(event, TeamEvent::Merge { .. }))
            .collect()
    }

    #[test]
    fn merge_apply_brings_tracked_untracked_and_deleted_files_back() {
        let (repo, manager, workspace) = merge_fixture("merge-apply");
        let root = &workspace.root;
        std::fs::write(root.join("a.txt"), LINES.replace("four", "FOUR")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/new.rs"), "fn main() {}\n").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        // A commit inside the worktree is part of the change too.
        git(root, &["add", "src/new.rs"]);
        git(root, &["commit", "-qm", "child"]);

        let refused = manager.merge_agent("/root", "worker", "apply").unwrap_err();
        assert!(refused.contains("still running"), "{refused}");
        manager.finish_ok("/root/worker", run_result(1, 1));
        manager.drain_events();

        let summary = manager.merge_summary("/root", "worker");
        assert!(summary.contains("3 files"), "{summary}");

        let result = manager.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["files"], json!(["a.txt", "b.txt", "src/new.rs"]));
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            LINES.replace("four", "FOUR")
        );
        assert!(repo.join("src/new.rs").exists());
        assert!(!repo.join("b.txt").exists());
        // The worktree is gone, from disk and from git.
        assert!(!root.exists());
        let list = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 1);
        assert_eq!(manager.runs_json()[0]["state"], "merged");
        assert_eq!(
            merge_events(&manager),
            vec![TeamEvent::Merge {
                target: "/root/worker".to_string(),
                action: "apply".to_string(),
                ok: true,
                files: vec!["a.txt".into(), "b.txt".into(), "src/new.rs".into()],
                conflicts: Vec::new(),
                error: None,
            }]
        );
        // Merged once: there is nothing left to merge.
        assert!(manager.merge_agent("/root", "worker", "apply").is_err());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn merge_apply_leaves_environments_and_caches_behind() {
        let (repo, manager, workspace) = merge_fixture("merge-skips-venv");
        let root = &workspace.root;
        std::fs::write(root.join("test_a.py"), "def test_a():\n    pass\n").unwrap();
        // What running the checks left: a virtualenv (with a symlink, which a
        // three-way merge cannot take), node_modules and a cache.
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::fs::write(root.join(".venv/pyvenv.cfg"), "home = /usr\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/usr/bin/python3", root.join(".venv/bin/python")).unwrap();
        std::fs::create_dir_all(root.join("web/node_modules/x")).unwrap();
        std::fs::write(root.join("web/node_modules/x/index.js"), "1\n").unwrap();
        std::fs::create_dir_all(root.join("__pycache__")).unwrap();
        std::fs::write(root.join("__pycache__/a.pyc"), "x").unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        manager.drain_events();

        let result = manager.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["files"], json!(["test_a.py"]));
        assert!(repo.join("test_a.py").exists());
        assert!(!repo.join(".venv").exists() && !repo.join("web/node_modules").exists());
    }

    #[test]
    fn merge_apply_merges_three_way_when_the_parent_changed_the_file_too() {
        let (repo, manager, workspace) = merge_fixture("merge-3way");
        std::fs::write(workspace.root.join("a.txt"), LINES.replace("four", "FOUR")).unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        // The parent changed a nearby line since: a plain apply no longer fits.
        std::fs::write(repo.join("a.txt"), LINES.replace("two", "TWO")).unwrap();

        let result = manager.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "one\nTWO\nthree\nFOUR\nfive\n"
        );
        assert!(!workspace.root.exists());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn merge_conflict_writes_nothing_then_discard_removes_the_worktree() {
        let (repo, manager, workspace) = merge_fixture("merge-conflict");
        std::fs::write(
            workspace.root.join("a.txt"),
            LINES.replace("three", "child"),
        )
        .unwrap();
        std::fs::write(workspace.root.join("c.txt"), "new from child\n").unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        let parent_edit = LINES.replace("three", "parent");
        std::fs::write(repo.join("a.txt"), &parent_edit).unwrap();
        manager.drain_events();

        let result = manager.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["conflicts"], json!(["a.txt"]));
        assert!(result["note"]
            .as_str()
            .unwrap()
            .contains("Nothing was written"));
        // All or nothing: the clean file was not written either.
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            parent_edit
        );
        assert!(!repo.join("c.txt").exists());
        assert!(workspace.root.exists());
        assert_eq!(manager.runs_json()[0]["state"], "conflict");
        let events = manager.drain_events();
        assert!(events.contains(&TeamEvent::Lifecycle {
            path: "/root/worker".to_string(),
            status: "conflict".to_string(),
            message: "conflicts in a.txt".to_string(),
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            TeamEvent::Merge { ok: false, conflicts, .. } if conflicts == &vec!["a.txt".to_string()]
        )));

        let result = manager
            .merge_agent("/root", "/root/worker", "discard")
            .unwrap();
        assert_eq!(result["ok"], true);
        assert!(!workspace.root.exists());
        assert_eq!(manager.runs_json()[0]["state"], "discarded");
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            parent_edit
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn a_new_file_the_parent_also_created_differently_conflicts() {
        let (repo, manager, workspace) = merge_fixture("merge-added");
        std::fs::write(workspace.root.join("c.txt"), "child\n").unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        std::fs::write(repo.join("c.txt"), "parent\n").unwrap();
        let result = manager.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["conflicts"], json!(["c.txt"]));
        assert_eq!(
            std::fs::read_to_string(repo.join("c.txt")).unwrap(),
            "parent\n"
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn a_later_manager_merges_a_worktree_left_by_an_earlier_turn() {
        let (repo, manager, workspace) = merge_fixture("merge-later");
        std::fs::write(workspace.root.join("c.txt"), "child\n").unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        // The next turn starts a new manager on the same team state.
        let next = SubagentManager::new(AgentsConfig::default(), manager.shared().clone());
        let error = next.reserve_spawn(spawn("worker")).unwrap_err();
        assert!(error.contains("worktree to merge"), "{error}");
        let result = next.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["files"], json!(["c.txt"]));
        assert!(repo.join("c.txt").exists());
        assert!(next.drain_events().contains(&TeamEvent::Lifecycle {
            path: "/root/worker".to_string(),
            status: "merged".to_string(),
            message: "applied 1 file".to_string(),
        }));
        next.reserve_spawn(spawn("worker")).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn merge_refuses_shared_cwd_agents_strangers_and_bad_actions() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("inplace")).unwrap();
        manager.finish_ok("/root/inplace", run_result(1, 1));
        let error = manager
            .merge_agent("/root", "inplace", "apply")
            .unwrap_err();
        assert!(error.contains("no worktree"), "{error}");
        let error = manager
            .merge_agent("/root/other", "/root/inplace", "apply")
            .unwrap_err();
        assert!(error.contains("not one of your subagents"), "{error}");
        let error = manager.merge_agent("/root", "inplace", "keep").unwrap_err();
        assert!(error.contains("apply"), "{error}");
        // Every attempt is reported to the front-ends.
        assert_eq!(merge_events(&manager).len(), 3);
    }

    #[test]
    fn plain_workspace_apply_copies_new_files() {
        let parent = temp_dir("merge-plain");
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("writer")).unwrap();
        let workspace = prepare_workspace(&parent, "writer").unwrap();
        manager.register_workspace("/root/writer", &workspace, &parent);
        std::fs::create_dir_all(workspace.root.join("docs")).unwrap();
        std::fs::write(workspace.root.join("docs/report.md"), "findings").unwrap();
        manager.finish_ok("/root/writer", run_result(1, 1));
        let result = manager.merge_agent("/root", "writer", "apply").unwrap();
        assert_eq!(result["files"], json!(["docs/report.md"]));
        assert_eq!(
            std::fs::read_to_string(parent.join("docs/report.md")).unwrap(),
            "findings"
        );
        assert!(!workspace.root.exists());
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn stale_workspaces_are_removed_after_keep_days() {
        let cwd = temp_dir("stale");
        let profile = temp_dir("stale-profile");
        let agents = cwd.join(".lynshen/agents");
        let old = now_ms() - 8 * 86_400_000;
        let fresh = now_ms() - 86_400_000;
        for name in [
            format!("old_task-{old}"),
            format!("fresh_task-{fresh}"),
            "notes".to_string(),
        ] {
            std::fs::create_dir_all(agents.join(&name)).unwrap();
            std::fs::write(agents.join(name).join("f.txt"), "x").unwrap();
        }
        assert_eq!(remove_stale_workspaces(&cwd, &profile, 0), 0);
        assert_eq!(remove_stale_workspaces(&cwd, &profile, 7), 1);
        assert!(!agents.join(format!("old_task-{old}")).exists());
        assert!(agents.join(format!("fresh_task-{fresh}")).exists());
        assert!(agents.join("notes").exists());
        assert_eq!(remove_stale_workspaces(&cwd, &profile, 0), 0);

        // When cwd/.lynshen is the profile (cwd is the home directory), its
        // agents directory holds the daemon's resident agents: never touched.
        let resident = agents.join(format!("resident-{old}"));
        std::fs::create_dir_all(&resident).unwrap();
        assert_eq!(remove_stale_workspaces(&cwd, &cwd.join(".lynshen"), 7), 0);
        assert!(resident.exists());
        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(profile);
    }

    fn background(task_name: &str) -> SubagentSpawn {
        SubagentSpawn {
            background: true,
            ..spawn(task_name)
        }
    }

    #[test]
    fn a_background_agent_outlives_its_parents_turn_and_wakes_the_main_agent_once() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("fg")).unwrap();
        manager.reserve_spawn(background("bg")).unwrap();
        let mut helper = spawn("helper");
        helper.parent_path = "/root/bg".to_string();
        helper.depth = 2;
        manager.reserve_spawn(helper).unwrap();
        for path in ["/root/fg", "/root/bg", "/root/bg/helper"] {
            manager.mark_running(path);
        }
        let state = |path: &str| {
            manager
                .runs_json()
                .into_iter()
                .find(|row| row["id"] == path)
                .unwrap()["state"]
                .clone()
        };
        // The turn ends (or is interrupted): only its foreground agents stop;
        // the background agent and what it started work on.
        manager.close_all_with_message("parent turn finished");
        manager.close_all();
        assert_eq!(state("/root/fg"), "closed");
        assert_eq!(state("/root/bg"), "running");
        assert_eq!(state("/root/bg/helper"), "running");
        let background = |path: &str| {
            manager
                .runs_json()
                .into_iter()
                .find(|row| row["id"] == path)
                .unwrap()["background"]
                .clone()
        };
        assert_eq!(background("/root/bg"), true);
        assert_eq!(background("/root/bg/helper"), false);
        assert!(manager.take_wake().is_empty());

        // It finishes: the agents it started in its turn stop with it, and
        // its result waits for the main agent, once.
        manager.finish_ok("/root/bg", run_result(1, 1));
        assert_eq!(state("/root/bg/helper"), "closed");
        let mail = manager.take_wake();
        assert_eq!(mail.len(), 1);
        assert_eq!(
            mail[0].kind,
            MailKind::Result {
                status: "completed".to_string()
            }
        );
        assert_eq!(
            mail[0].model_text(),
            "<subagent_result path=\"/root/bg\" status=\"completed\">\ndone\n</subagent_result>"
        );
        assert!(manager.take_wake().is_empty());
        // A foreground agent's result never wakes anyone.
        manager.reserve_spawn(spawn("fg2")).unwrap();
        manager.finish_ok("/root/fg2", run_result(1, 1));
        assert!(manager.take_wake().is_empty());
    }

    #[test]
    fn a_closed_background_agent_wakes_nobody_and_the_session_end_stops_all() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(background("bg")).unwrap();
        manager.mark_running("/root/bg");
        manager.close_agent("/root", "bg").unwrap();
        manager.finish_ok("/root/bg", run_result(1, 1));
        assert!(manager.take_wake().is_empty());

        manager.reserve_spawn(background("other")).unwrap();
        manager.close_everything("session switched");
        assert_eq!(manager.runs_json()[1]["state"], "closed");
    }

    #[test]
    fn wait_hands_over_a_background_result_once() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(background("bg")).unwrap();
        manager.finish_ok("/root/bg", run_result(1, 1));
        // Waited on: the status carries the result and the mail is dropped.
        let result = manager
            .wait_agents("/root", vec!["bg".to_string()], 100)
            .unwrap();
        assert_eq!(result["status"]["/root/bg"]["completed"], "done");
        assert!(result.get("messages").is_none(), "{result}");
        assert!(manager.take_wake().is_empty());

        // A finished agent does not end a wait without targets; a running
        // one does, and so does a background result arriving meanwhile.
        manager.reserve_spawn(spawn("busy")).unwrap();
        manager.mark_running("/root/busy");
        let result = manager.wait_agents("/root", Vec::new(), 100).unwrap();
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["status"]["/root/busy"], "running");
        manager.reserve_spawn(background("late")).unwrap();
        manager.finish_ok("/root/late", run_result(1, 1));
        let result = manager.wait_agents("/root", Vec::new(), 5_000).unwrap();
        assert_eq!(result["timed_out"], false);
        assert_eq!(
            result["messages"],
            json!([{ "from": "/root/late", "message": "done", "kind": "result", "status": "completed" }])
        );
        assert!(manager.take_wake().is_empty());
    }

    #[test]
    fn an_agent_messages_a_sibling_by_task_name_or_path() {
        let manager = SubagentManager::default();
        for name in ["api", "client"] {
            manager.reserve_spawn(spawn(name)).unwrap();
            manager.mark_running(&format!("/root/{name}"));
        }
        manager.drain_events();
        let sent = manager
            .send_message("/root/api", "client", "the route is /v2 now")
            .unwrap();
        assert_eq!(sent["target"], "/root/client");
        assert_eq!(
            manager.drain_messages("/root/client"),
            vec![InboxMessage::message("/root/api", "the route is /v2 now")]
        );
        manager
            .send_message("/root/client", "/root/api", "thanks")
            .unwrap();
        assert_eq!(manager.drain_messages("/root/api").len(), 1);
        assert!(manager.drain_events().contains(&TeamEvent::Message {
            from: "/root/client".to_string(),
            to: "/root/api".to_string(),
            summary: "thanks".to_string(),
        }));
        // To the main agent by its path, as "parent" does.
        manager.send_message("/root/api", "/root", "done").unwrap();
        assert_eq!(manager.drain_messages(ROOT_PATH).len(), 1);
        let error = manager
            .send_message("/root/api", "ghost", "hi")
            .unwrap_err();
        assert!(error.contains("no child or sibling named ghost"), "{error}");
        assert!(manager.send_message("/root/api", "api", "me").is_err());
    }

    fn attempt(group: &str, number: u64) -> SubagentSpawn {
        SubagentSpawn {
            attempt_group: Some(group.to_string()),
            attempt: Some(number),
            ..spawn(&format!("{group}_a{number}"))
        }
    }

    #[test]
    fn attempts_are_reserved_together_or_not_at_all() {
        let config = AgentsConfig {
            max_live: 2,
            ..AgentsConfig::default()
        };
        let manager = SubagentManager::new(config, TeamShared::default());
        let error = manager
            .reserve_spawns((1..=3).map(|n| attempt("fix", n)).collect())
            .unwrap_err();
        assert!(error.contains("too many live agents (2)"), "{error}");
        assert!(manager.runs_json().is_empty());
        let slots = manager
            .reserve_spawns((1..=2).map(|n| attempt("fix", n)).collect())
            .unwrap();
        assert_eq!(slots[1].path, "/root/fix_a2");
        let rows = manager.runs_json();
        assert_eq!(rows[1]["attempt_group"], "fix");
        assert_eq!(rows[1]["attempt"], 2);
        assert_eq!(rows[0]["attempt"], 1);
    }

    #[test]
    fn a_finished_group_is_compared_and_pick_attempt_keeps_one() {
        let repo = temp_dir("attempts");
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), ".lynshen/\n").unwrap();
        std::fs::write(repo.join("a.txt"), LINES).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let manager = SubagentManager::default();
        manager
            .reserve_spawns((1..=3).map(|n| attempt("fix", n)).collect())
            .unwrap();
        let mut roots = Vec::new();
        for n in 1..=3 {
            let path = format!("/root/fix_a{n}");
            let workspace = prepare_workspace(&repo, &format!("fix_a{n}")).unwrap();
            manager.register_workspace(&path, &workspace, &repo);
            manager.mark_running(&path);
            std::fs::write(
                workspace.root.join("a.txt"),
                LINES.replace("two", &format!("two-{n}")),
            )
            .unwrap();
            roots.push(workspace.root);
        }
        std::fs::write(roots[1].join("extra.txt"), "more\n").unwrap();
        manager.finish_ok("/root/fix_a1", run_result(1, 1));
        manager.finish_ok("/root/fix_a2", run_result(1, 1));
        // Not all attempts finished: the group's wait goes on.
        let waiting = manager
            .wait_agents("/root", vec!["fix".to_string()], 100)
            .unwrap();
        assert_eq!(waiting["timed_out"], true);
        assert!(waiting.get("attempts").is_none());
        let refused = manager.pick_attempt("/root", "fix", "fix_a3").unwrap_err();
        assert!(refused.contains("still running"), "{refused}");

        manager.finish_ok("/root/fix_a3", run_result(1, 1));
        let result = manager
            .wait_agents("/root", vec!["fix".to_string()], 100)
            .unwrap();
        assert_eq!(result["timed_out"], false);
        let attempts = result["attempts"]["fix"].as_array().unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[1]["path"], "/root/fix_a2");
        assert_eq!(attempts[1]["attempt"], 2);
        assert_eq!(attempts[1]["files"], json!(["a.txt", "extra.txt"]));
        assert_eq!(
            (attempts[1]["added"].clone(), attempts[1]["removed"].clone()),
            (json!(2), json!(1))
        );
        assert_eq!(attempts[0]["files"], json!(["a.txt"]));

        let error = manager.pick_attempt("/root", "fix", "other").unwrap_err();
        assert!(error.contains("not an attempt of fix"), "{error}");
        manager.drain_events();
        let picked = manager.pick_attempt("/root", "fix", "2").unwrap();
        assert_eq!(picked["picked"], "/root/fix_a2");
        assert_eq!(picked["merge"]["ok"], true);
        assert_eq!(picked["discarded"], json!(["/root/fix_a1", "/root/fix_a3"]));
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            LINES.replace("two", "two-2")
        );
        assert!(repo.join("extra.txt").exists());
        assert!(roots.iter().all(|root| !root.exists()));
        assert_eq!(merge_events(&manager).len(), 3);
        let states: Vec<Value> = manager
            .runs_json()
            .iter()
            .map(|row| row["state"].clone())
            .collect();
        assert_eq!(
            states,
            vec![json!("discarded"), json!("merged"), json!("discarded")]
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn resume_needs_a_finished_agent_with_its_conversation() {
        let manager = SubagentManager::default();
        manager.reserve_spawn(spawn("helper")).unwrap();
        manager.mark_running("/root/helper");
        let error = manager
            .reserve_resume("/root", "helper", "more")
            .err()
            .unwrap();
        assert!(error.contains("still running"), "{error}");
        manager.finish_ok("/root/helper", run_result(1, 1));
        let error = manager
            .reserve_resume("/root", "helper", "more")
            .err()
            .unwrap();
        assert!(error.contains("no saved conversation"), "{error}");
        let error = manager
            .reserve_resume("/root/other", "/root/helper", "more")
            .err()
            .unwrap();
        assert!(error.contains("not one of your subagents"), "{error}");
    }

    #[test]
    fn the_board_is_shared_and_reports_each_change() {
        let manager = SubagentManager::default();
        let revision = manager.revision();
        let created = manager
            .task_create(NewTask {
                title: "Parser".to_string(),
                ..NewTask::default()
            })
            .unwrap();
        assert_eq!(created, json!({ "id": "t1" }));
        // A subagent on another thread sees and claims the same board.
        let other = manager.clone();
        let claimed = std::thread::spawn(move || other.task_update("/root/w", "t1", "claim", None))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(claimed.owner.as_deref(), Some("/root/w"));
        assert_eq!(manager.task_list()["tasks"][0]["status"], "claimed");
        assert!(manager.revision() > revision);
        assert_eq!(
            manager
                .drain_events()
                .into_iter()
                .filter(|event| *event == TeamEvent::Board)
                .count(),
            1
        );
        assert!(manager
            .task_update("/root/x", "t1", "complete", None)
            .is_err());
    }

    #[test]
    fn a_hook_report_reaches_the_main_agent_and_wakes_it() {
        let manager = SubagentManager::default();
        manager.report_hook(crate::hooks::HookReport {
            event: "task_completed",
            ok: false,
            text: "2 tests failed".to_string(),
        });
        assert!(manager.drain_events().contains(&TeamEvent::Message {
            from: "hook:task_completed".to_string(),
            to: ROOT_PATH.to_string(),
            summary: "2 tests failed".to_string(),
        }));
        let mail = manager.take_wake();
        assert_eq!(
            mail[0].model_text(),
            "<hook_result hook=\"task_completed\" ok=\"false\">\n2 tests failed\n</hook_result>"
        );
        // Plain mail waits for the next turn instead.
        manager.tell_root("review_on_complete", "no reviewer");
        assert!(manager.take_wake().is_empty());
        assert_eq!(manager.drain_messages(ROOT_PATH).len(), 1);
    }

    #[test]
    fn the_team_survives_a_restart_without_worktrees_that_are_gone() {
        let (repo, manager, workspace) = merge_fixture("restart");
        std::fs::write(workspace.root.join("c.txt"), "child\n").unwrap();
        manager.finish_ok("/root/worker", run_result(1, 1));
        manager.reserve_spawn(spawn("gone")).unwrap();
        let gone = prepare_workspace(&repo, "gone").unwrap();
        manager.register_workspace("/root/gone", &gone, &repo);
        manager.reserve_spawn(spawn("busy")).unwrap();
        let busy = prepare_workspace(&repo, "busy").unwrap();
        manager.register_workspace("/root/busy", &busy, &repo);
        manager.mark_running("/root/busy");
        manager
            .task_create(NewTask {
                title: "Ship".to_string(),
                ..NewTask::default()
            })
            .unwrap();
        let saved = manager.team_json();
        std::fs::remove_dir_all(&gone.root).unwrap();

        let restored = SubagentManager::restore(AgentsConfig::default(), &saved);
        assert_eq!(restored.board_json(), manager.board_json());
        let rows = restored.runs_json();
        let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["/root/worker", "/root/busy"]);
        assert_eq!(rows[0]["state"], "completed");
        assert_eq!(rows[0]["isolation"], "worktree");
        // It was running when the engine stopped.
        assert_eq!(rows[1]["state"], "interrupted");
        let result = restored.merge_agent("/root", "worker", "apply").unwrap();
        assert_eq!(result["files"], json!(["c.txt"]));
        assert!(repo.join("c.txt").exists());
        // Once merged it is no longer saved.
        let saved = restored.team_json();
        assert_eq!(saved["worktrees"].as_array().unwrap().len(), 1);
        assert_eq!(
            SubagentManager::restore(AgentsConfig::default(), &Value::Null).runs_json(),
            Vec::<Value>::new()
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn a_new_budget_window_resets_the_count_and_old_agents_are_forgotten() {
        let config = AgentsConfig {
            turn_token_budget: 100,
            ..AgentsConfig::default()
        };
        let manager = SubagentManager::new(config.clone(), TeamShared::default());
        manager.reserve_spawn(spawn("a")).unwrap();
        manager.record("/root/a", &usage(90, 20));
        manager.finish_ok("/root/a", run_result(90, 20));
        assert!(manager.budget_exhausted());
        // A turn the engine starts for a result continues the window.
        manager.begin_turn(config.clone(), false);
        assert!(manager.budget_exhausted());
        manager.begin_turn(config.clone(), true);
        assert!(!manager.budget_exhausted());
        assert!(manager
            .drain_messages(ROOT_PATH)
            .iter()
            .all(|message| message.from != "team_budget"));

        for index in 0..MAX_KEPT_AGENTS + 3 {
            let path = format!("/root/old_{index}");
            manager
                .reserve_spawn(spawn(&format!("old_{index}")))
                .unwrap();
            manager.finish_ok(&path, run_result(1, 1));
        }
        manager.begin_turn(config, true);
        let rows = manager.runs_json();
        assert_eq!(rows.len(), MAX_KEPT_AGENTS);
        assert!(!rows.iter().any(|row| row["id"] == "/root/a"));
        // A finished agent's name can be used again.
        manager.reserve_spawn(spawn("old_30")).unwrap();
    }
}
