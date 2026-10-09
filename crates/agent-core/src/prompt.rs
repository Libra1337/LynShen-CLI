use std::{
    fs, io,
    path::{Path, PathBuf},
};

const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

/// How to use the tools in a coding session, naming only the edit tools that
/// are enabled. Behavior (finish the task, verify, ...) is the base prompt's.
fn tool_guidance(edit_tools: &[String]) -> String {
    let enabled = |name: &str| edit_tools.iter().any(|tool| tool == name);
    let edit_names: Vec<&str> = crate::config::EDIT_TOOL_NAMES
        .into_iter()
        .filter(|name| enabled(name))
        .collect();
    let mut guidance = "Tool use: explore with read, ls and ripgrep; run commands with bash. Make independent calls (reads, searches, checks) together in one response.".to_string();
    if edit_names.is_empty() {
        guidance.push_str(" No file-edit tools are enabled: describe the changes instead.");
        return guidance;
    }
    guidance.push_str(&format!(
        " Read a file before you edit it. Edit files with {}.",
        edit_names.join(" or ")
    ));
    guidance.push_str(if enabled("write") {
        " Create new files with write."
    } else if enabled("apply_patch") {
        " Create new files with apply_patch."
    } else {
        " Create new files with bash (for example a heredoc)."
    });
    guidance
}

#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub date: String,
    pub cwd: PathBuf,
    pub edit_tools: Vec<String>,
    pub project_instructions: Vec<ProjectInstruction>,
    pub skills: Vec<SkillPromptItem>,
    /// A chat session: its base prompt carries its own tool guidance, and
    /// plan mode does not apply.
    pub chat: bool,
    /// The shell sandbox's note; empty without a sandbox.
    pub sandbox: String,
    pub plan_mode: bool,
    /// What the host adds to every turn (a daemon agent's brief and peers);
    /// empty without a host.
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstruction {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPromptItem {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    pub command: String,
    pub skill: SkillPromptItem,
}

/// Where the per-session part of the system prompt begins: everything before
/// it is the stable prefix providers cache across sessions and subagents.
pub(crate) const ENV_START: &str = "\n\n<env>\n";

/// The length of the system prompt's stable prefix (see `ENV_START`).
pub(crate) fn stable_prefix_len(system_prompt: &str) -> Option<usize> {
    system_prompt.find(ENV_START)
}

/// The system prompt, ordered for prompt caching: what is the same across
/// sessions and turns comes first, so providers can reuse the cached prefix.
/// Base prompt and tool guidance (fixed per config), project instructions
/// and skills (fixed per project), the sandbox note, then the working
/// directory and the date (the date changes daily), plan mode (changes when
/// the user switches mode) and last the host's text (may change every turn).
pub fn build_system_prompt(base: &str, context: &PromptContext) -> String {
    let mut prompt = base.trim_end().to_string();
    if !context.chat {
        prompt.push_str("\n\n");
        prompt.push_str(&tool_guidance(&context.edit_tools));
    }

    if !context.project_instructions.is_empty() {
        prompt.push_str("\n\n<project_context>\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for instruction in &context.project_instructions {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                escape_xml(&instruction.path.display().to_string()),
                instruction.content.trim_end()
            ));
        }
        prompt.push_str("</project_context>");
    }

    prompt.push_str(&skills_block(&context.skills));
    if !context.sandbox.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(context.sandbox.trim_end());
    }
    prompt.push_str(&format!(
        "{ENV_START}Working directory: {}\nDate: {}\n</env>",
        context.cwd.display(),
        context.date
    ));
    if context.plan_mode && !context.chat {
        prompt.push_str("\n\n");
        prompt.push_str(crate::plan_mode::PROMPT_ADDENDUM);
    }
    if !context.host.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(context.host.trim_end());
    }
    prompt
}

