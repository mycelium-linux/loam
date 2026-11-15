//! Several adapters behind one roster.
//!
//! A desktop runs more than one harness: the scripted cast for demos, an ACP
//! agent for Operator or `delta-acp`, a session adapter for edgerunner, an org
//! chart for Agentsmith. The surface should not know which adapter produced a
//! row, so the host composes them once and hands `soil:agents` one backend.
//!
//! Identity is the agent id. When two adapters report the same id the first one
//! wins: an adapter that owns the session is listed before an adapter that
//! mirrors it. Control routes to the backend that can answer for that id, so a
//! prompt is never delivered to the wrong harness.

use std::time::{Duration, Instant};

use crate::{Agent, AgentBackend, Answer, Binding, Change, Error, Page, Phase, Scope};

pub struct CompositeAgents {
    backends: Vec<Box<dyn AgentBackend>>,
}

impl CompositeAgents {
    pub fn new(backends: Vec<Box<dyn AgentBackend>>) -> Self {
        Self { backends }
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// The backend that can answer for `id`, if one can.
    fn owner(&mut self, id: &str) -> Option<&mut Box<dyn AgentBackend>> {
        let index = self
            .backends
            .iter_mut()
            .position(|backend| backend.get(id).is_some())?;
        self.backends.get_mut(index)
    }
}

impl AgentBackend for CompositeAgents {
    fn list(&mut self) -> Vec<Agent> {
        let mut seen = std::collections::HashSet::new();
        let mut agents = Vec::new();
        for backend in &mut self.backends {
            for agent in backend.list() {
                if seen.insert(agent.id.clone()) {
                    agents.push(agent);
                }
            }
        }
        // The same order every adapter promises: a person is the scarce
        // resource, then the most recently active.
        agents.sort_by_key(|agent| {
            (
                agent.phase != Phase::NeedsYou,
                u64::MAX - agent.updated_at_ms,
            )
        });
        agents
    }

    fn get(&mut self, id: &str) -> Option<Agent> {
        self.backends.iter_mut().find_map(|backend| backend.get(id))
    }

    fn binding(&mut self) -> Binding {
        self.backends
            .first_mut()
            .map(|backend| backend.binding())
            .unwrap_or_default()
    }

    fn scope(&mut self) -> Scope {
        self.backends
            .first_mut()
            .map(|backend| backend.scope())
            .unwrap_or_default()
    }

    fn transcript(&mut self, id: &str, before: Option<u64>, limit: u32) -> Result<Page, Error> {
        self.owner(id)
            .ok_or(Error::NotFound)?
            .transcript(id, before, limit)
    }

    fn answer(&mut self, id: &str, prompt_id: &str, answer: Answer) -> Result<(), Error> {
        self.owner(id)
            .ok_or(Error::NotFound)?
            .answer(id, prompt_id, answer)
    }

    fn send(&mut self, id: &str, text: &str) -> Result<(), Error> {
        self.owner(id).ok_or(Error::NotFound)?.send(id, text)
    }

    fn interrupt(&mut self, id: &str) -> Result<(), Error> {
        self.owner(id).ok_or(Error::NotFound)?.interrupt(id)
    }

