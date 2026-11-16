//! Loose Marbles adapter.
//!
//! Soil calls the stable `mb ... --json` CLI contract. There is no compile-time
//! Marbles dependency, daemon socket, or copied task database. The host owns
//! project scope, endpoint, token file and actor.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// `list --all` returns every issue with its description: six thousand issues
/// is twelve megabytes of JSON, which is a normal project rather than a
/// runaway. The default cap is for the calls that are supposed to be small.
const MAX_FULL_LIST: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct MarblesClient {
    binary: PathBuf,
    project: Option<String>,
    url: Option<String>,
    token_file: Option<PathBuf>,
    actor: Option<String>,
}

/// What finishing the work delivered.
///
/// The tracker refuses to end an item on a shrug: either a merged pull request
/// or commit proves it shipped, or an acknowledgement says nothing was supposed
/// to ship. Both are recorded, so "done" can be audited later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery<'a> {
    Merged {
        pr: Option<&'a str>,
        commit: Option<&'a str>,
    },
    Nothing {
        reason: &'a str,
    },
}

/// One edit to an item, in the tracker's own vocabulary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit<'a> {
    pub title: Option<&'a str>,
    pub status: Option<&'a str>,
    pub priority: Option<i64>,
    /// Hand the work to this actor. This is a claim, not a metadata field: the
    /// tracker records who now holds the item.
    pub assignee: Option<&'a str>,
    /// Append to the item's notes rather than replacing the description.
    pub note: Option<&'a str>,
}

/// Scope for a burn-down series: exactly one of a project, a goal or an epic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Burndown<'a> {
    Project(&'a str),
    Goal(&'a str),
    Epic(&'a str),
}

impl MarblesClient {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            project: None,
            url: None,
            token_file: None,
            actor: None,
        }
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn with_token_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.token_file = Some(path.into());
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn list(&self) -> Result<Vec<Issue>> {
        self.run_json(&["list"])
    }

    pub fn ready(&self) -> Result<Vec<Issue>> {
        self.run_json(&["ready"])
    }

    /// Every issue, including closed ones.
    ///
    /// `list` stops at what is still open, which is the wrong filter for a
    /// board: a column of finished work is a fact about the project, and a
    /// board that cannot show it has no way to answer "what shipped".
    pub fn list_all(&self) -> Result<Vec<Issue>> {
        self.run_json_capped(&["list", "--all"], MAX_FULL_LIST)
    }

    pub fn show(&self, id: &str) -> Result<Issue> {
        let mut issues: Vec<Issue> = self.run_json(&["show", id])?;
        issues
            .pop()
            .with_context(|| format!("Marbles issue {id} not found"))
    }

    pub fn create(&self, title: &str, description: &str) -> Result<String> {
        let mut args = vec!["create".to_string(), title.to_string()];
        if !description.is_empty() {
            args.push("--description".into());
            args.push(description.into());
        }
        let value: Value = self.run_json_owned(&args)?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Marbles create returned no id")
    }

    pub fn claim(&self, id: &str) -> Result<Value> {
        self.run_json(&["claim", id])
    }

    /// Claim the item on behalf of `actor`.
    ///
    /// Handing work to somebody else is the same operation as taking it: the
    /// actor that holds the claim is the actor the tracker will report it to.
    /// `update --assignee` is the newer spelling; this one is what the CLI in
    /// service supports today, and it is the same state change.
    pub fn claim_as(&self, id: &str, actor: &str) -> Result<Value> {
        self.run_json_owned(&claim_args(id, Some(actor)))
    }

    pub fn close(&self, id: &str, reason: &str) -> Result<Issue> {
        let mut args = vec!["close".to_string(), id.to_string()];
        if !reason.is_empty() {
            args.push("--reason".into());
            args.push(reason.into());
        }
        self.run_json_owned(&args)
    }

    /// Move the item to review: the pull request is open, the merge is not.
    ///
    /// The tracker's own words for the state machine matter here. `review` means
    /// a person or a pipeline still has to decide; only `close` ends the item,
    /// and only with evidence.
    pub fn review(&self, id: &str, pr: Option<&str>, commit: Option<&str>) -> Result<Issue> {
        self.run_json_owned(&review_args(id, pr, commit))
    }

    /// Heartbeat a claim. Claims carry TTLs, and a long build is exactly when one
    /// expires out from under the agent that is still working.
    pub fn touch(&self, id: &str) -> Result<Value> {
        self.run_json_owned(&touch_args(id, None))
    }

    /// Heartbeat a claim held by `actor` (an agent's session id, not its name).
    pub fn touch_as(&self, id: &str, actor: &str) -> Result<Value> {
        self.run_json_owned(&touch_args(id, Some(actor)))
    }

    /// Give the claim back without finishing the work, so the next agent is not
    /// blocked by an expired lease.
    pub fn release(&self, id: &str) -> Result<Value> {
        self.run_json_owned(&release_args(id, None))
    }

    pub fn release_as(&self, id: &str, actor: &str) -> Result<Value> {
        self.run_json_owned(&release_args(id, Some(actor)))
    }

    /// Close the item with the evidence that ends it.
    pub fn close_with(&self, id: &str, delivery: Delivery<'_>) -> Result<Issue> {
        self.run_json_owned(&close_args(id, delivery))
    }

    /// Edit an item. `assignee` is a claim, not a metadata edit: handing work to
    /// a person or a PM is the tracker's assignment.
    pub fn update(&self, id: &str, edit: Edit<'_>) -> Result<Issue> {
        self.run_json_owned(&update_args(id, edit))
    }

    /// Folder → project → goal rollup: ready work, WIP, review/blocked, staleness
    /// and burn rate. The view a PM reads, as the tracker computes it.
    pub fn portfolio(&self, folder: Option<&str>, days: i64, stale_days: i64) -> Result<Value> {
        self.run_json_owned(&portfolio_args(folder, days, stale_days))
    }

    /// Daily remaining-work series and burn rate for one project, goal or epic.
    pub fn burndown(&self, scope: Burndown<'_>) -> Result<Value> {
        self.run_json_owned(&burndown_args(scope))
    }

    fn run_json<T>(&self, args: &[&str]) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.run_json_capped(args, MAX_OUTPUT)
    }

    fn run_json_capped<T>(&self, args: &[&str], max: usize) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        self.run_json_owned_capped(&args, max)
    }

    fn run_json_owned<T>(&self, args: &[String]) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.run_json_owned_capped(args, MAX_OUTPUT)
    }

    fn run_json_owned_capped<T>(&self, args: &[String], max: usize) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let output = self.command(args).output().context("run mb")?;
        if !output.status.success() || output.stdout.len() > max {
            anyhow::bail!(
                "mb {} failed",
                args.first().map(String::as_str).unwrap_or("call")
            );
        }
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "parse mb {} json",
                args.first().map(String::as_str).unwrap_or("call")
            )
        })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args).arg("--json");
        if let Some(project) = &self.project {
            command.arg("--project").arg(project);
        }
        if let Some(url) = &self.url {
            command.arg("--url").arg(url);
        }
        if let Some(token_file) = &self.token_file {
            command.arg("--token-file").arg(token_file);
        }
        if let Some(actor) = &self.actor {
            command.arg("--actor").arg(actor);
        }
        command
    }
}