/// The part of the system prompt that lists the skills ("" without any).
pub fn skills_block(skills: &[SkillPromptItem]) -> String {
    let mut block = String::new();
    if skills.is_empty() {
        return block;
    }
    block.push_str("\n\nSkills: when a task matches a skill's description, read its file first. Resolve relative paths in a skill against its directory.\n<available_skills>\n");
    for skill in skills {
        block.push_str(&format!(
            "<skill name=\"{}\" location=\"{}\">{}</skill>\n",
            escape_xml(&skill.name),
            escape_xml(&skill.path.display().to_string()),
            escape_xml(&skill.description)
        ));
    }
    block.push_str("</available_skills>");
    block
}

pub fn discover_skills(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> io::Result<Vec<SkillPromptItem>> {
    discover_skills_from(
        profile_dir,
        cwd,
        project_trusted,
        crate::secrets::home_dir().map(|home| home.join(".agents").join("skills")),
    )
}

fn discover_skills_from(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    user_agents_dir: Option<PathBuf>,
) -> io::Result<Vec<SkillPromptItem>> {
    let mut skills = Vec::new();
    let mut global_skills = Vec::new();
    read_skills_dir(&profile_dir.join("skills"), &mut global_skills)?;
    for skill in global_skills {
        if crate::skills::is_skill_path_enabled(profile_dir, &skill.path)? {
            skills.push(skill);
        }
    }
    // `~/.agents/skills/` is the cross-tool convention for user-level skills.
    if let Some(dir) = user_agents_dir {
        read_skills_dir(&dir, &mut skills)?;
    }
    if project_trusted {
        read_skills_dir(&cwd.join(".lynshen").join("skills"), &mut skills)?;
        // `.agents/skills/` is the cross-tool convention for project skills.
        read_skills_dir(&cwd.join(".agents").join("skills"), &mut skills)?;
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    Ok(skills)
}

pub fn skill_commands(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> io::Result<Vec<SkillCommand>> {
    skill_commands_from(
        profile_dir,
        cwd,
        project_trusted,
        crate::secrets::home_dir().map(|home| home.join(".agents").join("skills")),
    )
}

fn skill_commands_from(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    user_agents_dir: Option<PathBuf>,
) -> io::Result<Vec<SkillCommand>> {
    let mut commands = discover_skills_from(profile_dir, cwd, project_trusted, user_agents_dir)?
        .into_iter()
        .map(|skill| SkillCommand {
            command: format!("/{}", skill_command_name(&skill.name)),
            skill,
        })
        .collect::<Vec<_>>();
    commands.retain(|entry| entry.command != "/");
    commands.sort_by(|left, right| left.command.cmp(&right.command));
    Ok(commands)
}

/// Directories read-only file tools may read outside the workspace: the
/// directory each discovered skill lives in, so the model can read SKILL.md
/// and follow its relative references. Mutating tools stay confined.
pub fn skill_read_roots(skills: &[SkillPromptItem]) -> Vec<PathBuf> {
    let mut roots = skills
        .iter()
        .filter_map(|skill| skill.path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

pub fn skill_message(skill: &SkillPromptItem, request: &str) -> io::Result<String> {
    let content = fs::read_to_string(&skill.path)?;
    let mut message = format!(
        "Use the following skill instructions for this request.\n\n<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        content.trim_end()
    );
    if !request.trim().is_empty() {
        message.push_str("\n\nUser request:\n");
        message.push_str(request.trim());
    }
    Ok(message)
}

pub fn skill_pin_message(skill: &SkillPromptItem) -> io::Result<String> {
    let content = fs::read_to_string(&skill.path)?;
    Ok(format!(
        "<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        content.trim_end()
    ))
}

pub fn discover_project_instructions(cwd: &Path) -> io::Result<Vec<ProjectInstruction>> {
    let dirs = project_instruction_dirs(cwd);

    let mut instructions = Vec::new();
    let mut remaining = PROJECT_INSTRUCTIONS_MAX_BYTES;
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if path.exists() {
                if remaining == 0 {
                    return Ok(instructions);
                }
                let (content, bytes_read) = read_limited_utf8(&path, remaining)?;
                remaining = remaining.saturating_sub(bytes_read);
                instructions.push(ProjectInstruction {
                    path: path.clone(),
                    content,
                });
            }
        }
    }
    Ok(instructions)
}

fn project_instruction_dirs(cwd: &Path) -> Vec<&Path> {
    let mut dirs = cwd.ancestors().collect::<Vec<_>>();
    dirs.reverse();
    if let Some(index) = dirs.iter().position(|dir| dir.join(".git").exists()) {
        return dirs[index..].to_vec();
    }
    if let Some(index) = dirs
        .iter()
        .position(|dir| dir.join("AGENTS.md").exists() || dir.join("CLAUDE.md").exists())
    {
        return dirs[index..].to_vec();
    }
    vec![cwd]
}

fn read_limited_utf8(path: &Path, max_bytes: usize) -> io::Result<(String, usize)> {
    let bytes = fs::read(path)?;
    let truncated = bytes.len() > max_bytes;
    let mut end = bytes.len().min(max_bytes);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let mut content = String::from_utf8_lossy(&bytes[..end]).to_string();
    if truncated {
        content.push_str("\n\n[project instructions truncated by LynShen budget]\n");
    }
    Ok((content, end))
}

fn skill_command_name(name: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    output.trim_matches('-').to_string()
}

fn read_skills_dir(dir: &Path, skills: &mut Vec<SkillPromptItem>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if skill_path.exists() {
                if let Some(skill) = read_skill_file(&skill_path)? {
                    skills.push(skill);
                }
            } else {
                read_skills_dir(&path, skills)?;
            }
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            if let Some(skill) = read_skill_file(&path)? {
                skills.push(skill);
            }
        }
    }
    Ok(())
}

fn read_skill_file(path: &Path) -> io::Result<Option<SkillPromptItem>> {
    let content = fs::read_to_string(path)?;
    let name = read_frontmatter_field(&content, "name")
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "skill".to_string());
    let description = read_frontmatter_field(&content, "description")
        .or_else(|| first_non_empty_body_line(&content))
        .unwrap_or_default();
    if description.is_empty() {
        return Ok(None);
    }
    Ok(Some(SkillPromptItem {
        name,
        description,
        path: path.to_path_buf(),
    }))
}

