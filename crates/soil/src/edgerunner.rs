//! The dispatcher's sink: edgerunner owns session incarnation.
//!
//! The host asks; it never starts a process of its own. `POST /v1/sessions`
//! takes `CreateAgentSessionRequest` and answers an `AgentSession`, so the two
//! facts the host needs back — does it exist, and what is it called — come from
//! the authority that issued the id.
//!
//! Nothing here retries or queues. A refusal becomes `agent.task.failed.v1`,
//! which is a fact a person can act on; a silent retry would hide the same
//! failure behind a spinner.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use soil_agent_host::{Created, Error, Sessions, Task};

use crate::http_host::{DirectTransport, HttpTransport, TransportRequest};

/// A session create is a control-plane call: it starts a runner, possibly a
/// VM, so the budget is generous but still bounded.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest create response the host will read. A session record is small;
/// a body past this is not one.
const MAX_CREATE_BYTES: usize = 1 << 20;

/// Creates sessions through an edgerunner service.
pub struct EdgerunnerSessions {
    /// The service base, e.g. `http://127.0.0.1:7777`. No trailing slash.
    endpoint: String,
    transport: Arc<dyn HttpTransport>,
}

impl EdgerunnerSessions {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            transport: Arc::new(DirectTransport),
        }
    }

    /// The host's transport, so a fleet that routes through a gateway or a
    /// Mycelium peer keeps the same one for dispatch as for every other call.
    pub fn with_transport(endpoint: impl Into<String>, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            transport,
        }
    }
}

impl Sessions for EdgerunnerSessions {
    fn create(&mut self, task: &Task) -> Result<Created, Error> {
        let mut body = json!({
            "prompt": task.text,
            "agent": task.harness,
            // The task says where the work belongs; edgerunner decides whether
            // that is a runner it has, and refuses the create if it is not.
            "runner": task.environment,
            "auto_start": true,
        });
        if let Some(title) = &task.title {
            body["title"] = json!(title);
        }

        let request = TransportRequest::from_parts(
            &format!("{}/v1/sessions", self.endpoint),
            "post",
            vec![("content-type".to_string(), "application/json".to_string())],
            Some(body.to_string().into_bytes()),
            CREATE_TIMEOUT,
            MAX_CREATE_BYTES,
        )
        .map_err(|_| Error::Invalid)?;

        let response = self.transport.fetch(&request).map_err(|_| Error::Io)?;
        if !(200..300).contains(&response.status) {
            // A 4xx means the task asked for something the fleet does not have;
            // a 5xx means the fleet is broken. Both are `io` here because the
            // surface's vocabulary has no finer split, and the body was logged
            // by the transport.
            return Err(Error::Io);
        }
        let session: Value = serde_json::from_slice(&response.body).map_err(|_| Error::Io)?;
        let id = session
            .get("id")
            .and_then(Value::as_str)
            .ok_or(Error::Io)?;

        Ok(Created {
            id: id.to_string(),
            harness: task.harness.clone(),
            title: session
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| task.title.clone())
                .unwrap_or_else(|| task.text.lines().next().unwrap_or("session").to_string()),
            // A repo URL is not a workspace root; the adapter that enumerates
            // sessions reports where the checkout actually is.
            workspace: None,
            model: session
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::http_host::{TransportError, TransportResponse};

    use super::*;

    /// A transport that answers one scripted response and keeps the request.
    struct Fake {
        status: u16,
        body: &'static str,
        seen: Arc<Mutex<Vec<TransportRequest>>>,
    }

    impl HttpTransport for Fake {
        fn fetch(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError> {
            self.seen
                .lock()
                .expect("fake poisoned")
                .push(request.clone());
            Ok(TransportResponse {
                status: self.status,
                final_url: request.url.to_string(),
                headers: Vec::new(),
                body: self.body.as_bytes().to_vec(),
            })
        }
    }

    fn task() -> Task {
        Task {
            text: "fix the flaky test\nand say why".to_string(),
            harness: "operator".to_string(),
            environment: "boat".to_string(),
            title: Some("flaky lane".to_string()),
        }
    }

    #[test]
    fn a_task_posts_the_create_request() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sessions = EdgerunnerSessions::with_transport(
            "http://127.0.0.1:7777/",
            Arc::new(Fake {
                status: 201,
                body: r#"{"id":"s-9","title":"flaky lane","status":"running","created_at":1,"updated_at":1}"#,
                seen: seen.clone(),
            }),
        );

        let created = sessions.create(&task()).expect("create");

        assert_eq!(created.id, "s-9");
        assert_eq!(created.harness, "operator");
        assert_eq!(created.title, "flaky lane");
        assert_eq!(created.workspace, None);

        let seen = seen.lock().expect("fake poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url.as_str(), "http://127.0.0.1:7777/v1/sessions");
        assert_eq!(seen[0].method_name(), "post");
        let body: Value = serde_json::from_slice(seen[0].body.as_deref().expect("body"))
            .expect("request body");
        assert_eq!(body["prompt"], "fix the flaky test\nand say why");
        assert_eq!(body["agent"], "operator");
        assert_eq!(body["runner"], "boat");
        assert_eq!(body["title"], "flaky lane");
        assert_eq!(body["auto_start"], true);
    }

    #[test]
    fn a_refusal_is_an_error_not_a_session() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sessions = EdgerunnerSessions::with_transport(
            "http://127.0.0.1:7777",
            Arc::new(Fake {
                status: 503,
                body: r#"{"error":"no runner"}"#,
                seen,
            }),
        );

        assert_eq!(sessions.create(&task()), Err(Error::Io));
    }

    #[test]
    fn a_session_without_an_id_is_an_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sessions = EdgerunnerSessions::with_transport(
            "http://127.0.0.1:7777",
            Arc::new(Fake {
                status: 200,
                body: r#"{"title":"flaky lane"}"#,
                seen,
            }),
        );

        assert_eq!(sessions.create(&task()), Err(Error::Io));
    }
}