    /// Poll every adapter, because each owns its own stream. A zero timeout is
    /// a single non-blocking pass; a real timeout sleeps between passes instead
    /// of blocking on one adapter and starving the others.
    fn next(&mut self, timeout_ms: u32) -> Option<Change> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            for backend in &mut self.backends {
                if let Some(change) = backend.next(0) {
                    return Some(change);
                }
            }
            if timeout_ms == 0 || Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn refresh(&mut self) {
        for backend in &mut self.backends {
            backend.refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activity, ActivityKind, Goal, Status, Usage};

    fn agent(id: &str, phase: Phase, updated_at_ms: u64, prompts: usize) -> Agent {
        Agent {
            id: id.into(),
            harness: "test".into(),
            title: id.into(),
            workspace: None,
            model: None,
            phase,
            goal: Some(Goal {
                text: "finish".into(),
                set_at_ms: 0,
            }),
            owner: None,
            activity: vec![Activity {
                kind: ActivityKind::Other,
                label: "step".into(),
                detail: None,
                status: Status::Ok,
                started_at_ms: 0,
                finished_at_ms: Some(1),
            }],
            prompts: (0..prompts)
                .map(|index| crate::Prompt {
                    id: format!("p{index}"),
                    kind: crate::PromptKind::Question,
                    title: "answer me".into(),
                    body: None,
                    details: None,
                    origin: None,
                    choices: Vec::new(),
                    allows_text: true,
                    asked_at_ms: 0,
                    deadline_at_ms: None,
                })
                .collect(),
            usage: Usage::default(),
            revision: 1,
            started_at_ms: 0,
            updated_at_ms,
        }
    }

    struct Scripted {
        agents: Vec<Agent>,
        changes: Vec<Change>,
    }

    impl Scripted {
        fn new(agents: Vec<Agent>) -> Self {
            Self {
                agents,
                changes: Vec::new(),
            }
        }
    }

    impl AgentBackend for Scripted {
        fn list(&mut self) -> Vec<Agent> {
            self.agents.clone()
        }

        fn get(&mut self, id: &str) -> Option<Agent> {
            self.agents.iter().find(|agent| agent.id == id).cloned()
        }

        fn binding(&mut self) -> Binding {
            Binding::default()
        }

        fn transcript(
            &mut self,
            _id: &str,
            _before: Option<u64>,
            _limit: u32,
        ) -> Result<Page, Error> {
            Ok(Page::default())
        }

        fn answer(&mut self, id: &str, _prompt_id: &str, _answer: Answer) -> Result<(), Error> {
            if self.get(id).is_some() {
                Ok(())
            } else {
                Err(Error::NotFound)
            }
        }

        fn send(&mut self, id: &str, _text: &str) -> Result<(), Error> {
            if self.get(id).is_some() {
                Ok(())
            } else {
                Err(Error::NotFound)
            }
        }

        fn interrupt(&mut self, id: &str) -> Result<(), Error> {
            if self.get(id).is_some() {
                Ok(())
            } else {
                Err(Error::NotFound)
            }
        }

        fn next(&mut self, _timeout_ms: u32) -> Option<Change> {
            self.changes.pop()
        }
    }

    #[test]
    fn a_roster_is_the_union_needs_you_first() {
        let mut roster = CompositeAgents::new(vec![
            Box::new(Scripted::new(vec![agent("a", Phase::Working, 10, 0)])),
            Box::new(Scripted::new(vec![agent("b", Phase::NeedsYou, 5, 1)])),
        ]);
        let agents = roster.list();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id, "b", "the question outranks the busy row");
    }

    #[test]
    fn an_id_is_listed_once_and_the_first_owner_wins() {
        let mut roster = CompositeAgents::new(vec![
            Box::new(Scripted::new(vec![agent("same", Phase::Working, 10, 0)])),
            Box::new(Scripted::new(vec![agent("same", Phase::NeedsYou, 99, 1)])),
        ]);
        let agents = roster.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].phase, Phase::Working);
    }

    #[test]
    fn control_routes_to_the_owner_and_refuses_a_stranger() {
        let mut roster = CompositeAgents::new(vec![
            Box::new(Scripted::new(vec![agent("a", Phase::Working, 10, 0)])),
            Box::new(Scripted::new(vec![agent("b", Phase::Working, 10, 0)])),
        ]);
        roster.send("b", "hello").expect("b is listed");
        assert_eq!(roster.send("c", "hello"), Err(Error::NotFound));
    }

    #[test]
    fn next_drains_every_adapter() {
        let mut first = Scripted::new(Vec::new());
        first.changes.push(Change::Removed("a".into()));
        let mut second = Scripted::new(Vec::new());
        second.changes.push(Change::Removed("b".into()));
        let mut roster = CompositeAgents::new(vec![Box::new(first), Box::new(second)]);
        assert!(matches!(roster.next(0), Some(Change::Removed(_))));
        assert!(matches!(roster.next(0), Some(Change::Removed(_))));
        assert!(roster.next(0).is_none());
    }
}
