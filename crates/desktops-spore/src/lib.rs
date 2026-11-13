//! Core state machine for the Desktops Spore.
//!
//! Like `viewer-spore`, this crate contains no records handle, no network, no
//! presentation and no authority. It takes the snapshot the host already made
//! visible and one intent, and returns **one manifest write**. The package in
//! the Spores repository is the thin shell that reads records, mints a ticket
//! id, calls [`apply`], and sends the write through `soil:records`.
//!
//! Two rules make the split worth it:
//!
//! - The **actor comes from the snapshot**, not from the intent. A Lua view
//!   cannot claim to be somebody else by editing an action payload.
//! - A **generation is always named** on an update. A stale window gets a
//!   conflict from the host's compare-and-swap, not a silent overwrite of
//!   somebody else's membership edit.
//!
//! Redemption is deliberately absent. A guest redeems a ticket through a
//! narrow host path; it is not a management intent and must not require write
//! access to the manifest that grants it.

use serde::{Deserialize, Serialize};

use soil_desktop::{
    Access, Desktop, Error as ModelError, Invalid, Item, Owner, PrincipalId, Refusal, Role, Scope,
    Team, TicketRequest, schema,
};

/// Everything the Spore is allowed to see, already filtered by the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Presentation revision: the sum of the manifests' generations. A view can
    /// skip an unchanged redraw without re-deriving it.
    pub revision: u64,
    /// The host-attested principal. Never supplied by an intent.
    pub actor: PrincipalId,
    #[serde(default)]
    pub teams: Vec<Team>,
    #[serde(default)]
    pub desktops: Vec<Desktop>,
}

impl Snapshot {
    #[must_use]
    pub fn new(actor: PrincipalId, teams: Vec<Team>, desktops: Vec<Desktop>) -> Self {
        let revision = teams
            .iter()
            .map(|team| team.generation)
            .chain(desktops.iter().map(|desktop| desktop.generation))
            .fold(0u64, u64::saturating_add);
        Self {
            revision,
            actor,
            teams,
            desktops,
        }
    }

    /// The snapshot a Canvas view renders. No host paths, no book names.
    pub fn to_json(&self) -> Result<serde_json::Value, Error> {
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn team(&self, id: &str) -> Result<&Team, Error> {
        self.teams
            .iter()
            .find(|team| team.id.as_str() == id)
            .ok_or(Error::NotFound("team"))
    }

    pub fn desktop(&self, id: &str) -> Result<&Desktop, Error> {
        self.desktops
            .iter()
            .find(|desktop| desktop.id == id)
            .ok_or(Error::NotFound("desktop"))
    }
}

/// One management intent, as the module receives it.
///
/// The wire form is `run(["team.create", "{\"id\":\"fpl\",\"name\":\"FPL\"}"])`;
/// [`Intent::parse`] turns that into this. Names are display fields; ids are
/// validated by the model, not by the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Intent {
    TeamCreate {
        id: String,
        name: String,
    },
    TeamMemberSet {
        team: String,
        principal: PrincipalId,
        #[serde(default)]
        role: Option<Role>,
        #[serde(default)]
        remove: bool,
    },
    DesktopCreate {
        id: String,
        name: String,
        scope: Scope,
        #[serde(default)]
        team: Option<String>,
    },
    DesktopMemberSet {
        desktop: String,
        principal: PrincipalId,
        #[serde(default)]
        role: Option<Role>,
        #[serde(default)]
        remove: bool,
    },
    TicketIssue {
        desktop: String,
        item: String,
        principal: PrincipalId,
        role: Role,
        #[serde(default)]
        ttl_secs: Option<i64>,
        #[serde(default = "one_use")]
        uses: u32,
    },
    TicketRevoke {
        desktop: String,
        ticket: String,
    },
}

fn one_use() -> u32 {
    1
}

