//! Tool names from other agents. Models trained on Claude Code, Codex,
//! OpenCode or Gemini CLI traces call `Bash`, `WebFetch`, `read_file` or
//! `TodoWrite` even when the request offers `bash`, `web_fetch`, `read` and
//! `update_plan`; a long session drifts there more often. A call whose name
//! matches no offered tool is mapped to the tool it means, with its
//! arguments translated where the shapes differ. A name that maps to nothing
//! gets an error that names the offered tools, so the model can correct it
//! instead of retrying the same call.
use serde_json::{json, Map, Value};

/// Another agent's tool name (compared without case, `_` or `-`) and the
/// LynShen tool it means.
const ALIASES: &[(&str, &str)] = &[
    ("bash", "bash"),
    ("shell", "bash"),
    ("runshellcommand", "bash"),
    ("executecommand", "bash"),
    ("execcommand", "bash"),
    ("terminal", "bash"),
    ("read", "read"),
    ("readfile", "read"),
    ("view", "read"),
    ("write", "write"),
    ("writefile", "write"),
    ("createfile", "write"),
    ("edit", "str_replace"),
    ("multiedit", "str_replace"),
    ("replace", "str_replace"),
    ("grep", "ripgrep"),
    ("rg", "ripgrep"),
    ("searchfilecontent", "ripgrep"),
    ("ls", "ls"),
    ("listdir", "ls"),
    ("listdirectory", "ls"),
    ("webfetch", "web_fetch"),
    ("fetch", "web_fetch"),
    ("websearch", "web_search"),
    ("googlewebsearch", "web_search"),
    ("todowrite", "update_plan"),
];

fn key(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The offered tool `name` means, with its arguments in that tool's shape;
/// None when `name` is offered as it is or means nothing offered.
pub(crate) fn canonicalize(
    name: &str,
    arguments: &str,
    offered: &[String],
) -> Option<(String, String)> {
    if offered.iter().any(|tool| tool == name) {
        return None;
    }
    let wanted = key(name);
    // Same name, other spelling: `Web_Fetch`, `Ripgrep`.
    let target = offered
        .iter()
        .find(|tool| key(tool) == wanted)
        .cloned()
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == wanted)
                .map(|(_, tool)| (*tool).to_string())
                .filter(|tool| offered.contains(tool))
        })?;
    let mut args = match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => map,
        // Unparsable arguments: the tool reports them as it would anyway.
        _ => return Some((target, arguments.to_string())),
    };
    translate(&wanted, &target, &mut args);
    Some((target, Value::Object(args).to_string()))
}

/// Argument shapes of the other agents' tools, in LynShen's.
fn translate(from: &str, to: &str, args: &mut Map<String, Value>) {
    rename(
        args,
        &[
            "file_path",
            "filePath",
            "absolute_path",
            "filename",
            "file",
            "dir_path",
            "directory",
        ],
        "path",
    );
    match to {
        "bash" => {
            rename(args, &["cmd"], "command");
            // Codex: ["bash", "-lc", "…"] or argv.
            if let Some(Value::Array(argv)) = args.get("command").cloned() {
                let parts: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
                let line = match parts.as_slice() {
                    [_, flag, script] if flag.starts_with('-') && flag.contains('c') => {
                        (*script).to_string()
                    }
                    parts => parts.join(" "),
                };
                args.insert("command".to_string(), json!(line));
            }
            // Claude Code's Bash takes milliseconds; bash takes seconds.
            if from == "bash" {
                if let Some(ms) = args.get("timeout").and_then(Value::as_u64) {
                    args.insert("timeout".to_string(), json!(ms.div_ceil(1000).max(1)));
                }
            }
            args.remove("description");
            args.remove("run_in_background");
        }
        "str_replace" => {
            // Edit: one old/new pair; MultiEdit: a list of them.
            let pair = |edit: &Map<String, Value>| {
                json!({
                    "oldText": edit.get("old_string").or_else(|| edit.get("old_str")).cloned().unwrap_or_default(),
                    "newText": edit.get("new_string").or_else(|| edit.get("new_str")).cloned().unwrap_or_default(),
                })
            };
            if !args.contains_key("edits") {
                let edit = pair(args);
                args.insert("edits".to_string(), json!([edit]));
            } else if let Some(Value::Array(edits)) = args.get("edits").cloned() {
                let edits: Vec<Value> = edits
                    .iter()
                    .map(|edit| match edit.as_object() {
                        Some(edit)
                            if edit.contains_key("old_string") || edit.contains_key("old_str") =>
                        {
                            pair(edit)
                        }
                        _ => edit.clone(),
                    })
                    .collect();
                args.insert("edits".to_string(), json!(edits));
            }
            for key in [
                "old_string",
                "new_string",
                "old_str",
                "new_str",
                "replace_all",
            ] {
                args.remove(key);
            }
        }
        "ripgrep" => rename(args, &["query", "regex"], "pattern"),
        "web_fetch" => {
            // WebFetch's question for a reader model: the page comes back whole.
            args.remove("prompt");
        }
        "update_plan" => {
            // TodoWrite: {todos: [{content, status}]}.
            if let Some(Value::Array(todos)) = args.remove("todos") {
                let plan: Vec<Value> = todos
                    .iter()
                    .map(|todo| {
                        json!({
                            "step": todo.get("content").or_else(|| todo.get("step")).cloned().unwrap_or_default(),
                            "status": todo.get("status").cloned().unwrap_or(json!("pending")),
                        })
                    })
                    .collect();
                args.insert("plan".to_string(), json!(plan));
            }
        }
        _ => {}
    }
}

