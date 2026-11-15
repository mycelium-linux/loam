//! The dispatch seam: `agent.task.v1` in, `agent.session.v1` out.
//!
//! A person asks for work by publishing a task notice. The host turns it into a
//! session through the one authority that owns session creation, announces the
//! session, and every adapter that enumerates sessions re-lists. Nothing here
//! polls a harness: a tick with no notice costs one bounded bus read, which is
//! the difference between noticing a new session and discovering it by polling.
//!
//! The host owns this half because it owns the transport and the bus. A Spore
//! observes and answers; starting a session is a separate authority, so the
//! guest never sees this module.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{Value, json};
use soil_agent_host::{
    AgentBackend, Created, Error, NoSessions, SESSION_NOTICE, Sessions, TASK_FAILED_NOTICE,
    TASK_NOTICE, Task,
};

use crate::unibus::{Bus, BusError, Frame};

/// How long a notice stays available to a surface that was not listening yet.
/// Long enough for a window that opens a minute later, short enough that a
/// stale announcement dies with the process.
const NOTICE_TTL_MS: u64 = 60 * 60 * 1000;

/// Frames one read may return, so a tick stays bounded.
const PAGE: u32 = 16;

/// How many frames the pump remembers.
///
/// An in-process bus replays its retained window on every read (`None` means
/// "from the head", not "since last time"), so a frame is acted on once by
/// identity and the ring is what makes a replayed window free. A live bus
/// drains instead, where the ring simply never fills.
const SEEN: usize = 64;

/// What identifies one notice: kind, producer, issue time and body. Two frames
/// that agree on all four are the same notice, however often the bus hands them
/// back.
type Mark = (String, String, u64, Vec<u8>);

/// Which read a cursor belongs to. The bus takes one cursor per kind set.
#[derive(Clone, Copy)]
enum Slot {
    Tasks,
    Notices,
}

/// What a host does with the bus.
///
/// One process has one dispatcher. Every other surface watches the notices it
/// publishes — a task read twice would become two sessions, and a refusal
/// announced twice would read as two failed dispatches.
pub enum DispatchRole {
    /// This surface owns dispatch: it turns a task notice into a session.
    Own(Box<dyn Sessions>),
    /// This surface only re-lists when a session is announced.
    Watch,
}

/// How this host reads the bus.
enum Mode {
    Own(Box<dyn Sessions>),
    Watch,
}

/// Turns task notices into sessions and announces what it created.
pub struct Dispatch {
    bus: Arc<dyn Bus>,
    mode: Mode,
    tasks: Option<String>,
    notices: Option<String>,
    seen: VecDeque<Mark>,
}

impl Dispatch {
    pub fn new(bus: Arc<dyn Bus>, sessions: Box<dyn Sessions>) -> Self {
        Self {
            bus,
            mode: Mode::Own(sessions),
            tasks: None,
            notices: None,
            seen: VecDeque::new(),
        }
    }

    /// A host with no dispatcher: a task is still announced as failed rather
    /// than dropped, which is the whole point of the failure notice.
    pub fn observe_only(bus: Arc<dyn Bus>) -> Self {
        Self::new(bus, Box::new(NoSessions))
    }

    /// A second surface in the same process: it re-lists, and leaves the task
    /// queue to the one surface that owns dispatch.
    pub fn watcher(bus: Arc<dyn Bus>) -> Self {
        Self {
            bus,
            mode: Mode::Watch,
            tasks: None,
            notices: None,
            seen: VecDeque::new(),
        }
    }

    /// One tick.
    ///
    /// `agents` is the roster the announcement is about: a session created here
    /// is announced for other surfaces *and* re-listed for this one, so a window
    /// shows the new row now instead of on its next notice read.
    pub fn pump(&mut self, agents: &mut dyn AgentBackend) {
        let mut relist = !self.read(Slot::Notices, SESSION_NOTICE).is_empty();

        // A surface that only watches leaves the task queue to the one that
        // owns dispatch: a task read twice would become two sessions.
        let tasks = match self.mode {
            Mode::Own(_) => self.read(Slot::Tasks, TASK_NOTICE),
            Mode::Watch => Vec::new(),
        };
        if let Mode::Own(sessions) = &mut self.mode {
            for frame in tasks {
                let Some(task) = task_from_body(&frame.body) else {
                    // A malformed task is a failed dispatch: somebody asked and
                    // the work will not happen, which is exactly what must not
                    // be silent.
                    publish(&self.bus, TASK_FAILED_NOTICE, &failure_body(None, None, Error::Invalid));
                    continue;
                };
                match sessions.create(&task) {
                    Ok(created) => {
                        publish(&self.bus, SESSION_NOTICE, &session_body(&created));
                        relist = true;
                    }
                    Err(error) => {
                        publish(
                            &self.bus,
                            TASK_FAILED_NOTICE,
                            &failure_body(Some(&task.harness), task.title.as_deref(), error),
                        );
                    }
                }
            }
        }

        if relist {
            agents.refresh();
        }
    }

