//! Token budget of a default session's first request: the system prompt and
//! the tool definitions as sent, counted with the crate's own tokenizer.
//!
//! `cargo test -p agent-core prompt_budget -- --ignored --nocapture` prints
//! the table per part and per tool.

use crate::{
    config::{default_edit_tools, models_for_provider, LiveApprovalMode, DEFAULT_SYSTEM_PROMPT},
    hooks::Hooks,
    llm::{OpenAiClient, OpenAiClientConfig},
    mcp::McpManager,
    prompt::{build_system_prompt, PromptContext},
    sandbox::{SandboxMode, SandboxPolicy},
    subagents::SubagentManager,
    tokens::count_text,
    tools::ToolState,
};
use llm_provider_kit::{anthropic, chat, responses};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::mpsc, time::Duration};

const MODEL: &str = "gpt-5.5";

fn tokens(text: &str) -> usize {
    count_text(MODEL, text).tokens
}

/// The sandbox a new config gets on macOS and Linux.
fn default_sandbox() -> SandboxPolicy {
    SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        ..SandboxPolicy::default_for_platform()
    }
}

fn cwd() -> PathBuf {
    PathBuf::from("/home/user/project")
}

/// A default coding session's tool state: sandboxed shell, signed in to
/// LynShen (web_search offered) and an image model configured.
fn default_tool_state() -> ToolState {
    let state = ToolState::default();
    state.set_sandbox(Some(default_sandbox()));
    state.set_web(Some(crate::web::WebTools {
        api_url: "https://api.lynshen.org".to_string(),
        encrypt_secrets: false,
        search_engine: crate::web::DEFAULT_SEARCH_ENGINE.to_string(),
        fetch_engine: crate::web::DEFAULT_FETCH_ENGINE.to_string(),
        signed_in: true,
    }));
    state.set_images(Ok(crate::images::ImageTools::for_test()));
    state
}

fn system_prompt(chat: bool) -> String {
    let base = if chat {
        crate::chat::CHAT_SYSTEM_PROMPT
    } else {
        DEFAULT_SYSTEM_PROMPT
    };
    build_system_prompt(
        base,
        &PromptContext {
            date: "2026-10-09".to_string(),
            cwd: cwd(),
            edit_tools: default_edit_tools(),
            chat,
            sandbox: default_sandbox().prompt(),
            ..PromptContext::default()
        },
    )
}

fn default_client(system_prompt: String) -> OpenAiClient {
    let (goal_tx, _goal_rx) = mpsc::channel();
    OpenAiClient::from_config(OpenAiClientConfig {
        model: MODEL.to_string(),
        provider: "lynshen".to_string(),
        protocol: "responses".to_string(),
        reasoning_effort: "medium".to_string(),
        models: models_for_provider("lynshen"),
        subagent_models: Vec::new(),
        system_prompt,
        prompt_cache_key: "session".to_string(),
        mcp: McpManager::default(),
        base_url: "https://api.lynshen.org/v1".to_string(),
        max_output_tokens: 32_000,
        api_key: Some("test-key"),
        api_key_env: "LYNSHEN_TEST_API_KEY",
        retry_attempts: 1,
        connect_timeout: Duration::from_secs(1),
        read_timeout: Duration::from_secs(1),
        goal_tool_tx: Some(goal_tx),
        has_goal: false,
        approval_tx: None,
        approval_mode: LiveApprovalMode::default(),
        safety_model: None,
        safety_reasoning_effort: String::new(),
        model_headers: HashMap::new(),
        edit_tools: default_edit_tools(),
        extra_read_roots: Vec::new(),
        tool_state: default_tool_state(),
        host: None,
        subagent_manager: Some(SubagentManager::default()),
        roles: crate::roles::builtin(),
        hooks: Hooks::default(),
    })
    .expect("default client")
}

fn group(name: &str) -> &'static str {
    match name {
        "bash" | "exec_command" | "write_stdin" => "shell",
        "web_fetch" | "web_search" => "web",
        "generate_image" => "image",
        "spawn_agent" | "wait_agent" | "list_agents" | "send_message" | "close_agent"
        | "merge_agent" => "subagents",
        "get_goal" | "create_goal" | "update_goal" => "goals",
        "update_plan" | "propose_plan" => "plan",
        _ => "files",
    }
}

const GROUPS: [&str; 7] = [
    "files",
    "shell",
    "web",
    "image",
    "subagents",
    "goals",
    "plan",
];

struct Budget {
    lines: Vec<(String, usize)>,
    system: usize,
    tools_responses: usize,
    tools_chat: usize,
    tools_anthropic: usize,
}

