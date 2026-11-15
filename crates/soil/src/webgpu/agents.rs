//! Which harnesses a presenter attaches, in one place.
//!
//! A window and a capture host ask for the same three answers: the scripted
//! cast, a real ACP agent, or both behind the composite roster. Keeping the
//! construction here means a new source is one match arm, not a second CLI
//! dialect.

use soil_agent_host::{AgentBackend, CompositeAgents, FixtureAgents, NoSessions};

use crate::acp::AcpBackend;
use crate::dispatch::DispatchRole;
use crate::edgerunner::EdgerunnerSessions;

/// Build the backend a presenter asked for.
///
/// `source` is the CLI vocabulary (`demo`, `acp`, `all`). The default ACP command
/// is `delta-acp` because that is the adapter that exists today; Operator passes
/// its own command or an edgerunner service URL later.
pub fn agent_backend(
    source: &str,
    acp_command: Option<&str>,
    agent: Option<String>,
    view: Option<String>,
) -> Result<Box<dyn AgentBackend>, String> {
    let demo = || {
        Box::new(FixtureAgents::new().bind(agent.clone(), view.clone())) as Box<dyn AgentBackend>
    };
    match source {
        "demo" => Ok(demo()),
        "acp" => {
            let command = acp_command.unwrap_or("delta-acp");
            Ok(Box::new(AcpBackend::spawn(command)?.bind(agent, view)))
        }
        "all" => {
            let fixture = demo();
            let command = acp_command.unwrap_or("delta-acp");
            let acp = AcpBackend::spawn(command)?.bind(agent, view);
            Ok(Box::new(CompositeAgents::new(vec![fixture, Box::new(acp)])))
        }
        other => Err(format!("unknown agent source {other:?} (demo, acp, all)")),
    }
}

/// Who creates a session when a task notice arrives.
///
/// `endpoint` is an edgerunner service (`http://127.0.0.1:7777`). Without one
/// this surface still owns dispatch and refuses every task, publishing
/// `agent.task.failed.v1` — a person learns the task went nowhere instead of
/// watching an empty wall.
pub fn session_role(endpoint: Option<&str>) -> DispatchRole {
    match endpoint {
        Some(endpoint) => DispatchRole::Own(Box::new(EdgerunnerSessions::new(endpoint))),
        None => DispatchRole::Own(Box::new(NoSessions)),
    }
}

/// The same, for a second surface in a process that already has a dispatcher:
/// it watches for announcements and leaves the task queue alone.
pub fn watch_only() -> DispatchRole {
    DispatchRole::Watch
}
