//! Plan mode (`ApprovalMode::Plan`): the agent investigates with read-only
//! tools and delivers a plan through `propose_plan`; nothing else runs until
//! the user approves it (`approve_plan`). This module decides which calls
//! may run, and holds the prompt and tool text the mode adds.

use serde_json::{json, Value};

pub const TOOL_NAME: &str = "propose_plan";

/// Added to the main agent's system prompt for a turn that starts in plan mode.
pub const PROMPT_ADDENDUM: &str = r#"<plan_mode>
You are in plan mode. Only read-only tools run: reading, listing and searching files, read-only shell commands (for example git status/diff/log, ls, cat, grep, find), web search and fetch, and subagents (which are in plan mode too). Edits, writes and other changes are refused until the user approves a plan. If the user asks you to make changes, plan them instead.

1. Investigate first: read the relevant code, configuration and tests until you understand the current state. Do not ask the user anything you can find out yourself; ask only when you are blocked by a decision only they can make.
2. Deliver the plan by calling propose_plan with a short title and the plan in Markdown. Then stop: the user approves the plan or asks for changes.

Write the plan for scanning, with short sections and tables:
- **Goal & principles**: what the change achieves and the rules it follows.
- **Changes**: a table with the columns File | Change | Why.
- **Steps**: numbered, in the order you will do them.
- **Risks**: what could break, and how you avoid it.
- **Verification**: the tests and checks that prove it works.
Keep it decision-complete and concise: whoever executes it must not have to make new decisions. A revised plan replaces the earlier one completely.
</plan_mode>"#;

/// Added to a subagent's context when it starts in plan mode.
pub const SUBAGENT_NOTE: &str = " Plan mode applies to you too: only read-only tools run. Report your findings to the parent; do not propose a plan.";

/// The value of string field `key` in a JSON object still being written:
/// what has arrived of it so far, unescaped; None until the value starts.
pub fn partial_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let mut from = 0;
    while let Some(found) = json[from..].find(&pattern) {
        let after = from + found + pattern.len();
        let rest = json[after..].trim_start();
        if let Some(value) = rest.strip_prefix(':').map(str::trim_start) {
            return value.strip_prefix('"').map(unescape_partial);
        }
        from = after;
    }
    None
}

/// A JSON string body up to its closing quote, or to the end when the rest
/// has not arrived; an escape cut off at the end is left out.
fn unescape_partial(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some(c @ ('"' | '\\' | '/')) => out.push(c),
                Some('u') => {
                    let Some(high) = hex4(&mut chars) else { break };
                    let code = if (0xD800..0xDC00).contains(&high) {
                        // A surrogate pair: its low half follows as \uXXXX.
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            break;
                        }
                        let Some(low) = hex4(&mut chars) else { break };
                        0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF)
                    } else {
                        high
                    };
                    if let Some(c) = char::from_u32(code) {
                        out.push(c);
                    }
                }
                _ => break,
            },
            c => out.push(c),
        }
    }
    out
}

fn hex4(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<u32> {
    let digits: String = chars.by_ref().take(4).collect();
    (digits.len() == 4)
        .then(|| u32::from_str_radix(&digits, 16).ok())
        .flatten()
}

pub fn propose_plan_definition() -> Value {
    json!({
        "type": "function",
        "name": TOOL_NAME,
        "description": "Plan mode only: present your finished plan to the user for approval. This ends your turn; the user approves the plan (then you implement it) or asks for changes (then you propose a complete revised plan).",
        "parameters": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short title of the plan, in the user's language." },
                "plan": { "type": "string", "description": "The full plan in Markdown: goal & principles, a File | Change | Why table, numbered steps, risks, verification." }
            },
            "required": ["title", "plan"],
            "additionalProperties": false
        }
    })
}

