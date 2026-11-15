//! One adapter, several surfaces.
//!
//! A roster is process state, not window state: two windows that each spawned
//! their own adapter would disagree about the same conversation and pay for it
//! twice — an ACP adapter holds one process per conversation, so a wall and the
//! popup it opens would hold two.

use std::sync::{Arc, Mutex};

use crate::{Agent, AgentBackend, Answer, Binding, Change, Error, Page, Scope};

/// A backend shared by every surface in one process.
///
/// Cloning the handle shares the adapter; each call takes the lock in turn.
/// `next` is the only call that can block, and a surface's own tick bounds it,
/// so one window waiting for a change cannot wedge the others for longer than a
/// tick.
#[derive(Clone)]
pub struct SharedAgents(Arc<Mutex<Box<dyn AgentBackend>>>);

impl SharedAgents {
    pub fn new(backend: Box<dyn AgentBackend>) -> Self {
        Self(Arc::new(Mutex::new(backend)))
    }

    fn with<T>(&self, call: impl FnOnce(&mut (dyn AgentBackend + 'static)) -> T) -> T {
        let mut backend = self.0.lock().expect("agent backend poisoned");
        call(&mut **backend)
    }
}

impl AgentBackend for SharedAgents {
    fn list(&mut self) -> Vec<Agent> {
        self.with(|backend| backend.list())
    }

    fn get(&mut self, id: &str) -> Option<Agent> {
        self.with(|backend| backend.get(id))
    }

    fn binding(&mut self) -> Binding {
        self.with(|backend| backend.binding())
    }

    fn scope(&mut self) -> Scope {
        self.with(|backend| backend.scope())
    }

    fn transcript(&mut self, id: &str, before: Option<u64>, limit: u32) -> Result<Page, Error> {
        self.with(|backend| backend.transcript(id, before, limit))
    }

    fn answer(&mut self, id: &str, prompt_id: &str, answer: Answer) -> Result<(), Error> {
        self.with(|backend| backend.answer(id, prompt_id, answer))
    }

    fn send(&mut self, id: &str, text: &str) -> Result<(), Error> {
        self.with(|backend| backend.send(id, text))
    }

    fn interrupt(&mut self, id: &str) -> Result<(), Error> {
        self.with(|backend| backend.interrupt(id))
    }

    fn next(&mut self, timeout_ms: u32) -> Option<Change> {
        self.with(|backend| backend.next(timeout_ms))
    }

    fn refresh(&mut self) {
        self.with(|backend| backend.refresh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Phase, Usage};

    /// An adapter whose roster is its own state, which is the case sharing has
    /// to keep coherent.
    #[derive(Default)]
    struct Roster {
        agents: Vec<Agent>,
        refreshed: u32,
    }

    impl AgentBackend for Roster {
        fn list(&mut self) -> Vec<Agent> {
            self.agents.clone()
        }

        fn get(&mut self, id: &str) -> Option<Agent> {
            self.agents.iter().find(|agent| agent.id == id).cloned()
        }

        fn binding(&mut self) -> Binding {
            Binding::default()
        }

        fn transcript(&mut self, _id: &str, _before: Option<u64>, _limit: u32) -> Result<Page, Error> {
            Err(Error::Denied)
        }

        fn answer(&mut self, _id: &str, _prompt: &str, _answer: Answer) -> Result<(), Error> {
            Err(Error::Denied)
        }

        fn send(&mut self, _id: &str, _text: &str) -> Result<(), Error> {
            Err(Error::Denied)
        }

        fn interrupt(&mut self, _id: &str) -> Result<(), Error> {
            Err(Error::Denied)
        }

        fn refresh(&mut self) {
            self.refreshed += 1;
            self.agents.push(Agent {
                id: "listed".to_string(),
                harness: "test".to_string(),
                title: "listed".to_string(),
                workspace: None,
                model: None,
                phase: Phase::Idle,
                goal: None,
                owner: None,
                activity: Vec::new(),
                prompts: Vec::new(),
                usage: Usage::default(),
                revision: 0,
                started_at_ms: 0,
                updated_at_ms: 0,
            });
        }
    }

    #[test]
    fn two_windows_share_one_roster() {
        let mut wall = SharedAgents::new(Box::new(Roster::default()));
        let mut popup = wall.clone();

        assert!(wall.list().is_empty());
        wall.refresh();

        // The popup did not refresh, but it sees what the wall discovered.
        let listed = popup.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "listed");
        assert_eq!(popup.get("listed").map(|agent| agent.title), Some("listed".to_string()));
    }
}