/// Host-granted Marbles snapshot used by search and mention autocomplete.
#[derive(Debug, Clone)]
pub struct MarblesIndex {
    pub store_id: String,
    pub company: String,
    pub issues: Vec<Issue>,
}

impl MarblesIndex {
    pub fn new(
        store_id: impl Into<String>,
        company: impl Into<String>,
        issues: Vec<Issue>,
    ) -> Self {
        Self {
            store_id: store_id.into(),
            company: company.into(),
            issues,
        }
    }

    pub fn canonical_ref(&self, issue: &Issue) -> String {
        format!(
            "marbles://{}/{}/{}/{}",
            self.store_id, self.company, issue.project, issue.id
        )
    }

    pub fn search(&self, text: &str, limit: usize) -> Vec<(Issue, u32)> {
        let needle = text.to_ascii_lowercase();
        let mut hits = self
            .issues
            .iter()
            .filter_map(|issue| {
                let haystack = format!(
                    "{} {} {} {} {} {}",
                    issue.id,
                    issue.title,
                    issue.description,
                    issue.project,
                    issue.assignee.as_deref().unwrap_or_default(),
                    issue.labels.join(" ")
                )
                .to_ascii_lowercase();
                if !haystack.contains(&needle) {
                    return None;
                }
                let mut score = 900_000u32;
                if issue.title.to_ascii_lowercase().contains(&needle) {
                    score += 10_000;
                }
                if issue.id.eq_ignore_ascii_case(text) {
                    score += 20_000;
                }
                Some((issue.clone(), score))
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| left.0.title.cmp(&right.0.title))
        });
        hits.truncate(limit);
        hits
    }

    pub fn from_snapshot(path: impl AsRef<Path>, store_id: &str, company: &str) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref())
            .with_context(|| format!("read {}", path.as_ref().display()))?;
        let issues: Vec<Issue> =
            serde_json::from_slice(&bytes).context("parse Marbles snapshot")?;
        Ok(Self::new(store_id, company, issues))
    }
}

/// The argv for each transition, kept as plain functions so the contract with the
/// CLI can be asserted without running it.
fn review_args(id: &str, pr: Option<&str>, commit: Option<&str>) -> Vec<String> {
    let mut args = vec!["review".to_string(), id.to_string()];
    for (flag, value) in [("--pr", pr), ("--commit", commit)] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
    }
    args
}

