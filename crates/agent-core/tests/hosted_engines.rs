//! Engines hosted inside one process, the shape the daemon uses: each
//! `AgentCore` is opened on an explicit directory, and an unattended engine
//! defers gated calls instead of blocking on a prompt.
//!
//! The whole binary runs against a temporary HOME and a local fake
//! chat-completions server, so it never reads or writes the developer's
//! `~/.lynshen` or calls a real provider.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use lynshen_agent_core::{AgentCore, AgentEvent, ApprovalMode};
use std::{
    env, fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Polls the engine until `done` matches an event (or times out), returning
/// every event seen.
/// Polls until an event satisfies `done`, keeping the whole batch it came in:
/// a poll appends the subagents' lifecycle events after the turn's `ready`,
/// and a fast subagent's land in that same batch.
fn pump(core: &mut AgentCore, done: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let batch = core.poll_events();
        let finished = batch.iter().any(&done);
        seen.extend(batch);
        if finished {
            return seen;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; events: {seen:#?}");
}

fn is_ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Status(status) if status == "ready")
}

fn assistant_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    core
}

fn write_command(text: &str, file: &str) -> String {
    if cfg!(windows) {
        format!(
            "RUN: [System.IO.File]::WriteAllText((Join-Path (Get-Location) '{file}'), '{text}')"
        )
    } else {
        format!("RUN: printf {text} > {file}")
    }
}

#[test]
fn engines_in_one_process_work_in_their_own_directories() {
    let _guard = setup();
    let first_dir = temp_dir("first");
    let second_dir = temp_dir("second");
    let mut first = open(&first_dir, ApprovalMode::FullAccess);
    let mut second = open(&second_dir, ApprovalMode::FullAccess);

    first.submit_user_message(write_command("first", "out.txt"));
    second.submit_user_message(write_command("second", "out.txt"));
    pump(&mut first, is_ready);
    pump(&mut second, is_ready);

    assert_eq!(
        fs::read_to_string(first_dir.join("out.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(second_dir.join("out.txt")).unwrap(),
        "second"
    );
    assert_ne!(env::current_dir().unwrap(), first_dir);
}

#[test]
fn unattended_engine_defers_a_gated_call_and_runs_it_once_approved() {
    let _guard = setup();
    let dir = temp_dir("deferred");
    let mut core = open(&dir, ApprovalMode::Manual);
    core.set_attended(false);

    core.submit_user_message(write_command("ran", "marker.txt"));
    let events = pump(&mut core, is_ready);
    let action = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.clone()),
            _ => None,
        })
        .expect("gated call is deferred, not prompted");
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    // The turn finished without waiting, and the model was told the call is
    // pending confirmation rather than denied.
    assert!(assistant_text(&events).contains("submitted for confirmation"));
    assert!(!dir.join("marker.txt").exists());
    assert_eq!(action.name, "bash");
    assert_eq!(action.cwd, dir);

    core.decide_action(&action.id, true);
    let events = pump(&mut core, is_ready);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ActionDecided { id, allow: true, is_error: false, .. } if *id == action.id
    )));
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "ran");
    // The outcome woke the session with a message the model answered.
    assert!(assistant_text(&events).contains(&format!("deferred action {} approved", action.id)));
}

#[test]
fn a_declined_deferred_action_is_reused_for_the_same_call() {
    let _guard = setup();
    let dir = temp_dir("declined");
    let mut core = open(&dir, ApprovalMode::Manual);
    core.set_attended(false);

    core.submit_user_message("RUN: printf no > marker.txt".to_string());
    let events = pump(&mut core, is_ready);
    let id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.id.clone()),
            _ => None,
        })
        .unwrap();
    core.decide_action(&id, false);
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("declined"));

    // The identical call is denied from the recorded decision: no new
    // deferred action, and nothing ran.
    core.submit_user_message("RUN: printf no > marker.txt".to_string());
    let events = pump(&mut core, is_ready);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ActionDeferred(_))));
    assert!(assistant_text(&events).contains("denied by user"));
    assert!(!dir.join("marker.txt").exists());
}

#[test]
fn going_unattended_releases_a_call_waiting_on_a_prompt() {
    let _guard = setup();
    let dir = temp_dir("release");
    let mut core = open(&dir, ApprovalMode::Manual);

    core.submit_user_message("RUN: printf later > marker.txt".to_string());
    pump(&mut core, |event| {
        matches!(event, AgentEvent::ApprovalRequest { .. })
    });
    let events = core.set_attended(false);
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ActionDeferred(_))));
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("submitted for confirmation"));
    assert!(!dir.join("marker.txt").exists());
}

#[test]
fn config_changes_from_two_engines_both_land() {
    let _guard = setup();
    let mut first = open(&temp_dir("config-first"), ApprovalMode::Manual);
    let mut second = open(&temp_dir("config-second"), ApprovalMode::Manual);
    let server = |name: &str| {
        serde_json::json!({
            "name": name, "transport": "stdio", "command": "true", "enabled": false,
        })
    };
    // `second` loaded the config before `first` changed it; its save must
    // not drop `first`'s server.
    first.mcp_set(&server("from_first"));
    second.mcp_set(&server("from_second"));

    let config_path = std::path::PathBuf::from(env::var("HOME").unwrap())
        .join(".lynshen")
        .join("config.json");
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let names: Vec<&str> = saved["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(names.contains(&"from_first"), "{names:?}");
    assert!(names.contains(&"from_second"), "{names:?}");

    second.mcp_remove("from_first");
    second.mcp_remove("from_second");
}

#[test]
fn host_tools_run_in_the_host_and_host_prompt_reaches_the_model() {
    use lynshen_agent_core::host::{HostExtensions, HostGate};
    use std::sync::{Arc, Mutex};
    let _guard = setup();
    let mut core = open(&temp_dir("host"), ApprovalMode::Manual);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    core.set_host_extensions(HostExtensions {
        tools: vec![serde_json::json!({
            "type": "function",
            "name": "note_down",
            "description": "Record a note.",
            "parameters": { "type": "object", "properties": { "text": { "type": "string" } } }
        })],
        run_tool: Arc::new(move |name, arguments, _| {
            seen.lock().unwrap().push(format!("{name} {arguments}"));
            ("noted".to_string(), false)
        }),
        prompt: Arc::new(|| "<host-marker>brief goes here</host-marker>".to_string()),
        exclusive: false,
        gate: Arc::new(|_| HostGate::Run),
        summary: Arc::new(|_, _| String::new()),
    });

    core.submit_user_message(r#"CALL note_down {"text":"hi"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![r#"note_down {"text":"hi"}"#.to_string()]
    );
    // No approval is asked for a host tool, and its output reaches the model.
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    assert!(assistant_text(&events).contains("noted"));

    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("<host-marker>brief goes here</host-marker>"));
}

