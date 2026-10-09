//! Subagent roles: named defaults for `spawn_agent` (model, effort, access,
//! isolation, budgets) plus instructions appended to the subagent's prompt.
//! Built-in roles can be replaced by a file of the same name in
//! `~/.lynshen/roles/<name>.md` (user) or `<cwd>/.lynshen/roles/<name>.md`
//! (project, only when the project is trusted); the project wins.

use std::{fs, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub description: String,
    /// Model the subagent runs on; None: the caller's choice or its own.
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// `access: read-only`: only read-only tools run (as in plan mode).
    pub read_only: bool,
    /// `isolation: worktree`: the subagent writes in its own worktree.
    pub worktree: bool,
    pub max_tool_calls: Option<u64>,
    pub timeout_secs: Option<u64>,
    /// The file's body, appended to the subagent's system prompt.
    pub instructions: String,
}

/// The roles a turn offers: built-ins, replaced by user files, replaced by
/// project files (when `project_trusted`), sorted by name. A file that does
/// not parse is skipped with a warning in the log.
pub fn discover(profile_dir: &Path, cwd: &Path, project_trusted: bool) -> Vec<Role> {
    let mut roles = builtin();
    let mut dirs = vec![profile_dir.join("roles")];
    if project_trusted {
        dirs.push(cwd.join(".lynshen").join("roles"));
    }
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            let parsed = fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse(&text, stem));
            match parsed {
                Ok(role) => {
                    roles.retain(|known| known.name != role.name);
                    roles.push(role);
                }
                Err(error) => crate::log_warn!(
                    "roles",
                    "role file skipped",
                    path = path.display().to_string(),
                    error = error
                ),
            }
        }
    }
    roles.sort_by(|left, right| left.name.cmp(&right.name));
    roles
}

/// Parses a role file: optional `---` frontmatter of `key: value` lines,
/// then the instructions. `name` defaults to `fallback_name` (the file name).
pub fn parse(text: &str, fallback_name: &str) -> Result<Role, String> {
    let (fields, body) = split_frontmatter(text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    };
    let number = |key: &str| -> Result<Option<u64>, String> {
        field(key)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{key} must be a whole number, got \"{value}\""))
            })
            .transpose()
    };
    let name = field("name").unwrap_or_else(|| fallback_name.to_string());
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "role name \"{name}\" must use lowercase letters, digits, - and _"
        ));
    }
    let read_only = match field("access").as_deref() {
        None | Some("inherit") => false,
        Some("read-only") => true,
        Some(other) => {
            return Err(format!(
                "access must be read-only or inherit, got \"{other}\""
            ))
        }
    };
    let worktree = match field("isolation").as_deref() {
        None | Some("none") => false,
        Some("worktree") => true,
        Some(other) => {
            return Err(format!(
                "isolation must be none or worktree, got \"{other}\""
            ))
        }
    };
    Ok(Role {
        name,
        description: field("description").unwrap_or_default(),
        model: field("model"),
        reasoning_effort: field("reasoning_effort"),
        read_only,
        worktree,
        max_tool_calls: number("max_tool_calls")?,
        timeout_secs: number("timeout_secs")?,
        instructions: body.trim().to_string(),
    })
}

/// `(key, value)` pairs of the leading `---` block and the text after it.
/// Without a frontmatter block the whole text is the body.
fn split_frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let mut fields = Vec::new();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (fields, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end();
        if line == "---" {
            return (fields, &rest[offset..]);
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            fields.push((key.trim().to_string(), value.to_string()));
        }
    }
    // An unclosed block: everything was frontmatter.
    (fields, "")
}