fn claim_args(id: &str, actor: Option<&str>) -> Vec<String> {
    let mut args = vec!["claim".to_string(), id.to_string()];
    if let Some(actor) = actor.filter(|actor| !actor.is_empty()) {
        args.push("--as".to_string());
        args.push(actor.to_string());
    }
    args
}

fn touch_args(id: &str, actor: Option<&str>) -> Vec<String> {
    let mut args = vec!["touch".to_string(), id.to_string()];
    if let Some(actor) = actor.filter(|actor| !actor.is_empty()) {
        args.push("--as".to_string());
        args.push(actor.to_string());
    }
    args
}

fn release_args(id: &str, actor: Option<&str>) -> Vec<String> {
    let mut args = vec!["release".to_string(), id.to_string()];
    if let Some(actor) = actor.filter(|actor| !actor.is_empty()) {
        args.push("--as".to_string());
        args.push(actor.to_string());
    }
    args
}

fn close_args(id: &str, delivery: Delivery<'_>) -> Vec<String> {
    let mut args = vec!["close".to_string(), id.to_string()];
    match delivery {
        Delivery::Merged { pr, commit } => {
            for (flag, value) in [("--pr", pr), ("--commit", commit)] {
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    args.push(flag.to_string());
                    args.push(value.to_string());
                }
            }
        }
        Delivery::Nothing { reason } => {
            args.push("--ack".to_string());
            args.push(reason.to_string());
        }
    }
    args
}

fn update_args(id: &str, edit: Edit<'_>) -> Vec<String> {
    let mut args = vec!["update".to_string(), id.to_string()];
    for (flag, value) in [
        ("--title", edit.title),
        ("--status", edit.status),
        ("--assignee", edit.assignee),
        ("--append-notes", edit.note),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
    }
    if let Some(priority) = edit.priority {
        args.push("--priority".to_string());
        args.push(priority.to_string());
    }
    args
}

fn portfolio_args(folder: Option<&str>, days: i64, stale_days: i64) -> Vec<String> {
    let mut args = vec!["portfolio".to_string()];
    if let Some(folder) = folder.filter(|folder| !folder.is_empty()) {
        args.push("--folder".to_string());
        args.push(folder.to_string());
    }
    args.push("--days".to_string());
    args.push(days.to_string());
    args.push("--stale-days".to_string());
    args.push(stale_days.to_string());
    args
}

fn burndown_args(scope: Burndown<'_>) -> Vec<String> {
    let (flag, value) = match scope {
        Burndown::Project(value) => ("--project", value),
        Burndown::Goal(value) => ("--goal", value),
        Burndown::Epic(value) => ("--epic", value),
    };
    vec![
        "burndown".to_string(),
        flag.to_string(),
        value.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_ref_is_store_company_project_issue() {
        let index = MarblesIndex::new("store", "fpl", Vec::new());
        let issue = Issue {
            id: "ACME-42".into(),
            project: "acme".into(),
            ..Issue::default()
        };
        assert_eq!(
            index.canonical_ref(&issue),
            "marbles://store/fpl/acme/ACME-42"
        );
    }

    #[test]
    fn search_matches_title_and_labels() {
        let index = MarblesIndex::new(
            "store",
            "fpl",
            vec![Issue {
                id: "ACME-42".into(),
                title: "Fix SSO".into(),
                labels: vec!["auth".into()],
                project: "acme".into(),
                ..Issue::default()
            }],
        );
        assert_eq!(index.search("sso", 10).len(), 1);
        assert_eq!(index.search("auth", 10).len(), 1);
        assert!(index.search("missing", 10).is_empty());
    }

    #[cfg(unix)]
    fn fake_mb(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("mb");
        let script = r#"#!/bin/sh
case "$1" in
  list|ready) echo '[{"id":"ACME-42","title":"Fix SSO","status":"open","project":"acme","updated_at":1700000000}]' ;;
  show) echo '[{"id":"ACME-42","title":"Fix SSO","status":"open","project":"acme","updated_at":1700000000}]' ;;
  create) echo '{"id":"ACME-43"}' ;;
  claim) echo '{"id":"ACME-42","assignee":"avery","actor_kind":"human"}' ;;
  review) echo '{"id":"ACME-42","title":"Fix SSO","status":"review","project":"acme"}' ;;
  update) echo '{"id":"ACME-42","title":"Fix SSO","status":"in_progress","project":"acme","assignee":"pm-rakazo"}' ;;
  touch|release) echo '{"id":"ACME-42","lease_expires_at":1700000900}' ;;
  portfolio) echo '{"projects":[{"project":"acme","wip":2,"review":1}]}' ;;
  burndown) echo '{"series":[[1700000000,3]]}' ;;
  close) echo '{"id":"ACME-42","title":"Fix SSO","status":"closed","project":"acme"}' ;;
  *) echo '{}' ;;