/// A host with `peek` (read-only), `tell` (outward), `always` (asks under
/// every mode) and `wait` (runs until its turn stops); each call is logged.
fn gated_host(
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> lynshen_agent_core::host::HostExtensions {
    use lynshen_agent_core::host::{HostExtensions, HostGate};
    use std::sync::{atomic::Ordering, Arc};
    let tool = |name: &str| {
        serde_json::json!({
            "type": "function",
            "name": name,
            "description": "Test tool.",
            "parameters": { "type": "object", "properties": { "text": { "type": "string" } } }
        })
    };
    HostExtensions {
        tools: ["peek", "tell", "always", "wait"].map(tool).to_vec(),
        run_tool: Arc::new(move |name, _, stopped| {
            if name == "wait" {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            calls.lock().unwrap().push(name.to_string());
            (format!("{name} done"), false)
        }),
        prompt: Arc::new(String::new),
        exclusive: false,
        gate: Arc::new(|name| match name {
            "peek" => HostGate::ReadOnly,
            "tell" => HostGate::Outward,
            "always" => HostGate::Ask,
            _ => HostGate::Run,
        }),
        summary: Arc::new(|name, arguments| format!("host card {name} {arguments}")),
    }
}

/// Runs `CALL <tool>` and allows an approval request (if any), for the
/// session too when `always`; returns the request's summary, None when
/// nothing was asked.
fn call_host_tool(core: &mut AgentCore, tool: &str, always: bool) -> Option<String> {
    core.submit_user_message(format!(r#"CALL {tool} {{"text":"hi"}}"#));
    let events = pump(core, |event| {
        is_ready(event) || matches!(event, AgentEvent::ApprovalRequest { .. })
    });
    let asked = events.iter().find_map(|event| match event {
        AgentEvent::ApprovalRequest {
            call_id, summary, ..
        } => Some((call_id.clone(), summary.clone())),
        _ => None,
    });
    if let Some((call_id, _)) = &asked {
        core.approve(call_id, true, always, None);
        pump(core, is_ready);
    }
    asked.map(|(_, summary)| summary)
}

#[test]
fn host_tools_are_gated_as_the_host_says() {
    use std::sync::{Arc, Mutex};
    let _guard = setup();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut core = open(&temp_dir("host-gates"), ApprovalMode::Plan);
    core.set_host_extensions(gated_host(Arc::clone(&calls)));

    // Plan mode: a read-only host tool runs; an outward one asks first.
    assert_eq!(call_host_tool(&mut core, "peek", false), None);
    assert_eq!(
        call_host_tool(&mut core, "tell", false).as_deref(),
        Some(r#"host card tell {"text":"hi"}"#)
    );
    core.set_approval_mode(ApprovalMode::Manual);
    assert!(call_host_tool(&mut core, "tell", false).is_some());
    core.set_approval_mode(ApprovalMode::Auto);
    assert_eq!(call_host_tool(&mut core, "tell", false), None);
    // `always` asks under full access too, and approving it "always" does
    // not allowlist it.
    core.set_approval_mode(ApprovalMode::FullAccess);
    assert!(call_host_tool(&mut core, "always", true).is_some());
    assert!(call_host_tool(&mut core, "always", true).is_some());
    assert_eq!(
        *calls.lock().unwrap(),
        ["peek", "tell", "tell", "tell", "always", "always"]
    );
}

#[test]
fn a_deferred_host_call_runs_in_the_host_once_approved() {
    use std::sync::{Arc, Mutex};
    let _guard = setup();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut core = open(&temp_dir("host-deferred"), ApprovalMode::Manual);
    core.set_host_extensions(gated_host(Arc::clone(&calls)));
    core.set_attended(false);

    core.submit_user_message(r#"CALL tell {"text":"later"}"#.to_string());
    let events = pump(&mut core, is_ready);
    let action = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.clone()),
            _ => None,
        })
        .expect("an unattended outward call is deferred");
    assert_eq!(action.summary, r#"host card tell {"text":"later"}"#);
    assert!(calls.lock().unwrap().is_empty());

    core.decide_action(&action.id, true);
    let events = pump(&mut core, is_ready);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ActionDecided { output: Some(output), is_error: false, .. } if output.contains("tell done")
    )));
    assert_eq!(*calls.lock().unwrap(), ["tell"]);
}

#[test]
fn a_waiting_host_tool_sees_its_turn_stopped() {
    use std::sync::{Arc, Mutex};
    let _guard = setup();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut core = open(&temp_dir("host-stop"), ApprovalMode::FullAccess);
    core.set_host_extensions(gated_host(Arc::clone(&calls)));

    let started = Instant::now();
    core.submit_user_message("CALL wait {}".to_string());
    pump(
        &mut core,
        |event| matches!(event, AgentEvent::ToolStart { name, .. } if name == "wait"),
    );
    core.interrupt();
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.lock().unwrap().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*calls.lock().unwrap(), ["wait"]);
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn an_exclusive_host_leaves_the_engine_no_tools_of_its_own() {
    use lynshen_agent_core::host::{HostExtensions, HostGate};
    use std::sync::Arc;
    let _guard = setup();
    let dir = temp_dir("exclusive");
    let mut core = open(&dir, ApprovalMode::FullAccess);
    core.set_host_extensions(HostExtensions {
        tools: vec![serde_json::json!({
            "type": "function",
            "name": "note_down",
            "description": "Record a note.",
            "parameters": { "type": "object", "properties": {} }
        })],
        run_tool: Arc::new(|_, _, _| ("noted".to_string(), false)),
        prompt: Arc::new(String::new),
        exclusive: true,
        gate: Arc::new(|_| HostGate::Run),
        summary: Arc::new(|_, _| String::new()),
    });
    core.submit_user_message(r#"CALL bash {"command":"touch made-by-bash"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert!(!dir.join("made-by-bash").exists());
    assert!(
        assistant_text(&events).contains("unknown tool"),
        "{}",
        assistant_text(&events)
    );
}

#[test]
fn another_agents_tool_name_runs_the_tool_it_means() {
    let _guard = setup();
    let dir = temp_dir("tool-alias");
    let mut core = open(&dir, ApprovalMode::FullAccess);
    // Claude Code's Bash: the name and its milliseconds timeout.
    core.submit_user_message(
        r#"CALL Bash {"command":"touch made-by-Bash","description":"x","timeout":120000}"#
            .to_string(),
    );
    let events = pump(&mut core, is_ready);
    assert!(
        dir.join("made-by-Bash").exists(),
        "{}",
        assistant_text(&events)
    );
    // A name that means nothing offered: the error lists the tools.
    core.submit_user_message(r#"CALL Glob {"pattern":"*"}"#.to_string());
    let events = pump(&mut core, is_ready);
    let text = assistant_text(&events);
    assert!(
        text.contains("unknown tool `Glob`") && text.contains("bash"),
        "{text}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_sandboxed_engine_runs_commands_inside_and_asks_to_leave() {
    use lynshen_agent_core::sandbox::{CommandRule, RuleAction, SandboxMode, SandboxPolicy};
    let _guard = setup();
    let dir = temp_dir("sandboxed");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // auto-edit asks for every shell command without a sandbox.
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    core.set_sandbox(Some(SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        writable_dirs: Vec::new(),
        readable_dirs: Vec::new(),
        network: true,
        rules: vec![
            CommandRule {
                prefix: "touch".into(),
                action: RuleAction::Allow,
            },
            CommandRule {
                prefix: "rm -rf".into(),
                action: RuleAction::Forbid,
            },
        ],
    }));
    let approvals = |events: &[AgentEvent]| {
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ApprovalRequest { .. }))
            .count()
    };

    // Inside the sandbox: no approval, workspace writable, .git not.
    core.submit_user_message("RUN: printf ok > inside.txt; printf no > .git/config".to_string());
    let events = pump(&mut core, is_ready);
    assert_eq!(approvals(&events), 0);
    assert_eq!(fs::read_to_string(dir.join("inside.txt")).unwrap(), "ok");
    assert!(!dir.join(".git/config").exists());

    // Leaving the sandbox asks; once allowed it runs outside.
    core.submit_user_message(
        r#"CALL bash {"command":"printf yes > .git/config","escalate":true,"justification":"test"}"#.to_string(),
    );
    let events = pump(&mut core, |event| {
        matches!(event, AgentEvent::ApprovalRequest { .. })
    });
    let call_id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ApprovalRequest { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .unwrap();
    core.approve(&call_id, true, false, None);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join(".git/config")).unwrap(), "yes");

    // An allow rule lets that escalation through without asking.
    core.submit_user_message(
        r#"CALL bash {"command":"touch .git/marker","escalate":true}"#.to_string(),
    );
    let events = pump(&mut core, is_ready);
    assert_eq!(approvals(&events), 0);
    assert!(dir.join(".git/marker").exists());

    // A forbid rule never runs.
    fs::write(dir.join("keep.txt"), "keep").unwrap();
    core.submit_user_message(r#"CALL bash {"command":"rm -rf keep.txt"}"#.to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("forbidden"));
    assert!(dir.join("keep.txt").exists());

    // The model is told about the sandbox and gets the escalate parameter.
    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("<sandbox mode=\"workspace-write\">"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_plain_engine_is_sandboxed_by_default_and_sandbox_switches_it() {
    let _guard = setup();
    let dir = temp_dir("plain-sandbox");
    fs::create_dir_all(dir.join(".git")).unwrap();
    // No agent, no set_sandbox: the config default applies.
    let mut core = open(&dir, ApprovalMode::FullAccess);
    core.submit_user_message("RUN: printf a > file.txt; printf b > .git/config".to_string());
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("file.txt")).unwrap(), "a");
    assert!(!dir.join(".git/config").exists());

    let (_, events) = core.handle_command("/sandbox");
    let shown = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Info(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(shown.starts_with("sandbox: workspace-write"), "{shown}");
    assert!(shown.contains("git push → ask"), "{shown}");

    core.handle_command("/sandbox full-access");
    core.submit_user_message("RUN: printf b > .git/config".to_string());
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join(".git/config")).unwrap(), "b");
}

