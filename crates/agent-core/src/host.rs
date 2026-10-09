//! Extensions a host process adds to an engine: extra tools the host runs
//! itself and text appended to the system prompt each turn. The daemon uses
//! this for long-lived agents (their brief, messaging and timers) and for
//! messages between conversations, without agent-core knowing about either.

use serde_json::Value;
use std::sync::{atomic::AtomicBool, Arc};

/// Runs a host tool: `(name, JSON arguments, stopped)` → `(output,
/// is_error)`. `stopped` turns true when the turn that made the call is
/// stopped; a tool that waits should give up then.
pub type HostToolRunner = Arc<dyn Fn(&str, &str, &AtomicBool) -> (String, bool) + Send + Sync>;

/// How the approval layer treats a host tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostGate {
    /// Runs without asking; refused in plan mode.
    Run,
    /// Only reads: runs without asking, in plan mode too.
    ReadOnly,
    /// Acts outside this conversation: asks under `manual` and `plan`, runs
    /// under the other modes. Plan mode lets it through; the host decides
    /// what it allows there.
    Outward,
    /// Asks under every mode, full access too, and is never allowlisted for
    /// the session.
    Ask,
}

#[derive(Clone)]
pub struct HostExtensions {
    /// Function-tool definitions, in the same shape as the built-in tools.
    pub tools: Vec<Value>,
    pub run_tool: HostToolRunner,
    /// Appended to the system prompt at the start of every turn, so the
    /// host can reflect state that changed since the last turn.
    pub prompt: Arc<dyn Fn() -> String + Send + Sync>,
    /// The session gets these tools only: no built-in, subagent or MCP
    /// tools (the daemon's dispatcher routes work and must not do it).
    pub exclusive: bool,
    /// How each of `tools` is gated, by name.
    pub gate: Arc<dyn Fn(&str) -> HostGate + Send + Sync>,
    /// The line an approval card shows for a gated call `(name, arguments)`.
    pub summary: Arc<dyn Fn(&str, &str) -> String + Send + Sync>,
}

impl HostExtensions {
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
    }

    /// The gate of `name` when it is one of these tools.
    pub fn gate_of(&self, name: &str) -> Option<HostGate> {
        self.has_tool(name).then(|| (self.gate)(name))
    }

    /// These tools and prompt followed by `other`'s. A name both offer runs
    /// here; the result is exclusive when either is.
    pub fn with(self, other: HostExtensions) -> HostExtensions {
        let mut tools = self.tools.clone();
        tools.extend(
            other
                .tools
                .iter()
                .filter(|tool| {
                    !tool
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| self.has_tool(name))
                })
                .cloned(),
        );
        let exclusive = self.exclusive || other.exclusive;
        let both = Arc::new((self, other));
        let (run, prompt, gate, summary) = (
            Arc::clone(&both),
            Arc::clone(&both),
            Arc::clone(&both),
            both,
        );
        HostExtensions {
            tools,
            exclusive,
            run_tool: Arc::new(move |name, arguments, stopped| {
                (pick(&run, name).run_tool)(name, arguments, stopped)
            }),
            prompt: Arc::new(move || format!("{}{}", (prompt.0.prompt)(), (prompt.1.prompt)())),
            gate: Arc::new(move |name| (pick(&gate, name).gate)(name)),
            summary: Arc::new(move |name, arguments| {
                (pick(&summary, name).summary)(name, arguments)
            }),
        }
    }
}

/// The host of a merged pair that runs `name`.
fn pick<'a>(both: &'a (HostExtensions, HostExtensions), name: &str) -> &'a HostExtensions {
    if both.0.has_tool(name) {
        &both.0
    } else {
        &both.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host(names: &[&str], gate: HostGate, said: &'static str) -> HostExtensions {
        HostExtensions {
            tools: names
                .iter()
                .map(|name| json!({ "type": "function", "name": name }))
                .collect(),
            run_tool: Arc::new(move |name, _, _| (format!("{said}:{name}"), false)),
            prompt: Arc::new(move || format!("<{said}>")),
            exclusive: false,
            gate: Arc::new(move |_| gate),
            summary: Arc::new(move |name, _| format!("{said} {name}")),
        }
    }

    #[test]
    fn merged_hosts_route_each_tool_to_its_own_host() {
        let merged = host(&["a", "b"], HostGate::Run, "one").with(host(
            &["b", "c"],
            HostGate::Outward,
            "two",
        ));
        let names: Vec<&str> = merged
            .tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        let stopped = AtomicBool::new(false);
        assert_eq!((merged.run_tool)("b", "{}", &stopped).0, "one:b");
        assert_eq!((merged.run_tool)("c", "{}", &stopped).0, "two:c");
        assert_eq!(merged.gate_of("a"), Some(HostGate::Run));
        assert_eq!(merged.gate_of("c"), Some(HostGate::Outward));
        assert_eq!(merged.gate_of("d"), None);
        assert_eq!((merged.summary)("c", "{}"), "two c");
        assert_eq!((merged.prompt)(), "<one><two>");
    }
}