esac
"#;
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn cli_adapter_uses_the_json_contract() {
        let dir = tempfile::tempdir().unwrap();
        let client = MarblesClient::new(fake_mb(dir.path()))
            .with_project("acme")
            .with_actor("avery");
        assert_eq!(client.list().unwrap()[0].id, "ACME-42");
        assert_eq!(client.show("ACME-42").unwrap().title, "Fix SSO");
        assert_eq!(client.create("Fix SSO", "details").unwrap(), "ACME-43");
        assert_eq!(client.claim("ACME-42").unwrap()["assignee"], "avery");
        assert_eq!(client.close("ACME-42", "done").unwrap().status, "closed");
    }

    #[test]
    fn the_lifecycle_argv_matches_the_cli_contract() {
        assert_eq!(
            review_args("ACME-42", Some("https://github.com/o/r/pull/7"), None),
            vec!["review", "ACME-42", "--pr", "https://github.com/o/r/pull/7"]
        );
        assert_eq!(
            close_args(
                "ACME-42",
                Delivery::Merged {
                    pr: Some("https://github.com/o/r/pull/7"),
                    commit: Some("abc123"),
                }
            ),
            vec![
                "close",
                "ACME-42",
                "--pr",
                "https://github.com/o/r/pull/7",
                "--commit",
                "abc123",
            ]
        );
        assert_eq!(
            close_args("ACME-42", Delivery::Nothing { reason: "no delivery expected" }),
            vec!["close", "ACME-42", "--ack", "no delivery expected"]
        );
    }

    #[test]
    fn handing_work_to_someone_else_is_a_claim_as_them() {
        assert_eq!(
            claim_args("ACME-42", Some("pm-rakazo")),
            vec!["claim", "ACME-42", "--as", "pm-rakazo"]
        );
        assert_eq!(claim_args("ACME-42", None), vec!["claim", "ACME-42"]);
    }

    #[test]
    fn a_claim_is_held_by_a_session_not_a_name() {
        assert_eq!(touch_args("ACME-42", Some("sess-9")), vec!["touch", "ACME-42", "--as", "sess-9"]);
        assert_eq!(release_args("ACME-42", Some("sess-9")), vec!["release", "ACME-42", "--as", "sess-9"]);
        assert_eq!(touch_args("ACME-42", None), vec!["touch", "ACME-42"]);
    }

    #[test]
    fn an_empty_flag_is_left_out_rather_than_sent_blank() {
        assert_eq!(review_args("ACME-42", Some(""), None), vec!["review", "ACME-42"]);
        assert_eq!(touch_args("ACME-42", Some("")), vec!["touch", "ACME-42"]);
    }

    #[test]
    fn handing_work_over_is_an_assignment_edit() {
        let args = update_args(
            "ACME-42",
            Edit {
                assignee: Some("pm-rakazo"),
                priority: Some(1),
                note: Some("blocked on the schema decision"),
                ..Edit::default()
            },
        );
        assert_eq!(
            args,
            vec![
                "update",
                "ACME-42",
                "--assignee",
                "pm-rakazo",
                "--append-notes",
                "blocked on the schema decision",
                "--priority",
                "1",
            ]
        );
    }

    #[test]
    fn the_pm_rollups_name_their_scope() {
        assert_eq!(
            portfolio_args(Some("personal"), 14, 7),
            vec!["portfolio", "--folder", "personal", "--days", "14", "--stale-days", "7"]
        );
        assert_eq!(
            burndown_args(Burndown::Epic("epic-1")),
            vec!["burndown", "--epic", "epic-1"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_lifecycle_runs_through_the_cli_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let client = MarblesClient::new(fake_mb(dir.path())).with_project("acme");
        assert_eq!(
            client
                .review("ACME-42", Some("https://github.com/o/r/pull/7"), None)
                .unwrap()
                .status,
            "review"
        );
        assert_eq!(client.touch("ACME-42").unwrap()["lease_expires_at"], 1700000900);
        assert_eq!(
            client
                .update("ACME-42", Edit { assignee: Some("pm-rakazo"), ..Edit::default() })
                .unwrap()
                .assignee
                .as_deref(),
            Some("pm-rakazo")
        );
        assert_eq!(client.portfolio(None, 14, 7).unwrap()["projects"][0]["wip"], 2);
        assert_eq!(client.burndown(Burndown::Project("acme")).unwrap()["series"][0][1], 3);
        assert_eq!(
            client
                .close_with(
                    "ACME-42",
                    Delivery::Merged {
                        pr: Some("https://github.com/o/r/pull/7"),
                        commit: None,
                    }
                )
                .unwrap()
                .status,
            "closed"
        );
    }
}