fn rename(args: &mut Map<String, Value>, from: &[&str], to: &str) {
    if args.contains_key(to) {
        return;
    }
    if let Some(value) = from.iter().find_map(|key| args.remove(*key)) {
        args.insert(to.to_string(), value);
    }
}

/// The error for a call to a tool that is not offered: what is.
pub(crate) fn unknown_tool_message(name: &str, offered: &[String]) -> String {
    let mut names: Vec<&str> = offered.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.dedup();
    format!(
        "unknown tool `{name}`. Tool names are exact; the tools you can call are: {}.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered() -> Vec<String> {
        [
            "bash",
            "read",
            "write",
            "str_replace",
            "ripgrep",
            "ls",
            "web_fetch",
            "web_search",
            "update_plan",
            "mcp__node__js",
        ]
        .map(String::from)
        .to_vec()
    }

    fn call(name: &str, args: Value) -> (String, Value) {
        let (name, args) = canonicalize(name, &args.to_string(), &offered()).expect("mapped");
        (name, serde_json::from_str(&args).unwrap())
    }

    #[test]
    fn offered_names_pass_untouched() {
        assert_eq!(
            canonicalize("bash", r#"{"command":"ls"}"#, &offered()),
            None
        );
        assert_eq!(canonicalize("mcp__node__js", "{}", &offered()), None);
    }

    #[test]
    fn claude_code_calls_reach_the_tools_they_mean() {
        let (name, args) = call(
            "Bash",
            json!({ "command": "echo ok", "description": "test", "timeout": 120000 }),
        );
        assert_eq!(
            (name.as_str(), args),
            ("bash", json!({ "command": "echo ok", "timeout": 120 }))
        );
        let (name, args) = call(
            "WebFetch",
            json!({ "url": "https://a.b", "prompt": "summarise" }),
        );
        assert_eq!(
            (name.as_str(), args),
            ("web_fetch", json!({ "url": "https://a.b" }))
        );
        let (name, args) = call("Read", json!({ "file_path": "/x/a.rs", "offset": 10 }));
        assert_eq!(
            (name.as_str(), args),
            ("read", json!({ "path": "/x/a.rs", "offset": 10 }))
        );
        let (name, args) = call(
            "Edit",
            json!({ "file_path": "a.rs", "old_string": "x", "new_string": "y" }),
        );
        assert_eq!(
            (name.as_str(), args),
            (
                "str_replace",
                json!({ "path": "a.rs", "edits": [{ "oldText": "x", "newText": "y" }] })
            )
        );
        let (_, args) = call(
            "MultiEdit",
            json!({ "file_path": "a.rs", "edits": [{ "old_string": "x", "new_string": "y" }] }),
        );
        assert_eq!(args["edits"], json!([{ "oldText": "x", "newText": "y" }]));
        let (name, args) = call(
            "TodoWrite",
            json!({ "todos": [{ "content": "Fix it", "status": "in_progress", "activeForm": "Fixing" }] }),
        );
        assert_eq!(
            (name.as_str(), args),
            (
                "update_plan",
                json!({ "plan": [{ "step": "Fix it", "status": "in_progress" }] })
            )
        );
        assert_eq!(call("WebSearch", json!({ "query": "q" })).0, "web_search");
        assert_eq!(call("Grep", json!({ "pattern": "fn main" })).0, "ripgrep");
    }

    #[test]
    fn other_spellings_and_agents() {
        assert_eq!(call("Web_Fetch", json!({ "url": "u" })).0, "web_fetch");
        assert_eq!(
            call("read_file", json!({ "absolute_path": "a" })).1,
            json!({ "path": "a" })
        );
        let (name, args) = call("shell", json!({ "command": ["bash", "-lc", "cargo test"] }));
        assert_eq!(
            (name.as_str(), args),
            ("bash", json!({ "command": "cargo test" }))
        );
        // Codex's timeout is not Claude Code's milliseconds.
        assert_eq!(
            call("shell", json!({ "command": "ls", "timeout": 30 })).1["timeout"],
            30
        );
    }

    #[test]
    fn a_tool_that_is_not_offered_stays_unknown_with_the_list() {
        let only_read = vec!["read".to_string()];
        assert_eq!(
            canonicalize("Bash", r#"{"command":"ls"}"#, &only_read),
            None
        );
        assert_eq!(canonicalize("Glob", r#"{"pattern":"*"}"#, &offered()), None);
        let message = unknown_tool_message("Bash", &only_read);
        assert!(
            message.contains("`Bash`") && message.contains("read"),
            "{message}"
        );
    }
}