/// Tools that never change anything.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "ls",
    "ripgrep",
    "outline",
    "web_fetch",
    "web_search",
    "get_goal",
    // Subagents share the session's live mode, so they are in plan mode too.
    "spawn_agent",
    "wait_agent",
    "list_agents",
    "send_message",
    "close_agent",
    "resume_agent",
    // The task board is the team's coordination, not the workspace.
    "task_create",
    "task_list",
    "task_update",
    TOOL_NAME,
];

/// Why a call is not read-only.
enum Change {
    /// A shell command that is not a known read-only command.
    Command(String),
    /// Any other tool that changes things.
    Tool,
}

/// What makes this call change something, or None when it only reads.
/// `mcp_read_only` is the MCP server's readOnlyHint for the tool, when it
/// gave one.
fn change(name: &str, arguments: &str, mcp_read_only: Option<bool>) -> Option<Change> {
    if READ_ONLY_TOOLS.contains(&name) {
        return None;
    }
    let args = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let allowed = match name {
        name if name.starts_with("mcp__") => mcp_read_only == Some(true),
        "bash" | "execute" | "exec_command" | "shell_command" => {
            let command = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let escalated = args.get("escalate").and_then(Value::as_bool) == Some(true);
            if escalated || !is_read_only_command(command) {
                return Some(Change::Command(command.trim().to_string()));
            }
            true
        }
        // Polling a running shell writes nothing.
        "write_stdin" => field("text").is_empty() && field("chars").is_empty(),
        "checkpoint" => field("action") == "list",
        _ => false,
    };
    (!allowed).then_some(Change::Tool)
}

/// Why plan mode refuses this call, or None when it may run. `mcp_read_only`
/// is the MCP server's readOnlyHint for the tool, when it gave one.
pub fn refusal(name: &str, arguments: &str, mcp_read_only: Option<bool>) -> Option<String> {
    change(name, arguments, mcp_read_only).map(|change| match change {
        Change::Command(command) => format!(
            "plan mode: `{command}` was not run because it is not a known read-only command. Only read-only commands run in plan mode (git status/diff/log/show, ls, cat, head, tail, grep, rg, find, wc and similar, optionally piped together). Use read, ls and ripgrep to investigate, and put the commands that change things into the plan you deliver with {TOOL_NAME}."
        ),
        Change::Tool => format!(
            "plan mode: {name} was not run. You are in plan mode, where only read-only tools run and nothing in the workspace may change. Finish investigating, then call {TOOL_NAME} with your plan; the user approves it before anything is changed."
        ),
    })
}

/// Why a subagent with a read-only role may not make this call (the same
/// rules as plan mode), or None when it may run.
pub fn read_only_refusal(
    name: &str,
    arguments: &str,
    mcp_read_only: Option<bool>,
) -> Option<String> {
    change(name, arguments, mcp_read_only).map(|change| match change {
        Change::Command(command) => format!(
            "read-only role: `{command}` was not run because it is not a known read-only command. Use read-only commands (git status/diff/log/show, ls, cat, grep, rg, find and similar) and report what should change."
        ),
        Change::Tool => format!(
            "read-only role: {name} was not run. Your role cannot change anything; report what should change to your parent instead."
        ),
    })
}

/// Programs that only read, whatever their arguments (apart from the
/// options `forbidden_option` rejects).
const READ_ONLY_PROGRAMS: &[&str] = &[
    "cat", "head", "tail", "wc", "ls", "pwd", "echo", "printf", "grep", "egrep", "fgrep", "rg",
    "find", "tree", "file", "stat", "du", "df", "which", "whereis", "sort", "cut", "tr", "diff",
    "cmp", "comm", "nl", "basename", "dirname", "realpath", "readlink", "uname", "whoami", "id",
    "jq", "true", "false", "cd",
];

/// git subcommands that only read.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "describe",
    "shortlog",
    "grep",
    "cat-file",
    "rev-list",
    "show-ref",
    "merge-base",
    "name-rev",
    "reflog",
];

