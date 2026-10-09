//! The team's shared task board: one per main session, seen by the main
//! agent and every subagent (`task_create`, `task_list`, `task_update`),
//! persisted with the session and sent to clients as `task_board`.

use serde_json::{json, Value};

/// The main agent's path: it may act on any task.
const ROOT: &str = crate::subagents::ROOT_PATH;
const MAX_TASKS: usize = 100;
const MAX_TITLE_CHARS: usize = 300;
const MAX_TEXT_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Pending,
    Claimed,
    Completed,
    Failed,
    Blocked,
}

impl TaskStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "claimed" => Self::Claimed,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Task {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub status: TaskStatus,
    /// The agent path that claimed it.
    pub owner: Option<String>,
    pub depends_on: Vec<String>,
    /// The role that should do it (a hint; `worker` tasks get a review
    /// with `agents.review_on_complete`).
    pub role: Option<String>,
    pub files: Vec<String>,
    pub result: Option<String>,
    pub updated_at: u64,
}

impl Task {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "detail": self.detail,
            "status": self.status.as_str(),
            "owner": self.owner,
            "depends_on": self.depends_on,
            "role": self.role,
            "files": self.files,
            "result": self.result,
            "updated_at": self.updated_at,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        let list = |key: &str| {
            value[key]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        Some(Self {
            id: text("id")?,
            title: text("title").unwrap_or_default(),
            detail: text("detail").unwrap_or_default(),
            status: TaskStatus::parse(value["status"].as_str()?)?,
            owner: text("owner"),
            depends_on: list("depends_on"),
            role: text("role"),
            files: list("files"),
            result: text("result"),
            updated_at: value["updated_at"].as_u64().unwrap_or_default(),
        })
    }
}

/// A new task, as `task_create` gives it.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewTask {
    pub title: String,
    pub detail: String,
    pub depends_on: Vec<String>,
    pub role: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Board {
    /// The number of the next task id (`t1`, `t2`, …).
    next: u64,
    tasks: Vec<Task>,
}

impl Board {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub(crate) fn tasks_json(&self) -> Vec<Value> {
        self.tasks.iter().map(Task::to_json).collect()
    }

    /// Adds a task; its dependencies must exist. A task that depends on a
    /// failed one starts blocked.
    pub(crate) fn create(&mut self, task: NewTask, now: u64) -> Result<Task, String> {
        let title = task.title.trim();
        if title.is_empty() {
            return Err("title is required".to_string());
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(format!("the board is full ({MAX_TASKS} tasks)"));
        }
        let mut depends_on = Vec::new();
        for id in task.depends_on.iter().map(|id| id.trim()) {
            if self.get(id).is_none() {
                return Err(format!("depends_on names no task: {id}"));
            }
            if !depends_on.iter().any(|known| known == id) {
                depends_on.push(id.to_string());
            }
        }
        self.next = self.next.max(self.tasks.len() as u64) + 1;
        let failed = depends_on.iter().any(|id| {
            self.get(id)
                .is_some_and(|dep| dep.status == TaskStatus::Failed)
        });
        let created = Task {
            id: format!("t{}", self.next),
            title: cut(title, MAX_TITLE_CHARS),
            detail: cut(task.detail.trim(), MAX_TEXT_CHARS),
            status: if failed {
                TaskStatus::Blocked
            } else {
                TaskStatus::Pending
            },
            owner: None,
            depends_on,
            role: task
                .role
                .map(|role| role.trim().to_string())
                .filter(|role| !role.is_empty()),
            files: task
                .files
                .into_iter()
                .map(|file| file.trim().to_string())
                .filter(|file| !file.is_empty())
                .collect(),
            result: None,
            updated_at: now,
        };
        self.tasks.push(created.clone());
        Ok(created)
    }