#[test]
fn subagents_command_saves_the_models_subagents_may_use() {
    let _guard = setup();
    let mut core = open(&temp_dir("subagents"), ApprovalMode::Manual);
    let config_path = std::path::PathBuf::from(env::var("HOME").unwrap())
        .join(".lynshen")
        .join("config.json");
    let saved = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap()
    };

    let (_, events) = core.handle_command("/subagents add no-such-model search");
    assert!(
        matches!(&events[..], [AgentEvent::Error(error)] if error.contains("unknown model")),
        "{events:?}"
    );

    let (_, events) = core.handle_command("/subagents add fake-model wide code search");
    assert!(matches!(&events[..], [AgentEvent::Status(_)]), "{events:?}");
    assert_eq!(
        saved()["subagent_models"],
        serde_json::json!([{ "name": "fake-model", "description": "wide code search" }])
    );

    let (_, events) = core.handle_command("/subagents");
    assert!(
        matches!(&events[..], [AgentEvent::Info(info)] if info.contains("fake-model — wide code search")),
        "{events:?}"
    );

    core.handle_command("/subagents remove fake-model");
    assert_eq!(saved()["subagent_models"], serde_json::json!([]));
}

fn plan_event(events: &[AgentEvent]) -> Option<(String, String, String)> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::ProposedPlan {
            id, title, status, ..
        } => Some((id.clone(), title.clone(), status.clone())),
        _ => None,
    })
}