/// Whether `command` provably only reads: a pipeline or list of known
/// read-only programs, with no redirection to files, no command
/// substitution and no background jobs. Anything else is not read-only.
/// Quotes and backslashes are read as the shell reads them: a `|`, `;` or
/// `>` inside a quoted argument (`grep "a\\|b"`) belongs to that argument.
pub fn is_read_only_command(command: &str) -> bool {
    shell_commands(command).is_some_and(|commands| {
        !commands.is_empty() && commands.iter().all(|words| words_are_read_only(words))
    })
}

/// The simple commands in `command`, each as its words with the quoting
/// removed. None when `command` substitutes a command, redirects output to
/// anything but /dev/null or another descriptor, runs something in the
/// background, groups commands in parentheses, or does not parse.
fn shell_commands(command: &str) -> Option<Vec<Vec<String>>> {
    let mut lexer = Lexer::default();
    let mut chars = command.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => lexer.end_word(),
            '\n' | '\r' | ';' => lexer.end_command(),
            '|' => {
                // `||` (or), `|&` (pipe both streams), `|` (pipe).
                if matches!(chars.peek(), Some('|' | '&')) {
                    chars.next();
                }
                lexer.end_command();
            }
            '&' => match chars.peek() {
                Some('&') => {
                    chars.next();
                    lexer.end_command();
                }
                // `&>/dev/null`, `&>>/dev/null`.
                Some('>') => {
                    chars.next();
                    lexer.end_word();
                    redirect_to_null(&mut chars)?;
                }
                // A background job.
                _ => return None,
            },
            '>' => {
                // The descriptor before `>` (`2>`) is not an argument.
                if lexer.word.chars().all(|d| d.is_ascii_digit()) && !lexer.quoted {
                    lexer.word.clear();
                    lexer.in_word = false;
                }
                lexer.end_word();
                redirect_to_null(&mut chars)?;
            }
            '<' => match chars.peek() {
                // Process substitution and here-documents.
                Some('(' | '<') => return None,
                // Input from a file only reads it.
                _ => lexer.end_word(),
            },
            '(' | ')' | '`' => return None,
            '$' => match chars.peek() {
                Some('(' | '{') => return None,
                _ => lexer.push('$'),
            },
            '\\' => match chars.next() {
                // A line continuation.
                Some('\n') => {}
                Some(next) => lexer.push(next),
                None => lexer.push('\\'),
            },
            '\'' => {
                lexer.quoted = true;
                lexer.in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => lexer.word.push(other),
                    }
                }
            }
            '"' => {
                lexer.quoted = true;
                lexer.in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '`' => return None,
                        '$' if matches!(chars.peek(), Some('(' | '{')) => return None,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\' | '$' | '`') => lexer.word.push(escaped),
                            '\n' => {}
                            other => {
                                lexer.word.push('\\');
                                lexer.word.push(other);
                            }
                        },
                        other => lexer.word.push(other),
                    }
                }
            }
            other => lexer.push(other),
        }
    }
    lexer.end_command();
    Some(lexer.commands)
}

/// After `>`, `>>` or `&>`: the redirection is harmless only when it goes to
/// /dev/null or duplicates a descriptor (`2>&1`).
fn redirect_to_null(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<()> {
    if chars.peek() == Some(&'>') {
        chars.next();
    }
    if chars.peek() == Some(&'&') {
        chars.next();
        let mut digits = 0;
        while chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
            digits += 1;
        }
        return (digits > 0).then_some(());
    }
    while matches!(chars.peek(), Some(' ' | '\t')) {
        chars.next();
    }
    let target: String = std::iter::from_fn(|| {
        chars.next_if(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | '<' | '>'))
    })
    .collect();
    (target == "/dev/null").then_some(())
}

/// Words and commands as the lexer collects them.
#[derive(Default)]
struct Lexer {
    commands: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    /// The current word has started (a quoted empty string is a word).
    in_word: bool,
    /// The current word has a quoted part.
    quoted: bool,
}

impl Lexer {
    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }

    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
        }
        self.in_word = false;
        self.quoted = false;
    }

    fn end_command(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
    }
}

fn words_are_read_only(words: &[String]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    // An environment assignment or a path could run anything.
    if program.contains(['=', '/']) {
        return false;
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match (program.as_str(), args.as_slice()) {
        ("git", _) => git_is_read_only(&args),
        // `node --version`: any program asked only for its version.
        (_, ["--version"]) => true,
        // `command -v node`, `type node`: where a command is.
        ("command", ["-v" | "-V", ..]) | ("type", _) => true,
        ("sed", _) => sed_only_prints(&args),
        // `uniq in out` writes `out`.
        ("uniq", _) => args.iter().filter(|arg| !arg.starts_with('-')).count() <= 1,
        _ => {
            READ_ONLY_PROGRAMS.contains(&program.as_str())
                && !args.iter().any(|arg| forbidden_option(program, arg))
        }
    }
}

/// `sed -n '60,200p' file` and the like: printing ranges of lines. Any other
/// script may write (`w`, `-i`) or run (`e`) something.
fn sed_only_prints(args: &[&str]) -> bool {
    let mut quiet = false;
    let mut script = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "-E" | "-r" | "--regexp-extended" => {}
            "-e" => script = rest.next().copied(),
            arg if arg.starts_with('-') => return false,
            arg if script.is_none() => script = Some(arg),
            _ => {}
        }
    }
    let address =
        |part: &str| part == "$" || (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    quiet
        && script.is_some_and(|script| {
            script.split(';').all(|command| {
                command.trim().strip_suffix('p').is_some_and(|range| {
                    range.split(',').count() <= 2 && range.split(',').all(address)
                })
            })
        })
}

/// Options that make an otherwise read-only program write or run something.
fn forbidden_option(program: &str, arg: &str) -> bool {
    match program {
        "find" => matches!(
            arg,
            "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-delete"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
                | "-fls"
        ),
        "sort" => {
            arg == "-o" || arg.starts_with("--output") || (arg.starts_with("-o") && arg.len() > 2)
        }
        "rg" => arg.starts_with("--pre"),
        "tree" => arg == "-o",
        _ => false,
    }
}

fn git_is_read_only(args: &[&str]) -> bool {
    let mut rest = args;
    // Global options before the subcommand.
    loop {
        match rest {
            ["-C", _, tail @ ..] => rest = tail,
            ["--no-pager" | "-P", tail @ ..] => rest = tail,
            _ => break,
        }
    }
    let Some((&subcommand, options)) = rest.split_first() else {
        return false;
    };
    if options
        .iter()
        .any(|arg| *arg == "-o" || arg.starts_with("--output") || arg.starts_with("--ext-diff"))
    {
        return false;
    }
    match subcommand {
        "branch" => options.iter().all(|arg| {
            matches!(
                *arg,
                "-a" | "-r"
                    | "-v"
                    | "-vv"
                    | "--list"
                    | "--all"
                    | "--remotes"
                    | "--show-current"
                    | "--verbose"
            )
        }),
        "remote" => options.iter().all(|arg| matches!(*arg, "-v" | "--verbose")),
        "tag" => options.iter().all(|arg| matches!(*arg, "-l" | "--list")),
        "stash" => options.first() == Some(&"list"),
        "config" => options.first().is_some_and(|arg| {
            matches!(
                *arg,
                "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"
            )
        }),
        other => READ_ONLY_GIT.contains(&other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_tools_run_and_changes_are_refused() {
        for tool in [
            "read",
            "ls",
            "ripgrep",
            "outline",
            "web_fetch",
            "web_search",
            "spawn_agent",
            "wait_agent",
            TOOL_NAME,
        ] {
            assert_eq!(refusal(tool, "{}", None), None, "{tool}");
        }
        for tool in [
            "write",
            "str_replace",
            "hashline_edit",
            "apply_patch",
            "generate_image",
            "update_plan",
            "create_goal",
            "report",
        ] {
            let reason = refusal(tool, "{}", None).expect(tool);
            assert!(reason.contains("plan mode"), "{reason}");
            assert!(reason.contains(TOOL_NAME), "{reason}");
        }
        assert_eq!(refusal("checkpoint", r#"{"action":"list"}"#, None), None);
        assert!(refusal("checkpoint", r#"{"action":"create","paths":["a"]}"#, None).is_some());
        assert_eq!(refusal("write_stdin", r#"{"session_id":1}"#, None), None);
        assert!(refusal(
            "write_stdin",
            r#"{"session_id":1,"text":"rm -rf x\n"}"#,
            None
        )
        .is_some());
    }

    #[test]
    fn a_read_only_role_gets_the_same_rules_with_its_own_message() {
        assert_eq!(read_only_refusal("read", "{}", None), None);
        assert_eq!(read_only_refusal("send_message", "{}", None), None);
        for board in ["task_create", "task_list", "task_update", "resume_agent"] {
            assert_eq!(read_only_refusal(board, "{}", None), None, "{board}");
        }
        assert!(read_only_refusal("pick_attempt", "{}", None).is_some());
        assert_eq!(
            read_only_refusal("bash", r#"{"command":"git diff HEAD"}"#, None),
            None
        );
        for (tool, args) in [
            ("hashline_edit", "{}"),
            ("write", "{}"),
            ("merge_agent", r#"{"target":"w","action":"apply"}"#),
            ("bash", r#"{"command":"cargo fmt"}"#),
            ("mcp__db__drop", "{}"),
        ] {
            let reason = read_only_refusal(tool, args, None).expect(tool);
            assert!(reason.starts_with("read-only role:"), "{reason}");
            assert!(!reason.contains(TOOL_NAME), "{reason}");
        }
    }

    #[test]
    fn mcp_tools_run_only_when_marked_read_only() {
        assert_eq!(refusal("mcp__docs__search", "{}", Some(true)), None);
        assert!(refusal("mcp__docs__search", "{}", None).is_some());
        assert!(refusal("mcp__db__drop", "{}", Some(false)).is_some());
    }

    #[test]
    fn shell_commands_run_only_when_provably_read_only() {
        for command in [
            "git status",
            "git -C crates/x --no-pager log --oneline -5",
            "git diff HEAD~1 -- src/lib.rs | head -50",
            "ls -la && cat Cargo.toml",
            "rg -n 'fn main' src | wc -l",
            "find . -name '*.rs' -type f 2>/dev/null | sort",
            "cd crates && grep -rn TODO .",
            "git branch -a",
            "git config --get user.name",
        ] {
            assert!(is_read_only_command(command), "{command}");
            assert_eq!(
                refusal("bash", &json!({ "command": command }).to_string(), None),
                None,
                "{command}"
            );
        }
        for command in [
            "",
            "cargo test",
            "rm -rf target",
            "echo hi > notes.txt",
            "cat a >> b",
            "ls; touch x",
            "git commit -m x",
            "git checkout main",
            "git branch new-branch",
            "git diff --output=patch.diff",
            "find . -name x -delete",
            "find . -exec rm {} ;",
            "sort -o out.txt in.txt",
            "rg --pre ./script x",
            "echo $(rm -rf x)",
            "cat `ls`",
            "sleep 10 &",
            "FOO=1 ls",
            "/bin/rm x",
            "xargs rm",
            "sed -i s/a/b/ f",
            "python -c 'print(1)'",
            "git stash",
        ] {
            assert!(!is_read_only_command(command), "{command:?}");
        }
        let escalated = json!({ "command": "ls", "escalate": true }).to_string();
        assert!(refusal("bash", &escalated, None).is_some());
        let refused = refusal("exec_command", r#"{"cmd":"cargo build"}"#, None).unwrap();
        assert!(refused.contains("`cargo build`"), "{refused}");
    }

    #[test]
    fn quoted_operators_are_part_of_their_argument() {
        // A grep alternation inside quotes is not a pipe (as reported from a
        // plan-mode session).
        for command in [
            r#"cd /Users/chad/Desktop/test && wc -l pelican-rider.html pelican-bike.html && grep -n "function\|const .*=\s*(" pelican-rider.html | head -80"#,
            r#"grep -n 'a|b;c > d' src/lib.rs"#,
            r#"grep -E "x (y|z)" f.txt | sort | head"#,
            r#"rg 'fn \w+\(' -g '*.rs'"#,
            r#"grep a\|b file"#,
            r#"echo '$(rm -rf x)' | wc -c"#,
            r#"find . \( -name '*.ts' -o -name '*.svelte' \) -type f"#,
            "wc -l < Cargo.toml",
            "ls 2>&1 | head",
            "ls 2> /dev/null; cat a &>/dev/null",
            "git log --format='%h %s' -3",
            "cat \"file;with;semicolons\"",
            "ls \\\n  -la",
            "cd /Users/chad/Desktop/test && sed -n '60,200p' pelican-rider.html",
            "sed -n -e '1,20p;40,$p' a.txt",
            "node --version; command -v node python3",
            "type cargo",
            "sort a.txt | uniq -c",
        ] {
            assert!(is_read_only_command(command), "{command}");
        }
        for command in [
            r#"echo "$(rm -rf x)""#,
            r#"echo "`rm -rf x`""#,
            r#"echo "${x:=1}""#,
            r#"grep "x" f > "out file.txt""#,
            r#"grep x f 2>err.log"#,
            r#"cat 'unterminated"#,
            r#"grep "a\"b f"#,
            "(cd x && rm -rf y)",
            "cat <(curl https://example.com)",
            "cat <<EOF\nhi\nEOF",
            "ls >&",
            r#"ls ";" ; rm x"#,
            r#"cat "a" | "sh""#,
            r#"find . -name '*.tmp' -exec rm {} \;"#,
            "sed -i s/a/b/ f",
            "sed -n '1,5w out.txt' f",
            "sed -n '1e rm -rf x' f",
            "sed 's/a/b/' f",
            r#"node -e 'require("fs").rmSync("x")'"#,
            "node --version --eval x",
            "uniq a.txt b.txt",
            "command rm x",
        ] {
            assert!(!is_read_only_command(command), "{command:?}");
        }
    }

    #[test]
    fn reads_a_string_field_while_it_is_still_being_written() {
        let whole =
            r###"{"title": "Web 版", "plan": "## Goal\n\n- \"one\" \u4e2d \ud83d\ude00\n"}"###;
        assert_eq!(partial_string_field(whole, "title").unwrap(), "Web 版");
        assert_eq!(
            partial_string_field(whole, "plan").unwrap(),
            "## Goal\n\n- \"one\" 中 😀\n"
        );
        // Cut anywhere: what arrived so far, an escape cut off left out.
        let cut = |json: &str| partial_string_field(json, "plan");
        assert_eq!(cut(r###"{"title":"T","plan":"## Go"###).unwrap(), "## Go");
        assert_eq!(cut(r###"{"title":"T","plan":"line\"###).unwrap(), "line");
        assert_eq!(cut(r###"{"title":"T","plan":"a\u4e"###).unwrap(), "a");
        assert_eq!(cut(r###"{"title":"T","pl"###), None);
        assert_eq!(cut(r###"{"title":"T","plan": "###), None);
        // A quoted "plan" inside the title is not the field.
        assert_eq!(cut(r###"{"title":"the \"plan\" one"}"###), None);
    }
}