pub(crate) fn read_frontmatter_field(content: &str, key: &str) -> Option<String> {
    let mut lines = content.lines().peekable();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    while let Some(line) = lines.next() {
        if line.trim_end() == "---" {
            return None;
        }
        // An indented line, or one without a colon, continues another field.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        if field.trim() != key {
            continue;
        }
        let value = value.trim();
        // A block scalar (`>`, `|`, `>-`, `|+`…): the indented lines below.
        if value.starts_with(['>', '|']) && value.len() <= 2 {
            let mut parts = Vec::new();
            while let Some(next) =
                lines.next_if(|next| next.trim().is_empty() || next.starts_with([' ', '\t']))
            {
                parts.push(next.trim());
            }
            let separator = if value.starts_with('>') { " " } else { "\n" };
            return Some(parts.join(separator).trim().to_string());
        }
        return Some(value.trim_matches('"').trim_matches('\'').to_string());
    }
    None
}

fn first_non_empty_body_line(content: &str) -> Option<String> {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .map(|line| line.trim_start_matches('#').trim().to_string())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn coding_context(edit_tools: Vec<String>) -> PromptContext {
        PromptContext {
            date: "2026-05-27".to_string(),
            cwd: PathBuf::from("/repo"),
            edit_tools,
            ..PromptContext::default()
        }
    }

    #[test]
    fn the_stable_prefix_is_the_same_across_days_and_directories() {
        let context = |cwd: &str, date: &str| PromptContext {
            date: date.to_string(),
            cwd: PathBuf::from(cwd),
            ..coding_context(vec!["hashline_edit".to_string()])
        };
        let a = build_system_prompt("base", &context("/a", "2026-10-09"));
        let b = build_system_prompt("base", &context("/b/worktree", "2026-10-10"));
        let (sa, sb) = (
            stable_prefix_len(&a).unwrap(),
            stable_prefix_len(&b).unwrap(),
        );
        assert_eq!(a[..sa], b[..sb]);
        assert!(a[sa..].starts_with(ENV_START) && a[sa..].contains("2026-10-09"));
    }

    #[test]
    fn chat_prompt_has_no_coding_tool_guidance() {
        let prompt = build_system_prompt(
            crate::chat::CHAT_SYSTEM_PROMPT,
            &PromptContext {
                chat: true,
                plan_mode: true,
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        assert!(!prompt.contains("Tool use:"));
        assert!(!prompt.contains("<plan_mode>"));
        assert!(prompt.contains("web_fetch"));
        assert!(prompt.ends_with("Date: 2026-05-27\n</env>"));
    }

    #[test]
    fn prompt_includes_env_project_context_and_skills() {
        let prompt = build_system_prompt(
            "Base prompt",
            &PromptContext {
                cwd: PathBuf::from("C:/repo"),
                project_instructions: vec![ProjectInstruction {
                    path: PathBuf::from("C:/repo/AGENTS.md"),
                    content: "Follow project rules.".to_string(),
                }],
                skills: vec![SkillPromptItem {
                    name: "review".to_string(),
                    description: "Review <code> & tests".to_string(),
                    path: PathBuf::from("C:/skills/review/SKILL.md"),
                }],
                ..coding_context(crate::config::default_edit_tools())
            },
        );

        assert!(prompt.contains("<env>\nWorking directory: C:/repo\nDate: 2026-05-27\n</env>"));
        assert!(prompt.contains("<project_context>"));
        assert!(prompt.contains("Follow project rules."));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains(
            "<skill name=\"review\" location=\"C:/skills/review/SKILL.md\">Review &lt;code&gt; &amp; tests</skill>"
        ));
    }

    #[test]
    fn stable_parts_come_before_the_date_mode_and_host_text() {
        let prompt = build_system_prompt(
            "Base prompt",
            &PromptContext {
                project_instructions: vec![ProjectInstruction {
                    path: PathBuf::from("/repo/AGENTS.md"),
                    content: "Follow project rules.".to_string(),
                }],
                skills: vec![SkillPromptItem {
                    name: "review".to_string(),
                    description: "Review code".to_string(),
                    path: PathBuf::from("/skills/review/SKILL.md"),
                }],
                sandbox: "<sandbox mode=\"workspace-write\">\nnote\n</sandbox>".to_string(),
                plan_mode: true,
                host: "Peers: ops (busy)".to_string(),
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        let position = |needle: &str| {
            prompt
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        let order = [
            position("Base prompt"),
            position("Tool use:"),
            position("<project_context>"),
            position("<available_skills>"),
            position("<sandbox"),
            position("<env>"),
            position("Date: 2026-05-27"),
            position("<plan_mode>"),
            position("Peers: ops (busy)"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        // Only the date line differs between two days' prompts, near the end.
        let tomorrow = build_system_prompt(
            "Base prompt",
            &PromptContext {
                date: "2026-05-28".to_string(),
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        let today = build_system_prompt(
            "Base prompt",
            &coding_context(crate::config::default_edit_tools()),
        );
        let shared = today
            .bytes()
            .zip(tomorrow.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert_eq!(&today[shared..], "7\n</env>");
    }

    #[test]
    fn default_guidance_names_only_hashline_edit() {
        let prompt =
            build_system_prompt("Base", &coding_context(crate::config::default_edit_tools()));
        assert!(prompt.contains("Edit files with hashline_edit."));
        assert!(prompt.contains("Create new files with bash"));
        for disabled in ["str_replace", "apply_patch", "with write"] {
            assert!(!prompt.contains(disabled), "guidance names {disabled}");
        }
    }

    #[test]
    fn guidance_lists_enabled_edit_tools_and_new_file_rule() {
        let all = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        let prompt = build_system_prompt(
            "Base",
            &coding_context(all(&["hashline_edit", "write", "apply_patch"])),
        );
        assert!(prompt.contains("Edit files with hashline_edit or write or apply_patch."));
        assert!(prompt.contains("Create new files with write."));
        let patch_only = build_system_prompt("Base", &coding_context(all(&["apply_patch"])));
        assert!(patch_only.contains("Create new files with apply_patch."));
        let none = build_system_prompt("Base", &coding_context(Vec::new()));
        assert!(none.contains("No file-edit tools are enabled"));
        assert!(!none.contains("Edit files with"));
    }

    #[test]
    fn discovers_project_instructions_from_root_to_cwd() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-instruction-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("repo");
        let nested = project.join("crates").join("cli");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("AGENTS.md"), "root agents").unwrap();
        fs::write(nested.join("CLAUDE.md"), "nested claude").unwrap();

        let instructions = discover_project_instructions(&nested).unwrap();

        assert_eq!(instructions.len(), 2);
        assert_eq!(
            instructions[0]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("AGENTS.md")
        );
        assert_eq!(
            instructions[1]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("CLAUDE.md")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn frontmatter_reads_block_scalars_and_skips_continuations() {
        let text = "---\nname: finetone\ndescription: >-\n  Tune the voice\n  of prose.\nlicense: MIT\n---\nbody";
        assert_eq!(
            read_frontmatter_field(text, "description").unwrap(),
            "Tune the voice of prose."
        );
        assert_eq!(read_frontmatter_field(text, "license").unwrap(), "MIT");
        let quoted = "---\nother: |\n  a: b\nname: 'Code Review'\n---\n";
        assert_eq!(
            read_frontmatter_field(quoted, "name").unwrap(),
            "Code Review"
        );
    }

    #[test]
    fn discovers_frontmatter_skill_files() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review code carefully\n---\nbody\n",
        )
        .unwrap();

        let skills =
            discover_skills_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].description, "Review code carefully");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_nested_skill_files() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-nested-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root
            .join("profile")
            .join("skills")
            .join("bundle")
            .join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: nested-review\ndescription: Review nested code\n---\nbody\n",
        )
        .unwrap();

        let skills =
            discover_skills_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "nested-review");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_commands_slugify_names() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-command-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("code-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Code Review\ndescription: Review code\n---\nbody\n",
        )
        .unwrap();

        let commands =
            skill_commands_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "/code-review");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_global_skills_are_hidden_but_trusted_project_skills_load() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-sources-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("profile");
        let cwd = root.join("repo");
        let global = profile.join("skills/global");
        let user_agents = root.join("home/.agents/skills");
        let user_agents_skill = user_agents.join("home-skill");
        let project = cwd.join(".lynshen/skills/project");
        let agents = cwd.join(".agents/skills/agents-skill");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&user_agents_skill).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            global.join("SKILL.md"),
            "---\nname: global\ndescription: Global\n---\n",
        )
        .unwrap();
        fs::write(
            user_agents_skill.join("SKILL.md"),
            "---\nname: home-agents\ndescription: Home agents dir\n---\n",
        )
        .unwrap();
        fs::write(
            project.join("SKILL.md"),
            "---\nname: project\ndescription: Project\n---\n",
        )
        .unwrap();
        fs::write(
            agents.join("SKILL.md"),
            "---\nname: agents\ndescription: Agents dir\n---\n",
        )
        .unwrap();
        crate::skills::set_skill_enabled(&profile, "global", false).unwrap();

        let trusted =
            discover_skills_from(&profile, &cwd, true, Some(user_agents.clone())).unwrap();
        let untrusted = discover_skills_from(&profile, &cwd, false, Some(user_agents)).unwrap();

        assert_eq!(
            trusted
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["agents", "home-agents", "project"]
        );
        // Project trust only gates project dirs; user-level sources always load.
        assert_eq!(
            untrusted
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["home-agents"]
        );
        let _ = fs::remove_dir_all(root);
    }
}