#[test]
fn plan_mode_refuses_changes_and_tells_the_model_to_plan() {
    let _guard = setup();
    let dir = temp_dir("plan-refuse");
    let mut core = open(&dir, ApprovalMode::Plan);

    core.submit_user_message(write_command("x", "marker.txt"));
    let events = pump(&mut core, is_ready);
    assert!(!dir.join("marker.txt").exists());
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequest { .. })));
    let reply = assistant_text(&events);
    assert!(reply.contains("plan mode"), "{reply}");
    assert!(reply.contains("propose_plan"), "{reply}");

    // The plan-mode rules are part of the prompt.
    core.submit_user_message("SYSTEM".to_string());
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("</plan_mode>"));
}

#[test]
fn a_proposed_plan_waits_then_runs_in_the_approved_mode() {
    let _guard = setup();
    let dir = temp_dir("plan-approve");
    let mut core = open(&dir, ApprovalMode::Plan);

    let args = serde_json::json!({ "title": "Add marker", "plan": "| File | Change |\n|---|---|\n| marker.txt | create |" });
    core.submit_user_message(format!("CALL propose_plan {args}"));
    let events = pump(&mut core, is_ready);
    let (id, title, status) = plan_event(&events).expect("proposed_plan event");
    assert_eq!((title.as_str(), status.as_str()), ("Add marker", "pending"));
    // Bare `/plan` shows the waiting plan again.
    let (_, events) = core.handle_command("/plan");
    assert_eq!(plan_event(&events).map(|plan| plan.0), Some(id.clone()));
    // The turn ended at the plan: the model did not get to answer the tool.
    assert!(!assistant_text(&events).contains("tool said"));

    // A revision keeps plan mode and asks the model again (the TUI's
    // `/plan` command, the text twin of `approve_plan`).
    let (_, events) = core.handle_command(&format!("/plan {id} revise also add a test"));
    assert_eq!(plan_event(&events).unwrap().2, "revising");
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("also add a test"));

    let (_, events) = core.handle_command(&format!("/plan {id} approve full-access keep it short"));
    assert_eq!(plan_event(&events).unwrap().2, "approved");
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ApprovalMode { mode } if mode == "full-access"
    )));
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("approved the plan"));
    assert!(assistant_text(&events).contains("Their notes: keep it short"));
    let again = core.approve_plan(&id, true, None, "");
    assert!(matches!(again.as_slice(), [AgentEvent::Error(_)]));
    let (_, events) = core.handle_command("/plan");
    assert!(plan_event(&events).is_none());

    // Reloading the session shows the plan once, with its latest status.
    let transcript = core.transcript_event();
    let AgentEvent::Transcript(items) = transcript else {
        panic!("transcript event")
    };
    let plans: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            lynshen_agent_core::TranscriptItem::Plan { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(plans, vec!["approved".to_string()]);
}

#[test]
fn subagents_report_their_work_to_the_agent_trace() {
    let _guard = setup();
    let dir = temp_dir("trace");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "lister", "message": "RUN: ls" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    // The lifecycle events can come after `ready` on a slower machine: wait
    // for the turn and for them.
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let lifecycle = |event: &AgentEvent| matches!(event, AgentEvent::SubagentLifecycle { .. });
    while !(events.iter().any(is_ready) && events.iter().any(lifecycle))
        && Instant::now() < deadline
    {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(10));
    }
    // The child runs on its own thread; wait until it has finished.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
            unreachable!()
        };
        if rows
            .iter()
            .any(|row| row["state"] != "running" && row["state"] != "pending")
        {
            break;
        }
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(20));
    }

    let lifecycle = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SubagentLifecycle {
                label, tool_use_id, ..
            } => Some((label.clone(), tool_use_id.clone())),
            _ => None,
        })
        .expect("subagent_lifecycle");
    // Its card shows the nickname it was given, not the task name.
    assert_eq!(lifecycle.1, "call_1");
    assert!(
        !lifecycle.0.is_empty() && lifecycle.0 != "lister",
        "{lifecycle:?}"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::AgentRuns(_))));

    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    let row = rows
        .iter()
        .find(|row| row["task"] == "lister")
        .expect("row");
    assert_eq!(row["id"], "/root/lister");
    assert_eq!(row["tool_use_id"], "call_1");
    assert_eq!(row["type"], "subagent");
    assert_eq!(row["prompt"], "RUN: ls");
    assert!(row["started_at"].as_u64().unwrap() > 1_700_000_000_000);

    let AgentEvent::SubagentTranscript {
        items: Some(items), ..
    } = core.subagent_transcript_event("/root/lister")
    else {
        panic!("transcript")
    };
    // The child may still be working when the parent's turn ends and closes
    // it, so only its task is certain.
    assert_eq!(
        items[0],
        serde_json::json!({ "role": "user", "content": "RUN: ls" })
    );
    let AgentEvent::SubagentTranscript { items: None, .. } =
        core.subagent_transcript_event("/root/nobody")
    else {
        panic!("unknown agent")
    };

    // A later turn keeps the earlier turn's agent in the trace.
    core.submit_user_message("hello".to_string());
    pump(&mut core, is_ready);
    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    assert!(rows.iter().any(|row| row["task"] == "lister"));
    assert!(matches!(
        core.subagent_transcript_event("/root/lister"),
        AgentEvent::SubagentTranscript { items: Some(_), .. }
    ));
}