    /// Read one kind and return only the frames this pump has not acted on.
    ///
    /// A cursor the bus has outrun is `stale`, not an empty poll: the next read
    /// starts at the head again, which is what a live tail with no retention
    /// means.
    fn read(&mut self, slot: Slot, kind: &str) -> Vec<Frame> {
        let cursor = self.cursor(slot).as_ref().cloned();
        let page = match self.bus.poll(&[kind.to_string()], cursor.as_deref(), PAGE) {
            Ok(page) => page,
            Err(BusError::Stale) => {
                *self.cursor(slot) = None;
                return Vec::new();
            }
            Err(_) => return Vec::new(),
        };
        *self.cursor(slot) = page.next;
        self.fresh(page.frames)
    }

    fn cursor(&mut self, slot: Slot) -> &mut Option<String> {
        match slot {
            Slot::Tasks => &mut self.tasks,
            Slot::Notices => &mut self.notices,
        }
    }

    fn fresh(&mut self, frames: Vec<Frame>) -> Vec<Frame> {
        let mut out = Vec::new();
        for frame in frames {
            let mark = (
                frame.kind.clone(),
                frame.producer.clone(),
                frame.issued_at_ms,
                frame.body.clone(),
            );
            if self.seen.contains(&mark) {
                continue;
            }
            if self.seen.len() == SEEN {
                self.seen.pop_front();
            }
            self.seen.push_back(mark);
            out.push(frame);
        }
        out
    }

}

/// A publish that fails is logged where the host can see it: the guest is not
/// the audience for a control-plane notice.
fn publish(bus: &Arc<dyn Bus>, kind: &str, body: &str) {
    if let Err(error) = bus.publish(kind, body.as_bytes(), None, NOTICE_TTL_MS) {
        tracing::warn!(kind, %error, "session notice not published");
    }
}

/// Parse `agent.task.v1`.
///
/// `text` is required — a task that names no work is malformed, not a task with
/// a default. `harness` and `environment` default to the local Operator lane,
/// which is the one that exists today; a publisher that cares says so.
fn task_from_body(body: &[u8]) -> Option<Task> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text = value.get("text").and_then(Value::as_str)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(Task {
        text: text.to_string(),
        harness: value
            .get("harness")
            .and_then(Value::as_str)
            .unwrap_or("operator")
            .to_string(),
        environment: value
            .get("environment")
            .and_then(Value::as_str)
            .unwrap_or("local")
            .to_string(),
        title: value.get("title").and_then(Value::as_str).map(str::to_string),
    })
}

fn session_body(created: &Created) -> String {
    let mut body = json!({
        "id": created.id,
        "harness": created.harness,
        "title": created.title,
    });
    if let Some(workspace) = &created.workspace {
        body["workspace"] = json!(workspace);
    }
    if let Some(model) = &created.model {
        body["model"] = json!(model);
    }
    body.to_string()
}

fn failure_body(harness: Option<&str>, title: Option<&str>, error: Error) -> String {
    let mut body = json!({"cause": error_name(error)});
    if let Some(harness) = harness {
        body["harness"] = json!(harness);
    }
    if let Some(title) = title {
        body["title"] = json!(title);
    }
    body.to_string()
}