impl Intent {
    /// Parse the module's command surface: an action name and one JSON object.
    pub fn parse(args: &[String]) -> Result<Self, Error> {
        let Some(action) = args.first() else {
            return Err(Invalid::new("intent", "expected <action> [json]").into());
        };
        if args.len() > 2 {
            return Err(Invalid::new("intent", "expected <action> [json]").into());
        }
        let mut payload: serde_json::Value = match args.get(1) {
            Some(json) if !json.trim().is_empty() => serde_json::from_str(json)?,
            _ => serde_json::Value::Object(serde_json::Map::new()),
        };
        let Some(object) = payload.as_object_mut() else {
            return Err(Invalid::new("intent", "the payload must be a JSON object").into());
        };
        object.insert(
            "action".to_owned(),
            serde_json::Value::String(action.clone()),
        );
        Ok(serde_json::from_value(payload)?)
    }
}

/// One manifest write, ready for `soil:records.compare-and-swap`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// `team:<id>` or `desktop:<id>`.
    pub key: String,
    pub schema: &'static str,
    /// `None` creates; `Some` requires the stored manifest at that generation.
    pub expected_generation: Option<u64>,
    pub body: serde_json::Value,
}

impl Write {
    pub fn body_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(&self.body)?)
    }
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Invalid(Invalid),
    NotFound(&'static str),
    Exists(&'static str),
    Json(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::Invalid(invalid) => invalid.fmt(formatter),
            Self::NotFound(kind) => write!(formatter, "{kind} not found"),
            Self::Exists(kind) => write!(formatter, "{kind} already exists"),
            Self::Json(error) => write!(formatter, "intent body: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<Invalid> for Error {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<ModelError> for Error {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::Refused(refusal) => Self::Refused(refusal),
            ModelError::Invalid(invalid) => Self::Invalid(invalid),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Turn one intent into the single write the host should attempt.
///
/// `ticket_id` is required for [`Intent::TicketIssue`]: the handle must come
/// from host entropy, not from a Lua view. `now_ms` is the host clock; the
/// module never reads one.
pub fn apply(
    snapshot: &Snapshot,
    intent: Intent,
    now_ms: i64,
    ticket_id: Option<&str>,
) -> Result<Write, Error> {
    match intent {
        Intent::TeamCreate { id, name } => {
            if snapshot.teams.iter().any(|team| team.id.as_str() == id) {
                return Err(Error::Exists("team"));
            }
            let team = Team::new(id, name, snapshot.actor.clone());
            team.validate()?;
            write_team(&team, None)
        }
        Intent::TeamMemberSet {
            team,
            principal,
            role,
            remove,
        } => {
            let current = snapshot.team(&team)?;
            let expected = current.generation;
            let mut next = current.clone();
            next.set_member(
                &snapshot.actor,
                &principal,
                member_role(role, remove)?,
                now_ms,
            )?;
            write_team(&next, Some(expected))
        }
        Intent::DesktopCreate {
            id,
            name,
            scope,
            team,
        } => {
            if snapshot.desktops.iter().any(|desktop| desktop.id == id) {
                return Err(Error::Exists("desktop"));
            }
            let owner = match (scope, team) {
                (Scope::Team, Some(team)) => Owner::team(team),
                (Scope::Team, None) => {
                    return Err(Invalid::new("desktop.owner", "a team desktop needs a team").into());
                }
                (_, Some(_)) => {
                    return Err(Invalid::new(
                        "desktop.owner",
                        "a team is only valid with team scope",
                    )
                    .into());
                }
                (_, None) => Owner::principal(snapshot.actor.clone()),
            };
            let desktop = Desktop {
                id,
                name,
                scope,
                owner,
                members: Vec::new(),
                shares: Vec::new(),
                tickets: Vec::new(),
                installs: Vec::new(),
                denied: std::collections::BTreeSet::new(),
                services: Vec::new(),
                records: Vec::new(),
                generation: 0,
            };
            desktop.validate()?;
            if !desktop.contradictions().is_empty() {
                return Err(Invalid::new(
                    "desktop.scope",
                    "the scope and owner contradict each other",
                )
                .into());
            }
            authorize_owner(snapshot, &desktop.owner, now_ms)?;
            write_desktop(&desktop, None)
        }
        Intent::DesktopMemberSet {
            desktop,
            principal,
            role,
            remove,
        } => {
            let current = snapshot.desktop(&desktop)?;
            let expected = current.generation;
            let mut next = current.clone();
            let access = Access::new(&snapshot.teams, &[], now_ms);
            next.set_member(
                &access,
                &snapshot.actor,
                &principal,
                member_role(role, remove)?,
            )?;
            write_desktop(&next, Some(expected))
        }
        Intent::TicketIssue {
            desktop,
            item,
            principal,
            role,
            ttl_secs,
            uses,
        } => {
            let current = snapshot.desktop(&desktop)?;
            let expected = current.generation;
            let id = ticket_id.ok_or(Invalid::new("ticket.id", "the host must mint an id"))?;
            let item = Item::parse(&item)?;
            let expires_at_ms = match ttl_secs {
                Some(seconds) if seconds > 0 => Some(
                    now_ms
                        .checked_add(seconds.saturating_mul(1000))
                        .ok_or(Invalid::new("ticket.expires_at_ms", "overflows the clock"))?,
                ),
                Some(_) => {
                    return Err(Invalid::new("ticket.ttl_secs", "expected a positive value").into());
                }
                None => None,
            };
            let mut next = current.clone();
            let access = Access::new(&snapshot.teams, &[], now_ms);
            next.issue_ticket(
                &access,
                &snapshot.actor,
                TicketRequest {
                    id: id.to_owned(),
                    item,
                    grantee: principal,
                    role,
                    expires_at_ms,
                    max_uses: uses,
                },
            )?;
            write_desktop(&next, Some(expected))
        }
        Intent::TicketRevoke { desktop, ticket } => {
            let current = snapshot.desktop(&desktop)?;
            let expected = current.generation;
            let mut next = current.clone();
            let access = Access::new(&snapshot.teams, &[], now_ms);
            next.revoke_ticket(&access, &snapshot.actor, &ticket)?;
            write_desktop(&next, Some(expected))
        }
    }
}

fn member_role(role: Option<Role>, remove: bool) -> Result<Option<Role>, Error> {
    match (role, remove) {
        (_, true) => Ok(None),
        (Some(role), false) => Ok(Some(role)),
        (None, false) => Err(Invalid::new("member.role", "expected a role or remove=true").into()),
    }
}

fn authorize_owner(snapshot: &Snapshot, owner: &Owner, now_ms: i64) -> Result<(), Error> {
    match owner {
        Owner::Principal { principal } if principal == &snapshot.actor => Ok(()),
        Owner::Principal { .. } => Err(Refusal::NotGranted.into()),
        Owner::Team { team } => {
            let team = snapshot.team(team.as_str())?;
            match team.relation(&snapshot.actor, now_ms) {
                Ok(Role::Owner) => Ok(()),
                Ok(_) => Err(Refusal::NotGranted.into()),
                Err(refusal) => Err(refusal.into()),
            }
        }
    }
}

fn write_team(team: &Team, expected_generation: Option<u64>) -> Result<Write, Error> {
    Ok(Write {
        key: format!("team:{}", team.id),
        schema: schema::TEAM,
        expected_generation,
        body: serde_json::to_value(team)?,
    })
}

fn write_desktop(desktop: &Desktop, expected_generation: Option<u64>) -> Result<Write, Error> {
    Ok(Write {
        key: format!("desktop:{}", desktop.id),
        schema: schema::DESKTOP,
        expected_generation,
        body: serde_json::to_value(desktop)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use soil_desktop::{Membership, Ticket};

    const NOW: i64 = 1_700_000_000_000;

    fn principal(id: &str) -> PrincipalId {
        PrincipalId::from(id)
    }

    fn team() -> Team {
        let mut team = Team::new("fpl", "FPL", "avery");
        team.members
            .push(Membership::new(principal("kwame"), Role::Writer));
        team
    }

    fn desktop() -> Desktop {
        Desktop {
            id: "fpl-work".to_owned(),
            name: "FPL work".to_owned(),
            scope: Scope::Team,
            owner: Owner::team("fpl"),
            members: Vec::new(),
            shares: Vec::new(),
            tickets: Vec::new(),
            installs: Vec::new(),
            denied: std::collections::BTreeSet::new(),
            services: Vec::new(),
            records: Vec::new(),
            generation: 4,
        }
    }

    fn snapshot(actor: &str) -> Snapshot {
        Snapshot::new(principal(actor), vec![team()], vec![desktop()])
    }

    #[test]
    fn the_snapshot_revision_moves_when_a_manifest_does() {
        let snapshot = Snapshot::new(principal("avery"), vec![team()], vec![desktop()]);
        assert_eq!(snapshot.revision, 4);
        assert_eq!(snapshot.to_json().unwrap()["actor"], "avery");
    }

    #[test]
    fn an_intent_comes_off_the_command_surface() {
        let intent = Intent::parse(&[
            "team_member_set".to_owned(),
            r#"{"team":"fpl","principal":"dana","role":"reader"}"#.to_owned(),
        ])
        .unwrap();
        assert_eq!(
            intent,
            Intent::TeamMemberSet {
                team: "fpl".to_owned(),
                principal: principal("dana"),
                role: Some(Role::Reader),
                remove: false
            }
        );
        assert!(Intent::parse(&["team_member_set".to_owned(), "[]".to_owned()]).is_err());
    }

    #[test]
    fn a_team_create_names_the_actor_as_owner() {
        let write = apply(
            &snapshot("avery"),
            Intent::TeamCreate {
                id: "new-team".to_owned(),
                name: "New".to_owned(),
            },
            NOW,
            None,
        )
        .unwrap();
        assert_eq!(write.key, "team:new-team");
        assert_eq!(write.schema, schema::TEAM);
        assert_eq!(write.expected_generation, None);
        assert_eq!(write.body["owner"], "avery");
    }

    #[test]
    fn a_duplicate_team_is_refused_before_the_write() {
        let error = apply(
            &snapshot("avery"),
            Intent::TeamCreate {
                id: "fpl".to_owned(),
                name: "FPL".to_owned(),
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Exists("team")));
    }

    #[test]
    fn a_member_edit_names_the_generation_it_started_from() {
        let write = apply(
            &snapshot("avery"),
            Intent::TeamMemberSet {
                team: "fpl".to_owned(),
                principal: principal("dana"),
                role: Some(Role::Reader),
                remove: false,
            },
            NOW,
            None,
        )
        .unwrap();
        assert_eq!(write.expected_generation, Some(0));
        assert_eq!(write.body["generation"], 1);
    }

    #[test]
    fn a_non_owner_cannot_edit_a_team() {
        let error = apply(
            &snapshot("kwame"),
            Intent::TeamMemberSet {
                team: "fpl".to_owned(),
                principal: principal("dana"),
                role: Some(Role::Reader),
                remove: false,
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Refused(Refusal::NotGranted)));
    }

    #[test]
    fn a_team_member_cannot_create_a_team_owned_desktop_on_the_teams_behalf() {
        let error = apply(
            &snapshot("kwame"),
            Intent::DesktopCreate {
                id: "other".to_owned(),
                name: "Other".to_owned(),
                scope: Scope::Team,
                team: Some("fpl".to_owned()),
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Refused(Refusal::NotGranted)));
    }

    #[test]
    fn the_team_owner_can_create_one() {
        let write = apply(
            &snapshot("avery"),
            Intent::DesktopCreate {
                id: "other".to_owned(),
                name: "Other".to_owned(),
                scope: Scope::Team,
                team: Some("fpl".to_owned()),
            },
            NOW,
            None,
        )
        .unwrap();
        assert_eq!(write.key, "desktop:other");
        assert_eq!(write.body["owner"]["kind"], "team");
    }

    #[test]
    fn a_ticket_issue_needs_a_host_minted_id() {
        let intent = Intent::TicketIssue {
            desktop: "fpl-work".to_owned(),
            item: "file:docs/q3.md".to_owned(),
            principal: principal("dana"),
            role: Role::Reader,
            ttl_secs: Some(60),
            uses: 1,
        };
        let error = apply(&snapshot("avery"), intent.clone(), NOW, None).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));

        let write = apply(
            &snapshot("avery"),
            intent,
            NOW,
            Some("ticket-0123456789abcdef"),
        )
        .unwrap();
        assert_eq!(write.expected_generation, Some(4));
        assert_eq!(write.body["generation"], 5);
        assert_eq!(write.body["tickets"][0]["grantee"], "dana");
        assert_eq!(write.body["tickets"][0]["expires_at_ms"], NOW + 60_000);
    }

    #[test]
    fn a_writer_may_mint_a_reader_ticket() {
        let writer = Snapshot::new(principal("kwame"), vec![team()], vec![desktop()]);
        let error = apply(
            &writer,
            Intent::TicketIssue {
                desktop: "fpl-work".to_owned(),
                item: "file:docs/q3.md".to_owned(),
                principal: principal("dana"),
                role: Role::Reader,
                ttl_secs: None,
                uses: 1,
            },
            NOW,
            Some("ticket-0123456789abcdef"),
        );
        // A team Writer is a desktop Writer, so they may mint a reader ticket.
        assert!(write_has_ticket(&error.unwrap()));
    }

    fn write_has_ticket(write: &Write) -> bool {
        write.body["tickets"]
            .as_array()
            .is_some_and(|tickets| tickets.len() == 1)
    }

    #[test]
    fn a_reader_cannot_mint_a_writer_ticket() {
        let mut k = team();
        k.members[0].role = Role::Reader;
        let snapshot = Snapshot::new(principal("kwame"), vec![k], vec![desktop()]);
        let error = apply(
            &snapshot,
            Intent::TicketIssue {
                desktop: "fpl-work".to_owned(),
                item: "file:docs/q3.md".to_owned(),
                principal: principal("dana"),
                role: Role::Writer,
                ttl_secs: None,
                uses: 1,
            },
            NOW,
            Some("ticket-0123456789abcdef"),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Refused(Refusal::NotGranted)));
    }

    #[test]
    fn a_ticket_revoke_by_a_non_owner_is_refused() {
        let mut with_ticket = desktop();
        with_ticket.tickets.push(Ticket {
            id: "ticket-0123456789abcdef".to_owned(),
            item: Item::file("docs/q3.md"),
            grantee: principal("dana"),
            role: Role::Reader,
            issued_by: principal("avery"),
            issued_at_ms: NOW,
            expires_at_ms: None,
            max_uses: 1,
        });
        let snapshot = Snapshot::new(principal("kwame"), vec![team()], vec![with_ticket]);
        let error = apply(
            &snapshot,
            Intent::TicketRevoke {
                desktop: "fpl-work".to_owned(),
                ticket: "ticket-0123456789abcdef".to_owned(),
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Refused(Refusal::NotGranted)));
    }

    #[test]
    fn an_unknown_manifest_is_not_found_not_a_denial() {
        let error = apply(
            &snapshot("avery"),
            Intent::DesktopMemberSet {
                desktop: "missing".to_owned(),
                principal: principal("dana"),
                role: Some(Role::Reader),
                remove: false,
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::NotFound("desktop")));
    }

    #[test]
    fn a_member_intent_without_a_role_or_remove_is_rejected() {
        let error = apply(
            &snapshot("avery"),
            Intent::DesktopMemberSet {
                desktop: "fpl-work".to_owned(),
                principal: principal("dana"),
                role: None,
                remove: false,
            },
            NOW,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
    }
}