#[test]
fn a_steered_message_joins_the_running_turn_without_stopping_its_tool() {
    let _guard = setup();
    let dir = temp_dir("steer");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // A command that takes a moment, so the message arrives while it runs.
    let slow = if cfg!(windows) {
        "RUN: Start-Sleep -Milliseconds 800; Set-Content done.txt ok".to_string()
    } else {
        "RUN: sleep 0.8 && printf ok > done.txt".to_string()
    };
    core.submit_user_message(slow);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { .. }))
    {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(5));
    }
    core.submit_user_message("also check the weather".to_string());
    events.extend(core.steer());
    events.extend(pump(&mut core, is_ready));

    // The tool was not killed: its file exists.
    assert_eq!(fs::read_to_string(dir.join("done.txt")).unwrap(), "ok");
    // The message reached the model in the same turn (it answered it), and
    // shows as the user's message.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::UserMessage(m) if m == "also check the weather")));
    assert!(assistant_text(&events).contains("also check the weather"));
    let turns = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Status(s) if s == "ready"))
        .count();
    assert_eq!(turns, 1, "one turn, not a restart");
}

#[test]
fn a_queued_message_taken_back_never_runs() {
    let _guard = setup();
    let dir = temp_dir("unqueue");
    let mut core = open(&dir, ApprovalMode::FullAccess);
    let slow = if cfg!(windows) {
        "RUN: Start-Sleep -Milliseconds 600; Set-Content done.txt ok".to_string()
    } else {
        "RUN: sleep 0.6 && printf ok > done.txt".to_string()
    };
    core.submit_user_message(slow);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { .. }))
    {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(5));
    }
    core.submit_user_message("first queued".to_string());
    core.submit_user_message("second queued".to_string());
    // The client saw the second one at index 1; a stale index still finds it
    // by its text.
    let taken = core.unqueue(0, Some("second queued"));
    assert!(taken.iter().any(
        |e| matches!(e, AgentEvent::PendingMessages(m) if m == &["first queued".to_string()])
    ));
    assert!(taken
        .iter()
        .any(|e| matches!(e, AgentEvent::Unqueued(t) if t == "second queued")));
    // Text that is no longer queued changes nothing, and nothing comes back.
    let gone = core.unqueue(0, Some("gone"));
    assert!(gone.iter().any(
        |e| matches!(e, AgentEvent::PendingMessages(m) if m == &["first queued".to_string()])
    ));
    assert!(!gone.iter().any(|e| matches!(e, AgentEvent::Unqueued(_))));
    // The turn, then the one queued message as a turn of its own, until
    // that turn is ready.
    let done = |events: &[AgentEvent]| {
        events
            .iter()
            .position(|e| matches!(e, AgentEvent::UserMessage(m) if m == "first queued"))
            .is_some_and(|at| {
                events[at..]
                    .iter()
                    .any(|e| matches!(e, AgentEvent::Status(s) if s == "ready"))
            })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !done(&events) {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(5));
    }
    let users: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserMessage(m) => Some(m),
            _ => None,
        })
        .collect();
    assert!(users.iter().any(|m| *m == "first queued"), "{users:?}");
    assert!(!users.iter().any(|m| *m == "second queued"), "{users:?}");
    assert!(!assistant_text(&events).contains("second queued"));
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

#[test]
fn a_worker_runs_in_a_worktree_that_the_merge_op_brings_back() {
    let _guard = setup();
    let dir = temp_dir("team-merge");
    git(&dir, &["init", "-q"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "builder", "role": "worker", "message": "hello" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    let mut events = pump(&mut core, is_ready);
    let deadline = Instant::now() + Duration::from_secs(20);
    // The row can report the worker finished before its lifecycle events
    // are polled (a slower machine): wait for both.
    let worker_event = |events: &[AgentEvent]| {
        events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::SubagentLifecycle { role: Some(role), .. } if role == "worker"
            )
        })
    };
    let row = loop {
        events.extend(core.poll_events());
        let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
            unreachable!()
        };
        let row = rows
            .into_iter()
            .find(|row| row["task"] == "builder")
            .expect("row");
        if row["state"] != "running" && row["state"] != "pending" && worker_event(&events) {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the worker never finished, or no lifecycle event named its role"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(row["role"], "worker");
    assert_eq!(row["isolation"], "worktree");
    let workdir = std::path::PathBuf::from(row["workdir"].as_str().unwrap());
    assert!(workdir.starts_with(dir.join(".lynshen/agents")));
    // What the worker wrote, in its own worktree only.
    fs::write(workdir.join("feature.txt"), "done\n").unwrap();
    assert!(!dir.join("feature.txt").exists());

    let op = serde_json::json!({ "op": "merge_agent", "target": "builder", "action": "apply" });
    let (_, events) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    let merged = events
        .into_iter()
        .map(lynshen_agent_core::protocol::event_json)
        .collect::<Vec<_>>();
    let result = merged
        .iter()
        .find(|event| event["type"] == "merge_result")
        .expect("merge_result");
    assert_eq!(
        result,
        &serde_json::json!({
            "type": "merge_result", "target": "/root/builder", "action": "apply",
            "ok": true, "files": ["feature.txt"], "conflicts": [], "error": null
        })
    );
    assert!(merged
        .iter()
        .any(|event| event["type"] == "subagent_lifecycle"
            && event["status"] == "merged"
            && event["role"] == "worker"));
    let runs = merged
        .iter()
        .find(|event| event["type"] == "agent_runs")
        .expect("agent_runs");
    assert_eq!(runs["agents"][0]["state"], "merged");
    assert_eq!(
        fs::read_to_string(dir.join("feature.txt")).unwrap(),
        "done\n"
    );
    assert!(!workdir.exists());

    // Nothing left to merge: the op still answers with a merge_result.
    let (_, events) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    let again = events
        .into_iter()
        .map(lynshen_agent_core::protocol::event_json)
        .find(|event| event["type"] == "merge_result")
        .expect("merge_result");
    assert_eq!(again["ok"], false);
    assert!(again["error"].as_str().unwrap().contains("no worktree"));
}