/// The `soil:agents/types.error` name, so a reading surface does not need a
/// second vocabulary for the same refusal.
fn error_name(error: Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::NotFound => "not-found",
        Error::NotReady => "not-ready",
        Error::Invalid => "invalid",
        Error::OverBudget => "over-budget",
        Error::Stale => "stale",
        Error::Unsupported => "unsupported",
        Error::Io => "io",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use soil_agent_host::{Agent, Binding, Page};

    use super::*;
    use crate::unibus::MemoryBus;

    const TASK: &[u8] = br#"{"text":"fix the flaky test","harness":"operator","environment":"boat","title":"flaky lane"}"#;

    /// An adapter that only knows how many times it was told to re-list.
    #[derive(Default)]
    struct Recorder {
        refreshed: u32,
    }

    impl AgentBackend for Recorder {
        fn list(&mut self) -> Vec<Agent> {
            Vec::new()
        }

        fn get(&mut self, _id: &str) -> Option<Agent> {
            None
        }

        fn binding(&mut self) -> Binding {
            Binding::default()
        }

        fn transcript(&mut self, _id: &str, _before: Option<u64>, _limit: u32) -> Result<Page, Error> {
            Err(Error::Denied)
        }

        fn answer(&mut self, _id: &str, _prompt: &str, _answer: soil_agent_host::Answer) -> Result<(), Error> {
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
        }
    }

    /// A dispatcher that records what it was asked to create.
    struct Script {
        asked: Arc<Mutex<Vec<Task>>>,
        failure: Option<Error>,
    }

    impl Script {
        fn ok(asked: Arc<Mutex<Vec<Task>>>) -> Self {
            Self {
                asked,
                failure: None,
            }
        }
    }

    impl Sessions for Script {
        fn create(&mut self, task: &Task) -> Result<Created, Error> {
            self.asked.lock().expect("script poisoned").push(task.clone());
            if let Some(failure) = self.failure {
                return Err(failure);
            }
            Ok(Created {
                id: "session-1".to_string(),
                harness: task.harness.clone(),
                title: task.title.clone().unwrap_or_else(|| task.text.clone()),
                workspace: None,
                model: None,
            })
        }
    }

    fn seed(bus: &MemoryBus, kind: &str, body: &[u8]) {
        bus.publish(kind, body, None, 60_000).expect("seed publish");
    }

    fn body_of(bus: &MemoryBus, kind: &str) -> Vec<Value> {
        bus.frames()
            .into_iter()
            .filter(|frame| frame.kind == kind)
            .map(|frame| serde_json::from_slice(&frame.body).expect("notice body"))
            .collect()
    }

    #[test]
    fn a_task_becomes_a_session_and_is_announced() {
        let bus = Arc::new(MemoryBus::default());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut dispatch = Dispatch::new(bus.clone(), Box::new(Script::ok(asked.clone())));
        let mut agents = Recorder::default();
        seed(&bus, TASK_NOTICE, TASK);

        dispatch.pump(&mut agents);

        let asked = asked.lock().expect("script poisoned");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].text, "fix the flaky test");
        assert_eq!(asked[0].harness, "operator");
        assert_eq!(asked[0].environment, "boat");
        drop(asked);

        let notices = body_of(&bus, SESSION_NOTICE);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["id"], "session-1");
        assert_eq!(notices[0]["title"], "flaky lane");
        assert_eq!(notices[0]["harness"], "operator");
        // The creating host shows the row now, not on its next notice read.
        assert_eq!(agents.refreshed, 1);
    }

    #[test]
    fn a_replayed_window_is_acted_on_once() {
        let bus = Arc::new(MemoryBus::default());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut dispatch = Dispatch::new(bus.clone(), Box::new(Script::ok(asked.clone())));
        let mut agents = Recorder::default();
        seed(&bus, TASK_NOTICE, TASK);

        // An in-process bus hands the same retained frame back on every read;
        // one task must not become five sessions.
        for _ in 0..5 {
            dispatch.pump(&mut agents);
        }

        assert_eq!(asked.lock().expect("script poisoned").len(), 1);
        assert_eq!(body_of(&bus, SESSION_NOTICE).len(), 1);
        assert_eq!(body_of(&bus, TASK_FAILED_NOTICE).len(), 0);
    }

    #[test]
    fn a_refused_dispatch_is_announced() {
        let bus = Arc::new(MemoryBus::default());
        let mut dispatch = Dispatch::observe_only(bus.clone());
        let mut agents = Recorder::default();
        seed(&bus, TASK_NOTICE, TASK);

        dispatch.pump(&mut agents);

        let failures = body_of(&bus, TASK_FAILED_NOTICE);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["cause"], "unsupported");
        assert_eq!(failures[0]["harness"], "operator");
        assert_eq!(failures[0]["title"], "flaky lane");
        // Nothing was created, so nothing claims to have been.
        assert_eq!(body_of(&bus, SESSION_NOTICE).len(), 0);
        assert_eq!(agents.refreshed, 0);
    }

    #[test]
    fn a_malformed_task_fails_loudly() {
        let bus = Arc::new(MemoryBus::default());
        let mut dispatch = Dispatch::observe_only(bus.clone());
        let mut agents = Recorder::default();
        seed(&bus, TASK_NOTICE, br#"{"text":"  "}"#);

        dispatch.pump(&mut agents);

        let failures = body_of(&bus, TASK_FAILED_NOTICE);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["cause"], "invalid");
    }

    #[test]
    fn a_watcher_re_lists_but_does_not_dispatch() {
        let bus = Arc::new(MemoryBus::default());
        let asked: Arc<Mutex<Vec<Task>>> = Arc::new(Mutex::new(Vec::new()));
        let mut dispatch = Dispatch::watcher(bus.clone());
        let mut agents = Recorder::default();
        seed(&bus, TASK_NOTICE, TASK);
        seed(&bus, SESSION_NOTICE, br#"{"id":"session-3"}"#);

        dispatch.pump(&mut agents);

        // The same task frame is on the bus the dispatcher reads; a second
        // reader must not make a second session of it, nor announce a failure
        // for work it never tried.
        assert!(asked.lock().expect("script poisoned").is_empty());
        assert_eq!(body_of(&bus, SESSION_NOTICE).len(), 1);
        assert_eq!(body_of(&bus, TASK_FAILED_NOTICE).len(), 0);
        assert_eq!(agents.refreshed, 1);
    }

    #[test]
    fn a_session_notice_re_lists_once() {
        let bus = Arc::new(MemoryBus::default());
        let mut dispatch = Dispatch::observe_only(bus.clone());
        let mut agents = Recorder::default();
        seed(&bus, SESSION_NOTICE, br#"{"id":"session-2"}"#);

        dispatch.pump(&mut agents);
        dispatch.pump(&mut agents);

        assert_eq!(agents.refreshed, 1);
    }
}