    /// `task_update` by `requester` (an agent path). `claim` takes a pending
    /// task whose dependencies are all completed; only the owner, or the
    /// main agent, may `release`, `complete`, `fail` or `block` it. A failed
    /// task blocks every pending task that depends on it, directly or not.
    /// `result` is kept on the task (the outcome, or why it failed or is
    /// blocked).
    pub(crate) fn update(
        &mut self,
        requester: &str,
        id: &str,
        action: &str,
        result: Option<&str>,
        now: u64,
    ) -> Result<Task, String> {
        let id = id.trim();
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| format!("no task {id}"))?;
        let task = &self.tasks[index];
        let root = requester == ROOT;
        let owner = task.owner.as_deref() == Some(requester);
        let status = task.status;
        let describe = || match &task.owner {
            Some(owner) => format!("{id} is {} by {owner}", status.as_str()),
            None => format!("{id} is {}", status.as_str()),
        };
        let may_act = owner || root;
        let next = match action {
            "claim" => {
                if status != TaskStatus::Pending {
                    return Err(describe());
                }
                let waiting: Vec<&str> = task
                    .depends_on
                    .iter()
                    .filter(|dep| {
                        self.get(dep)
                            .is_none_or(|dep| dep.status != TaskStatus::Completed)
                    })
                    .map(String::as_str)
                    .collect();
                if !waiting.is_empty() {
                    return Err(format!("{id} waits for {}", waiting.join(", ")));
                }
                TaskStatus::Claimed
            }
            "release" | "complete" | "fail" | "block" if !may_act => {
                return Err(match &task.owner {
                    Some(owner) => format!(
                        "{id} belongs to {owner}; only it or the main agent may {action} it"
                    ),
                    None => format!("claim {id} before you {action} it"),
                });
            }
            "release" => match status {
                TaskStatus::Claimed | TaskStatus::Blocked => TaskStatus::Pending,
                _ => return Err(describe()),
            },
            "complete" => match status {
                TaskStatus::Claimed => TaskStatus::Completed,
                TaskStatus::Pending | TaskStatus::Blocked if root => TaskStatus::Completed,
                _ => return Err(describe()),
            },
            "fail" => match status {
                TaskStatus::Claimed => TaskStatus::Failed,
                TaskStatus::Pending | TaskStatus::Blocked if root => TaskStatus::Failed,
                _ => return Err(describe()),
            },
            "block" => match status {
                TaskStatus::Claimed => TaskStatus::Blocked,
                TaskStatus::Pending if root => TaskStatus::Blocked,
                _ => return Err(describe()),
            },
            other => {
                return Err(format!(
                    "action must be claim, release, complete, fail or block, got \"{other}\""
                ))
            }
        };
        let task = &mut self.tasks[index];
        task.status = next;
        task.updated_at = now;
        match action {
            "claim" => task.owner = Some(requester.to_string()),
            "release" => task.owner = None,
            _ => {}
        }
        if let Some(result) = result.map(str::trim).filter(|text| !text.is_empty()) {
            task.result = Some(cut(result, MAX_TEXT_CHARS));
        }
        let updated = task.clone();
        if next == TaskStatus::Failed {
            self.block_dependents(id, now);
        }
        Ok(updated)
    }

    fn block_dependents(&mut self, failed: &str, now: u64) {
        let mut blocked = vec![failed.to_string()];
        let mut index = 0;
        while index < blocked.len() {
            let id = blocked[index].clone();
            for task in &mut self.tasks {
                if task.status == TaskStatus::Pending && task.depends_on.contains(&id) {
                    task.status = TaskStatus::Blocked;
                    task.updated_at = now;
                    blocked.push(task.id.clone());
                }
            }
            index += 1;
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({ "next": self.next, "tasks": self.tasks_json() })
    }

    pub(crate) fn from_json(value: &Value) -> Self {
        let tasks: Vec<Task> = value["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Task::from_json)
            .collect();
        Self {
            next: value["next"]
                .as_u64()
                .unwrap_or_default()
                .max(tasks.len() as u64),
            tasks,
        }
    }
}

fn cut(text: &str, limit: usize) -> String {
    let mut cut: String = text.chars().take(limit).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(title: &str, depends_on: &[&str]) -> NewTask {
        NewTask {
            title: title.to_string(),
            depends_on: depends_on.iter().map(|id| id.to_string()).collect(),
            ..NewTask::default()
        }
    }

    #[test]
    fn tasks_get_ids_in_order_and_dependencies_must_exist() {
        let mut board = Board::default();
        assert_eq!(board.create(new("Parser", &[]), 1).unwrap().id, "t1");
        let tests = board
            .create(
                NewTask {
                    detail: " write the tests ".to_string(),
                    role: Some("worker".to_string()),
                    files: vec!["src/parse.rs".to_string(), " ".to_string()],
                    ..new("Tests", &["t1", "t1"])
                },
                2,
            )
            .unwrap();
        assert_eq!(tests.id, "t2");
        assert_eq!(tests.depends_on, ["t1"]);
        assert_eq!(tests.files, ["src/parse.rs"]);
        assert_eq!(
            tests.to_json(),
            json!({
                "id": "t2", "title": "Tests", "detail": "write the tests",
                "status": "pending", "owner": null, "depends_on": ["t1"],
                "role": "worker", "files": ["src/parse.rs"], "result": null,
                "updated_at": 2
            })
        );
        assert!(board
            .create(new("Docs", &["t9"]), 3)
            .unwrap_err()
            .contains("t9"));
        assert!(board
            .create(new("  ", &[]), 3)
            .unwrap_err()
            .contains("title"));
    }

    #[test]
    fn a_task_is_claimed_once_its_dependencies_are_completed() {
        let mut board = Board::default();
        board.create(new("Parser", &[]), 1).unwrap();
        board.create(new("Tests", &["t1"]), 1).unwrap();
        let error = board.update("/root/b", "t2", "claim", None, 2).unwrap_err();
        assert_eq!(error, "t2 waits for t1");
        let claimed = board.update("/root/a", "t1", "claim", None, 2).unwrap();
        assert_eq!(claimed.status, TaskStatus::Claimed);
        assert_eq!(claimed.owner.as_deref(), Some("/root/a"));
        // Claimed once: a second claim fails, even by the same agent.
        let error = board.update("/root/b", "t1", "claim", None, 3).unwrap_err();
        assert_eq!(error, "t1 is claimed by /root/a");
        // Only the owner (or the main agent) completes it.
        let error = board
            .update("/root/b", "t1", "complete", Some("done"), 3)
            .unwrap_err();
        assert!(error.contains("belongs to /root/a"), "{error}");
        let done = board
            .update("/root/a", "t1", "complete", Some("parser in place"), 4)
            .unwrap();
        assert_eq!(done.status, TaskStatus::Completed);
        assert_eq!(done.result.as_deref(), Some("parser in place"));
        assert_eq!(done.updated_at, 4);
        let tests = board.update("/root/b", "t2", "claim", None, 5).unwrap();
        assert_eq!(tests.owner.as_deref(), Some("/root/b"));
        // Release puts it back.
        let released = board.update("/root/b", "t2", "release", None, 6).unwrap();
        assert_eq!(
            (released.status, released.owner),
            (TaskStatus::Pending, None)
        );
        assert!(board
            .update("/root/b", "t2", "complete", None, 6)
            .unwrap_err()
            .contains("claim t2 before"));
        assert!(board
            .update("/root/b", "t2", "finish", None, 6)
            .unwrap_err()
            .contains("action must be"));
        assert_eq!(
            board.update("/root/b", "t7", "claim", None, 6).unwrap_err(),
            "no task t7"
        );
    }

    #[test]
    fn the_main_agent_may_act_on_any_task() {
        let mut board = Board::default();
        board.create(new("A", &[]), 1).unwrap();
        board.create(new("B", &[]), 1).unwrap();
        board.update("/root/a", "t1", "claim", None, 2).unwrap();
        board
            .update(ROOT, "t1", "release", Some("reassigning"), 3)
            .unwrap();
        assert_eq!(board.get("t1").unwrap().owner, None);
        // An unclaimed task too.
        let done = board.update(ROOT, "t2", "complete", None, 4).unwrap();
        assert_eq!(done.status, TaskStatus::Completed);
        assert_eq!(done.owner, None);
        let blocked = board
            .update(ROOT, "t1", "block", Some("needs a key"), 5)
            .unwrap();
        assert_eq!(blocked.status, TaskStatus::Blocked);
        assert_eq!(blocked.result.as_deref(), Some("needs a key"));
        assert_eq!(
            board.update(ROOT, "t1", "release", None, 6).unwrap().status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn a_failed_task_blocks_its_dependents() {
        let mut board = Board::default();
        board.create(new("Schema", &[]), 1).unwrap();
        board.create(new("Migration", &["t1"]), 1).unwrap();
        board.create(new("Backfill", &["t2"]), 1).unwrap();
        board.create(new("Docs", &[]), 1).unwrap();
        board.update("/root/a", "t1", "claim", None, 2).unwrap();
        board
            .update("/root/a", "t1", "fail", Some("no access"), 3)
            .unwrap();
        let status = |id: &str| board.get(id).unwrap().status;
        assert_eq!(status("t1"), TaskStatus::Failed);
        assert_eq!(status("t2"), TaskStatus::Blocked);
        assert_eq!(status("t3"), TaskStatus::Blocked);
        assert_eq!(status("t4"), TaskStatus::Pending);
        assert!(board
            .update("/root/b", "t2", "claim", None, 4)
            .unwrap_err()
            .contains("t2 is blocked"));
        // A new task on the failed one starts blocked.
        let late = board.create(new("Report", &["t1"]), 5).unwrap();
        assert_eq!(late.status, TaskStatus::Blocked);
    }

    #[test]
    fn the_board_round_trips_through_json() {
        let mut board = Board::default();
        board.create(new("A", &[]), 1).unwrap();
        board.create(new("B", &["t1"]), 1).unwrap();
        board.update("/root/a", "t1", "claim", None, 2).unwrap();
        let restored = Board::from_json(&board.to_json());
        assert_eq!(restored, board);
        // Ids keep counting after a reload.
        let mut restored = restored;
        assert_eq!(restored.create(new("C", &[]), 3).unwrap().id, "t3");
        assert!(Board::from_json(&Value::Null).is_empty());
    }
}