/// explorer, worker and reviewer.
pub fn builtin() -> Vec<Role> {
    let role = |name: &str, description: &str, read_only, worktree, instructions: &str| Role {
        name: name.to_string(),
        description: description.to_string(),
        model: None,
        reasoning_effort: None,
        read_only,
        worktree,
        max_tool_calls: None,
        timeout_secs: None,
        instructions: instructions.to_string(),
    };
    vec![
        role(
            "explorer",
            "read-only: finds code, answers questions.",
            true,
            false,
            "You are an explorer: you research and cannot change anything. Answer the question in your task with evidence: file paths, line numbers and short quotes.",
        ),
        role(
            "reviewer",
            "read-only, fresh context: reviews a diff. Use one on the merged diff of a non-trivial change.",
            true,
            false,
            "You are a reviewer: you never edit. Read the diff your task names (for example with git diff) and the code around it. Report each real problem (bug, broken edge case, missing test) with file:line and why it matters, most severe first. If you find none, say so.",
        ),
        role(
            "worker",
            "writes in its own worktree. Give workers separate files; merge each with merge_agent.",
            false,
            true,
            "You are a worker. Change only the files your task names, in your worktree, and run a focused check. Do not commit. End with the files you changed and how you verified them.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_fields_and_body() {
        let role = parse(
            "---\nname: tester\ndescription: \"runs the tests\"\nmodel: gpt-mini\nreasoning_effort: low\naccess: read-only\nisolation: worktree\nmax_tool_calls: 30\ntimeout_secs: 600\n---\n\nRun cargo test and report.\n",
            "file",
        )
        .unwrap();
        assert_eq!(role.name, "tester");
        assert_eq!(role.description, "runs the tests");
        assert_eq!(role.model.as_deref(), Some("gpt-mini"));
        assert_eq!(role.reasoning_effort.as_deref(), Some("low"));
        assert!(role.read_only);
        assert!(role.worktree);
        assert_eq!(role.max_tool_calls, Some(30));
        assert_eq!(role.timeout_secs, Some(600));
        assert_eq!(role.instructions, "Run cargo test and report.");
    }

    #[test]
    fn defaults_and_invalid_fields() {
        let role = parse("Just instructions.", "docs_writer").unwrap();
        assert_eq!(role.name, "docs_writer");
        assert!(!role.read_only && !role.worktree);
        assert_eq!(role.model, None);
        assert_eq!(role.instructions, "Just instructions.");

        assert!(parse("---\naccess: write\n---\n", "x")
            .unwrap_err()
            .contains("access"));
        assert!(parse("---\nisolation: vm\n---\n", "x")
            .unwrap_err()
            .contains("isolation"));
        assert!(parse("---\nmax_tool_calls: many\n---\n", "x")
            .unwrap_err()
            .contains("max_tool_calls"));
        assert!(parse("---\nname: Bad Name\n---\n", "x")
            .unwrap_err()
            .contains("role name"));
    }

    #[test]
    fn files_replace_builtins_and_project_roles_need_trust() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-roles-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("profile");
        let cwd = root.join("project");
        fs::create_dir_all(profile.join("roles")).unwrap();
        fs::create_dir_all(cwd.join(".lynshen/roles")).unwrap();
        fs::write(
            profile.join("roles/worker.md"),
            "---\ndescription: user worker\nisolation: none\n---\nuser body",
        )
        .unwrap();
        fs::write(
            cwd.join(".lynshen/roles/worker.md"),
            "---\ndescription: project worker\n---\nproject body",
        )
        .unwrap();
        fs::write(
            cwd.join(".lynshen/roles/broken.md"),
            "---\naccess: x\n---\n",
        )
        .unwrap();

        let names = |roles: &[Role]| roles.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        let untrusted = discover(&profile, &cwd, false);
        assert_eq!(names(&untrusted), ["explorer", "reviewer", "worker"]);
        let worker = untrusted.iter().find(|r| r.name == "worker").unwrap();
        assert_eq!(worker.description, "user worker");
        assert!(!worker.worktree);

        let trusted = discover(&profile, &cwd, true);
        let worker = trusted.iter().find(|r| r.name == "worker").unwrap();
        assert_eq!(worker.instructions, "project body");
        // The broken file is skipped; the built-ins stay.
        assert_eq!(names(&trusted), ["explorer", "reviewer", "worker"]);
        let _ = fs::remove_dir_all(root);
    }
}