#[test]
fn a_plan_step_names_its_agent_and_the_agent_events_carry_the_step() {
    let _guard = setup();
    let dir = temp_dir("team-plan");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let plan = serde_json::json!({ "plan": [
        { "step": "Map the parser", "status": "in_progress", "agent": "mapper", "files": ["src/parse.rs"] },
        { "step": "Write tests", "status": "pending" }
    ] });
    core.submit_user_message(format!("CALL update_plan {plan}"));
    let events = pump(&mut core, is_ready);
    let items = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Plan(items) => Some(items.clone()),
            _ => None,
        })
        .expect("plan event");
    assert_eq!(items[0].agent.as_deref(), Some("mapper"));
    assert_eq!(items[0].files, ["src/parse.rs"]);
    assert_eq!(items[1].agent, None);

    let args = serde_json::json!({ "task_name": "mapper", "role": "explorer", "message": "look" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    // The lifecycle events can come after `ready` on a slower machine: wait
    // for the turn and for them.
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let lifecycle = |event: &AgentEvent| matches!(event, AgentEvent::SubagentLifecycle { .. });
    while !(events.iter().any(is_ready) && events.iter().any(lifecycle))
        && Instant::now() < deadline
    {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(10));
    }
    let (role, step) = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SubagentLifecycle {
                role, plan_step, ..
            } => Some((role.clone(), plan_step.clone())),
            _ => None,
        })
        .expect("subagent_lifecycle");
    assert_eq!(role.as_deref(), Some("explorer"));
    assert_eq!(step.as_deref(), Some("Map the parser"));
}

fn rows(core: &AgentCore) -> Vec<serde_json::Value> {
    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    rows
}

fn row(core: &AgentCore, label: &str) -> serde_json::Value {
    rows(core)
        .into_iter()
        .find(|row| row["task"] == label)
        .unwrap_or_else(|| panic!("no agent {label}"))
}

fn finished(row: &serde_json::Value) -> bool {
    row["state"] != "running" && row["state"] != "pending"
}

fn readies(events: &[AgentEvent]) -> usize {
    events.iter().filter(|event| is_ready(event)).count()
}

/// Polls into `events` until `done` holds for them and the agent rows.
fn pump_until(
    core: &mut AgentCore,
    events: &mut Vec<AgentEvent>,
    done: impl Fn(&[AgentEvent], &[serde_json::Value]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        events.extend(core.poll_events());
        if done(events, &rows(core)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out; events: {events:#?}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Polls for `millis`, collecting what happens.
fn pump_for(core: &mut AgentCore, events: &mut Vec<AgentEvent>, millis: u64) {
    let until = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < until {
        events.extend(core.poll_events());
        thread::sleep(Duration::from_millis(10));
    }
}

fn wire(events: Vec<AgentEvent>) -> Vec<serde_json::Value> {
    events
        .into_iter()
        .map(lynshen_agent_core::protocol::event_json)
        .collect()
}

#[test]
fn a_background_agent_outlives_the_turn_and_its_result_wakes_the_main_agent_once() {
    let _guard = setup();
    let dir = temp_dir("team-background");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "scout", "background": true, "message": "[sleep:1200] look around" });
    let mut events = core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    // The turn ended; the agent works on.
    let scout = row(&core, "scout");
    assert_eq!(scout["background"], true);
    assert!(!finished(&scout), "{scout}");

    // Its result starts a main turn by itself.
    pump_until(&mut core, &mut events, |events, _| readies(events) == 2);
    assert_eq!(row(&core, "scout")["state"], "completed");
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::AgentMessage { from, to, .. } if from == "/root/scout" && to == "/root"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SubagentLifecycle { background: true, status, .. } if status == "completed"
    )));
    let reply = assistant_text(&events);
    assert!(
        reply.contains("<subagent_result path=\"/root/scout\" status=\"completed\">"),
        "{reply}"
    );
    // Once: nothing else starts.
    pump_for(&mut core, &mut events, 800);
    assert_eq!(readies(&events), 2);
    let starts = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::AssistantStart))
        .count();
    assert_eq!(starts, 2);
}

#[test]
fn the_close_agent_op_stops_a_background_agent_and_nothing_wakes() {
    let _guard = setup();
    let dir = temp_dir("team-close");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "sleeper", "background": true, "message": "[sleep:1500] wait" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    let mut events = Vec::new();
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(!finished(&row(&core, "sleeper")));

    let op = serde_json::json!({ "op": "close_agent", "target": "sleeper" });
    let (_, closed) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    let closed = wire(closed);
    assert!(
        closed
            .iter()
            .any(|event| event["type"] == "subagent_lifecycle"
                && event["path"] == "/root/sleeper"
                && event["status"] == "closed"
                && event["background"] == true),
        "{closed:#?}"
    );
    let runs = closed
        .iter()
        .find(|event| event["type"] == "agent_runs")
        .expect("agent_runs");
    assert_eq!(runs["agents"][0]["state"], "closed");
    assert_eq!(runs["agents"][0]["background"], true);

    // Its model call ends later; a closed agent wakes nobody.
    pump_for(&mut core, &mut events, 2200);
    assert_eq!(readies(&events), 1);
    assert_eq!(row(&core, "sleeper")["state"], "closed");

    let op = serde_json::json!({ "op": "close_agent", "target": "nobody" });
    let (_, unknown) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    assert!(unknown.iter().any(
        |event| matches!(event, AgentEvent::Error(message) if message.contains("agent not found"))
    ));
}