fn measure(chat_session: bool) -> Budget {
    let system = system_prompt(chat_session);
    let client = default_client(system.clone());
    let definitions = client.tool_definitions();
    let mut lines = Vec::new();
    let base = if chat_session {
        crate::chat::CHAT_SYSTEM_PROMPT
    } else {
        DEFAULT_SYSTEM_PROMPT
    };
    lines.push(("system: base prompt".to_string(), tokens(base)));
    lines.push((
        "system: runtime context, guidance, sandbox".to_string(),
        tokens(&system) - tokens(base),
    ));
    for name in GROUPS {
        let in_group = definitions
            .iter()
            .filter(|tool| group(tool["name"].as_str().unwrap_or_default()) == name)
            .collect::<Vec<_>>();
        let total = in_group
            .iter()
            .map(|tool| tokens(&tool.to_string()))
            .sum::<usize>();
        lines.push((format!("tools: {name}"), total));
        for tool in in_group {
            lines.push((
                format!("  {}", tool["name"].as_str().unwrap_or_default()),
                tokens(&tool.to_string()),
            ));
        }
    }
    let array = |tools: Vec<Value>| tokens(&Value::Array(tools).to_string());
    Budget {
        lines,
        system: tokens(&system),
        tools_responses: array(definitions.clone()),
        tools_chat: array(chat::tools_from_definitions(&definitions)),
        tools_anthropic: array(anthropic::tool_definitions(&definitions)),
    }
}

/// The whole first request body without the user's message, per protocol.
fn request_bodies(system: &str, definitions: &[Value]) -> [(&'static str, usize); 3] {
    let responses_body = responses::request_body(responses::ResponsesRequest {
        model: MODEL,
        instructions: system,
        prompt_cache_key: "session",
        reasoning_effort: "medium",
        input: Vec::new(),
        tools: definitions,
        max_output_tokens: 32_000,
    });
    let chat_body = chat::request_body(&chat::ChatRequest {
        model: MODEL,
        system_prompt: system,
        input: &[],
        tools: definitions,
        max_output_tokens: 32_000,
        reasoning_effort: "medium",
    });
    let anthropic_tools = anthropic::tool_definitions(definitions);
    let anthropic_body = anthropic::request_body(&anthropic::AnthropicRequest {
        model: MODEL,
        system_prompt: system,
        input: &[],
        tools: &anthropic_tools,
        max_output_tokens: 32_000,
        reasoning_effort: "medium",
    });
    [
        ("responses", tokens(&responses_body.to_string())),
        ("chat", tokens(&chat_body.to_string())),
        ("anthropic", tokens(&anthropic_body.to_string())),
    ]
}

#[test]
#[ignore = "prints the prompt token table"]
fn print_prompt_token_report() {
    for (label, chat_session) in [("coding session", false), ("chat session", true)] {
        let budget = measure(chat_session);
        println!("\n== {label} ({MODEL} tokenizer) ==");
        for (name, count) in &budget.lines {
            println!("{name:<45} {count:>6}");
        }
        println!("{:<45} {:>6}", "system prompt total", budget.system);
        println!(
            "{:<45} {:>6}",
            "tool array (responses)", budget.tools_responses
        );
        println!("{:<45} {:>6}", "tool array (chat)", budget.tools_chat);
        println!(
            "{:<45} {:>6}",
            "tool array (anthropic)", budget.tools_anthropic
        );
        println!(
            "{:<45} {:>6}",
            "system + tools (responses)",
            budget.system + budget.tools_responses
        );
        let system = system_prompt(chat_session);
        let definitions = default_client(system.clone()).tool_definitions();
        for (protocol, count) in request_bodies(&system, &definitions) {
            println!("{:<45} {count:>6}", format!("request body ({protocol})"));
        }
    }
}

/// The coding session's first request before this budget was kept: 874
/// tokens of system prompt and 3647 of tool definitions (0.4.19).
const BASELINE_CODING_TOKENS: usize = 4521;

/// The first request may carry at most this share of the baseline, in
/// percent. 60 when the budget was set; 61 since the agent team tools
/// (roles, merge_agent, plan step owners) added about 160 tokens.
const MAX_PERCENT_OF_BASELINE: usize = 61;

#[test]
fn default_coding_request_stays_lean() {
    let budget = measure(false);
    let total = budget.system + budget.tools_responses;
    assert!(
        total * 100 <= BASELINE_CODING_TOKENS * MAX_PERCENT_OF_BASELINE,
        "system prompt + tools grew to {total} tokens; run print_prompt_token_report"
    );
}
