//! The skills marketplace for the desktop: github.com/anthropics/skills and
//! community skill repositories in one catalog, installed into the personal
//! skills directory of the session's engine.

use crate::engines;
use lynshen_agent_core::skills;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn handle(name: &str, op: &Value) -> Result<Value, String> {
    let dir = install_dir(op["backend"].as_str().unwrap_or("lynshen"))?;
    match name {
        "skills_catalog" => catalog(dir),
        "skill_install" => {
            let text = |key: &str| {
                op[key]
                    .as_str()
                    .ok_or_else(|| format!("skill_install requires {key}"))
            };
            install(dir, text("source")?, text("skill")?)
                .map(|path| json!({ "type": "skill_installed", "path": path }))
        }
        _ => Err(format!("unknown op {name}")),
    }
}

/// Claude Code reads `~/.claude/skills`; every other engine runs with the
/// LynShen profile's skills.
fn install_dir(backend: &str) -> Result<PathBuf, String> {
    if backend != "claude" {
        return skills::profile_skills_dir().map_err(|error| error.to_string());
    }
    let home = engines::home();
    if home.as_os_str().is_empty() {
        return Err("home directory not found".to_string());
    }
    Ok(home.join(".claude").join("skills"))
}

/// The community repositories, fetched at most once an hour: listing one
/// reads every SKILL.md it has.
fn community_sources() -> Vec<(String, Result<skills::SkillSource, String>)> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    type Cached = (Instant, Result<skills::SkillSource, String>);
    static CACHE: Mutex<Vec<(String, Cached)>> = Mutex::new(Vec::new());
    const FRESH: Duration = Duration::from_secs(3600);
    skills::COMMUNITY_SKILL_SOURCES
        .iter()
        .map(|(name, repository)| {
            let cached = CACHE.lock().ok().and_then(|cache| {
                cache
                    .iter()
                    .find(|(repo, (at, result))| {
                        repo == repository && (result.is_ok() && at.elapsed() < FRESH)
                    })
                    .map(|(_, (_, result))| result.clone())
            });
            let result = cached.unwrap_or_else(|| {
                let result = skills::fetch_github_source(name, repository);
                if let Ok(mut cache) = CACHE.lock() {
                    cache.retain(|(repo, _)| repo != repository);
                    cache.push((repository.to_string(), (Instant::now(), result.clone())));
                }
                result
            });
            (name.to_string(), result)
        })
        .collect()
}

/// A catalog entry's `source`: `anthropic`, or a community repository's
/// `owner/repo`.
fn source_key(repository: &str) -> String {
    repository
        .trim_start_matches("https://github.com/")
        .to_string()
}

/// Anthropic's catalog is bundled; a community repository that cannot be read
/// is only a warning.
fn catalog(dir: PathBuf) -> Result<Value, String> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut add = |source: &skills::SkillSource, key: &str| {
        entries.extend(source.skills.iter().map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "tags": skill.tags,
                "source": key,
                "sourceName": source.name,
                "isDefault": false,
                "installed": skills::skill_installed(&dir, &skill.id),
                "license": skill.license,
                "redistributable": skill.redistributable,
                "homepage": source.homepage(&skill.id),
            })
        }))
    };
    add(&skills::anthropic_source()?, "anthropic");
    for (name, result) in community_sources() {
        match result {
            Ok(source) => add(&source, &source_key(&source.repository)),
            Err(error) => warnings.push(format!("{name}: {error}")),
        }
    }
    let key = |entry: &Value| {
        (
            entry["source"] != "anthropic",
            entry["sourceName"].as_str().unwrap_or_default().to_string(),
            entry["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    };
    entries.sort_by_key(key);
    Ok(json!({
        "type": "skills_catalog",
        "skills": entries,
        "warnings": warnings,
        "installDir": dir,
    }))
}

/// The skill is looked up again here rather than trusting anything the
/// client sent beyond its source and id. Anthropic's source-available skills
/// are listed but not installed, as in the TUI's `/skills`.
fn install(dir: PathBuf, source: &str, id: &str) -> Result<PathBuf, String> {
    let found = if source == "anthropic" {
        skills::anthropic_source()?
    } else {
        community_sources()
            .into_iter()
            .find_map(|(_, result)| result.ok().filter(|s| source_key(&s.repository) == source))
            .ok_or_else(|| format!("unknown skill source: {source}"))?
    };
    let skill = found
        .skills
        .iter()
        .find(|skill| skill.id == id)
        .ok_or_else(|| format!("skill not found in {}: {id}", found.name))?;
    if !skill.redistributable {
        return Err(format!(
            "skill {id} is not offered: {}; not redistributed by LynShen",
            skill.license
        ));
    }
    skills::install_source_skill(&dir, &found, skill).map_err(|error| error.to_string())
}