#[test]
fn resume_agent_runs_a_finished_agent_again_on_its_conversation() {
    let _guard = setup();
    let dir = temp_dir("team-resume");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let args = serde_json::json!({ "task_name": "helper", "background": true, "message": "[sleep:800] first task" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    let mut events = Vec::new();
    pump_until(&mut core, &mut events, |events, rows| {
        readies(events) == 2 && rows.iter().all(finished)
    });
    assert_eq!(row(&core, "helper")["state"], "completed");

    let args =
        serde_json::json!({ "target": "helper", "message": "[sleep:800][history] second task" });
    core.submit_user_message(format!("CALL resume_agent {args}"));
    pump_until(&mut core, &mut events, |events, rows| {
        readies(events) == 4 && rows.iter().all(finished)
    });
    let helper = row(&core, "helper");
    assert_eq!(helper["state"], "completed");
    // The second run saw the first one's conversation.
    let result = helper["result"].as_str().unwrap();
    assert!(result.starts_with("history: "), "{result}");
    assert!(result.contains("first task"), "{result}");
    assert!(result.contains("second task"), "{result}");
    let AgentEvent::SubagentTranscript {
        items: Some(items), ..
    } = core.subagent_transcript_event("/root/helper")
    else {
        panic!("transcript")
    };
    assert!(items.iter().any(
        |item| item["role"] == "user" && item["content"] == "[sleep:800][history] second task"
    ));
}

#[test]
fn the_board_and_the_worktree_registry_survive_an_engine_restart() {
    let _guard = setup();
    let dir = temp_dir("team-restart");
    git(&dir, &["init", "-q"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let task = serde_json::json!({ "title": "Ship the feature", "role": "worker" });
    core.submit_user_message(format!("CALL task_create {task}"));
    let mut events = Vec::new();
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    let board = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TaskBoard(tasks) => Some(tasks.clone()),
            _ => None,
        })
        .expect("task_board");
    assert_eq!(board[0]["id"], "t1");
    assert_eq!(board[0]["status"], "pending");

    let args = serde_json::json!({ "task_name": "builder", "role": "worker", "message": "build" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump_until(&mut core, &mut events, |events, rows| {
        readies(events) == 2 && rows.iter().all(finished)
    });
    let workdir = std::path::PathBuf::from(row(&core, "builder")["workdir"].as_str().unwrap());
    fs::write(workdir.join("feature.txt"), "done\n").unwrap();
    pump_for(&mut core, &mut events, 100);
    let session = core.session_id().to_string();
    drop(core);

    let mut core = open(&dir, ApprovalMode::FullAccess);
    let (_, resumed) = core.handle_command(&format!("/resume {session}"));
    let resumed = wire(resumed);
    let board = resumed
        .iter()
        .find(|event| event["type"] == "task_board")
        .expect("task_board on resume");
    assert_eq!(board["tasks"][0]["title"], "Ship the feature");
    let runs = resumed
        .iter()
        .find(|event| event["type"] == "agent_runs")
        .expect("agent_runs on resume");
    let builder = &runs["agents"][0];
    assert_eq!(builder["id"], "/root/builder");
    assert_eq!(builder["isolation"], "worktree");
    assert_eq!(builder["role"], "worker");
    assert!(finished(builder), "{builder}");
    // A client that attaches later gets the board too.
    assert!(core
        .state_events()
        .iter()
        .any(|event| matches!(event, AgentEvent::TaskBoard(tasks) if tasks.len() == 1)));

    let op = serde_json::json!({ "op": "merge_agent", "target": "builder", "action": "apply" });
    let (_, merged) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    let result = wire(merged)
        .into_iter()
        .find(|event| event["type"] == "merge_result")
        .expect("merge_result");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        fs::read_to_string(dir.join("feature.txt")).unwrap(),
        "done\n"
    );
    assert!(!workdir.exists());
}

#[test]
fn best_of_n_attempts_run_in_worktrees_and_pick_attempt_keeps_one() {
    let _guard = setup();
    let dir = temp_dir("team-attempts");
    git(&dir, &["init", "-q"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // Attempts need worktrees.
    let args = serde_json::json!({ "task_name": "fix", "attempts": 2, "message": "m" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    let mut events = Vec::new();
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolOutput { name, is_error: true, output, .. }
            if name == "spawn_agent" && output.contains("attempts needs isolation")
    )));
    assert!(rows(&core).is_empty());

    let args = serde_json::json!({ "task_name": "fix", "role": "worker", "attempts": 2, "message": "fix it" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump_until(&mut core, &mut events, |events, rows| {
        readies(events) == 2 && rows.len() == 2 && rows.iter().all(finished)
    });
    let first = row(&core, "fix_a1");
    let second = row(&core, "fix_a2");
    assert_eq!(
        (&first["attempt_group"], &first["attempt"]),
        (&serde_json::json!("fix"), &serde_json::json!(1))
    );
    assert_eq!(second["attempt"], 2);
    let first_dir = std::path::PathBuf::from(first["workdir"].as_str().unwrap());
    let second_dir = std::path::PathBuf::from(second["workdir"].as_str().unwrap());
    assert_ne!(first_dir, second_dir);
    fs::write(first_dir.join("one.txt"), "1\n").unwrap();
    fs::write(second_dir.join("two.txt"), "2\n").unwrap();

    let op = serde_json::json!({ "op": "pick_attempt", "group": "fix", "target": "fix_a2" });
    let (_, picked) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
    let merges: Vec<serde_json::Value> = wire(picked)
        .into_iter()
        .filter(|event| event["type"] == "merge_result")
        .collect();
    assert_eq!(merges.len(), 2, "{merges:#?}");
    assert_eq!(merges[0]["target"], "/root/fix_a2");
    assert_eq!(merges[0]["action"], "apply");
    assert_eq!(merges[0]["ok"], true);
    assert_eq!(merges[1]["target"], "/root/fix_a1");
    assert_eq!(merges[1]["action"], "discard");
    assert_eq!(merges[1]["ok"], true);
    assert!(dir.join("two.txt").exists());
    assert!(!dir.join("one.txt").exists());
    assert!(!first_dir.exists() && !second_dir.exists());
    assert_eq!(row(&core, "fix_a2")["state"], "merged");
    assert_eq!(row(&core, "fix_a1")["state"], "discarded");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_worktree_agents_shell_writes_only_its_worktree() {
    let _guard = setup();
    let dir = temp_dir("team-confined");
    git(&dir, &["init", "-q"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    // The default sandbox: workspace-write.
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // The worktree is <dir>/.lynshen/agents/<name>: ../../.. is the project.
    let args = serde_json::json!({
        "task_name": "w",
        "role": "worker",
        "background": true,
        "message": "RUN: printf in > inside.txt; printf out > ../../../escape.txt; printf x > ok.txt",
    });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    let mut events = Vec::new();
    pump_until(&mut core, &mut events, |events, rows| {
        readies(events) == 2 && rows.iter().all(finished)
    });
    let workdir = std::path::PathBuf::from(row(&core, "w")["workdir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("inside.txt")).unwrap(),
        "in"
    );
    assert!(workdir.join("ok.txt").exists());
    assert!(!dir.join("escape.txt").exists());
}

/// Sets `agents.team_v2` in the shared config.json; the file as it was comes
/// back when this is dropped (a failed test must not leave v2 off for the
/// others).
struct TeamSwitch {
    path: std::path::PathBuf,
    saved: String,
}

impl TeamSwitch {
    fn new() -> Self {
        let path = std::path::PathBuf::from(env::var("HOME").unwrap())
            .join(".lynshen")
            .join("config.json");
        let saved = fs::read_to_string(&path).unwrap();
        Self { path, saved }
    }

    fn set(&self, on: bool) {
        let mut config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&self.path).unwrap()).unwrap();
        config["agents"] = serde_json::json!({ "team_v2": on });
        fs::write(&self.path, config.to_string()).unwrap();
    }
}

impl Drop for TeamSwitch {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.saved);
    }
}

#[test]
fn switching_team_v2_off_lets_a_background_agent_finish_without_waking_anyone() {
    let _guard = setup();
    let switch = TeamSwitch::new();
    let dir = temp_dir("team-switch");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // Started while v2 is on.
    let args = serde_json::json!({ "task_name": "scout", "background": true, "message": "[sleep:1200] look around" });
    let mut events = core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(!finished(&row(&core, "scout")));

    // Switched off while it works, with no turn in between: it finishes,
    // and its result starts no turn.
    switch.set(false);
    pump_until(&mut core, &mut events, |_, rows| rows.iter().all(finished));
    assert_eq!(row(&core, "scout")["state"], "completed");
    pump_for(&mut core, &mut events, 800);
    assert_eq!(readies(&events), 1);

    // The stop and pick ops answer with an error.
    for op in [
        serde_json::json!({ "op": "close_agent", "target": "scout" }),
        serde_json::json!({ "op": "pick_attempt", "group": "fix", "target": "1" }),
    ] {
        let (_, answer) = lynshen_agent_core::protocol::apply_op(&mut core, &op);
        assert!(
            answer.iter().any(|event| matches!(
                event,
                AgentEvent::Error(message) if message.ends_with("agent team v2 (Beta) is switched off (agents.team_v2 is false)")
            )),
            "{answer:#?}"
        );
    }

    // The next turn reads the result before its first request.
    let mut events = core.submit_user_message("what did the scout find?".to_string());
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    let reply = assistant_text(&events);
    assert!(
        reply.contains("<subagent_result path=\"/root/scout\" status=\"completed\">"),
        "{reply}"
    );

    // Its tools are refused, and so is a background spawn.
    let mut events = core.submit_user_message("CALL task_list {}".to_string());
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolOutput { name, is_error: true, output, .. }
            if name == "task_list" && output.contains("agents.team_v2 is false")
    )));
    let args = serde_json::json!({ "task_name": "later", "background": true, "message": "m" });
    let mut events = core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolOutput { name, is_error: true, output, .. }
            if name == "spawn_agent" && output.contains("spawn_agent with background is not available")
    )));
    assert_eq!(rows(&core).len(), 1);

    // Switched on again, v2 is back from the next turn.
    switch.set(true);
    let mut events = core.submit_user_message("CALL task_list {}".to_string());
    pump_until(&mut core, &mut events, |events, _| readies(events) == 1);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolOutput { name, is_error: false, .. } if name == "task_list"
    )));
}

#[test]
fn an_engine_is_idle_only_when_nothing_runs_or_waits_in_it() {
    let _guard = setup();
    let dir = temp_dir("idle");
    let mut core = open(&dir, ApprovalMode::Manual);
    core.set_attended(false);
    assert!(core.is_idle());

    core.submit_user_message("RUN: printf idle > marker.txt".to_string());
    assert!(!core.is_idle());
    let events = pump(&mut core, is_ready);
    let id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ActionDeferred(action) => Some(action.id.clone()),
            _ => None,
        })
        .unwrap();
    // The deferred call waits on the user's decision.
    assert!(!core.is_idle());
    core.decide_action(&id, false);
    pump(&mut core, is_ready);
    assert!(core.is_idle());
}
