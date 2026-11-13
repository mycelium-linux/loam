//! Records-backed Teams, Desktops and share tickets.
//!
//! The model in `soil-desktop` decides what a relation means. This crate is the
//! only place that turns those decisions into records: one book, one entity id
//! per noun, and one generation check per write.
//!
//! ```text
//! team:<id>                 schema soil.desktop/team@1
//! desktop:<id>              schema soil.desktop/desktop@1
//! redemption:<ticket>:<n>   schema soil.desktop/redemption@1
//! ```
//!
//! The book is chosen by the host, never by a Spore or a caller. For a Desktop
//! its records bindings derive their own books through
//! [`DesktopKey::records_book`](soil_desktop::DesktopKey::records_book); this
//! book is the directory that lists the Desktops and Teams themselves.
//!
//! **Redemptions are append-only.** The ticket carries a use budget; the store
//! records each use as its own entity under a deterministic id, so two devices
//! that redeem the same ticket see two records after sync instead of one
//! overwritten counter.
//!
//! One process is the writer for one records root. The generation check catches
//! a stale caller, not two processes racing SQLite; a second writer belongs on
//! its own device, with sync reconciling the ops.

use std::path::Path;
use std::sync::Mutex;

use fungos_records::Store as Records;
use soil_desktop::{
    Access, Desktop, DesktopKey, Error as ModelError, Invalid, Item, Owner, PrincipalId,
    Redemption, Refusal, Role, Scope, Team, Ticket, TicketRequest, schema,
};

/// How many records one scan page asks for. Big enough that a small book is one
/// page, small enough that a corrupted cursor cannot make us materialise a book.
const PAGE: usize = 512;

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Invalid(Invalid),
    NotFound,
    AlreadyExists,
    /// A write named a generation the stored manifest is not at.
    Conflict {
        expected: u64,
        actual: u64,
    },
    Store(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::Invalid(invalid) => invalid.fmt(formatter),
            Self::NotFound => formatter.write_str("not found"),
            Self::AlreadyExists => formatter.write_str("already exists"),
            Self::Conflict { expected, actual } => write!(
                formatter,
                "generation conflict: expected {expected}, stored {actual}"
            ),
            Self::Store(error) => write!(formatter, "records store: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

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

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Refused(left), Self::Refused(right)) => left == right,
            (Self::Invalid(left), Self::Invalid(right)) => left == right,
            (Self::NotFound, Self::NotFound) => true,
            (Self::AlreadyExists, Self::AlreadyExists) => true,
            (
                Self::Conflict {
                    expected: left_expected,
                    actual: left_actual,
                },
                Self::Conflict {
                    expected: right_expected,
                    actual: right_actual,
                },
            ) => left_expected == right_expected && left_actual == right_actual,
            (Self::Store(left), Self::Store(right)) => left.to_string() == right.to_string(),
            _ => false,
        }
    }
}

impl Eq for Error {}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Store(error.into())
    }
}

impl From<schema::Error> for Error {
    fn from(error: schema::Error) -> Self {
        match error {
            schema::Error::Invalid(invalid) => Self::Invalid(invalid),
            schema::Error::Json(error) => Self::Store(error.into()),
        }
    }
}

/// One ticket use, as returned to the redeemer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketGrant {
    pub ticket: String,
    pub item: Item,
    pub role: Role,
}

/// The records-backed desktop directory.
pub struct DesktopStore {
    records: Mutex<Records>,
    book: String,
}

impl DesktopStore {
    /// Open (or create) the book that holds Teams, Desktops and redemptions.
    pub fn open(root: impl AsRef<Path>, book: impl Into<String>) -> Result<Self, Error> {
        let root = root.as_ref();
        let book = book.into();
        if book.trim().is_empty() {
            return Err(Invalid::new("book", "expected a records book path").into());
        }
        let records = Records::open(root, &root.join("device_id"))?;
        Ok(Self {
            records: Mutex::new(records),
            book,
        })
    }

    #[must_use]
    pub fn book(&self) -> &str {
        &self.book
    }

    /// The derived address for one Desktop.
    pub fn key(&self, desktop_id: &str) -> Result<DesktopKey, Error> {
        Ok(DesktopKey::parse(desktop_id)?)
    }

    pub fn team(&self, id: &str) -> Result<Option<Team>, Error> {
        let records = self.records.lock().expect("records store poisoned");
        read_team(&records, &self.book, id)
    }

    pub fn teams(&self) -> Result<Vec<Team>, Error> {
        let records = self.records.lock().expect("records store poisoned");
        list::<Team>(&records, &self.book, "team:", schema::TEAM)
    }

    pub fn desktop(&self, id: &str) -> Result<Option<Desktop>, Error> {
        let records = self.records.lock().expect("records store poisoned");
        read_desktop(&records, &self.book, id)
    }

    pub fn desktops(&self) -> Result<Vec<Desktop>, Error> {
        let records = self.records.lock().expect("records store poisoned");
        list::<Desktop>(&records, &self.book, "desktop:", schema::DESKTOP)
    }

    pub fn redemptions(&self) -> Result<Vec<Redemption>, Error> {
        let records = self.records.lock().expect("records store poisoned");
        list::<Redemption>(&records, &self.book, "redemption:", schema::REDEMPTION)
    }

    /// Create a team owned by `actor`.
    pub fn create_team(&self, actor: &PrincipalId, id: &str, name: &str) -> Result<Team, Error> {
        let team = Team::new(id, name, actor.clone());
        team.validate()?;
        self.put_team(&team, None)
    }

    /// Write a team, creating it when `expected` is `None`.
    pub fn put_team(&self, team: &Team, expected: Option<u64>) -> Result<Team, Error> {
        team.validate()?;
        let mut records = self.records.lock().expect("records store poisoned");
        let current = read_team(&records, &self.book, team.id.as_str())?;
        check_generation(current.as_ref().map(|team| team.generation), expected)?;
        // A whole-manifest write is still a new generation; the writer should
        // not have to remember to bump a number the store can derive.
        let mut next = team.clone();
        if expected.is_some() {
            next.generation = current.map_or(0, |team| team.generation).saturating_add(1);
        }
        write(
            &mut records,
            &self.book,
            &team_key(&next),
            schema::TEAM,
            next.to_body()?,
        )?;
        Ok(next)
    }

    /// Add, change or remove one team member. Owners only.
    pub fn set_team_member(
        &self,
        actor: &PrincipalId,
        team_id: &str,
        principal: &PrincipalId,
        role: Option<Role>,
        now_ms: i64,
    ) -> Result<Team, Error> {
        let mut records = self.records.lock().expect("records store poisoned");
        let mut team = read_team(&records, &self.book, team_id)?.ok_or(Error::NotFound)?;
        team.set_member(actor, principal, role, now_ms)?;
        write(
            &mut records,
            &self.book,
            &team_key(&team),
            schema::TEAM,
            team.to_body()?,
        )?;
        Ok(team)
    }

    /// Create a Desktop. The actor must be the owner, or an owner of the owning
    /// team; a Desktop cannot be made on somebody else's behalf.
    pub fn create_desktop(
        &self,
        actor: &PrincipalId,
        id: &str,
        name: &str,
        scope: Scope,
        owner: Owner,
        now_ms: i64,
    ) -> Result<Desktop, Error> {
        let desktop = Desktop {
            id: id.to_owned(),
            name: name.to_owned(),
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
            return Err(
                Invalid::new("desktop.scope", "the scope and owner contradict each other").into(),
            );
        }
        let mut records = self.records.lock().expect("records store poisoned");
        authorize_owner(&records, &self.book, actor, &desktop.owner, now_ms)?;
        if read_desktop(&records, &self.book, id)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        write(
            &mut records,
            &self.book,
            &desktop_key(&desktop),
            schema::DESKTOP,
            desktop.to_body()?,
        )?;
        Ok(desktop)
    }

    /// Replace a Desktop manifest, guarded by its generation.
    ///
    /// Any owner may write; a member with [`Role::Writer`] may not rewrite the
    /// membership that grants them the workspace.
    pub fn put_desktop(
        &self,
        actor: &PrincipalId,
        desktop: &Desktop,
        expected: u64,
        now_ms: i64,
    ) -> Result<Desktop, Error> {
        desktop.validate()?;
        if !desktop.contradictions().is_empty() {
            return Err(
                Invalid::new("desktop.scope", "the scope and owner contradict each other").into(),
            );
        }
        let mut records = self.records.lock().expect("records store poisoned");
        let Some(current) = read_desktop(&records, &self.book, &desktop.id)? else {
            return Err(Error::NotFound);
        };
        let teams = list::<Team>(&records, &self.book, "team:", schema::TEAM)?;
        if !current.is_owner(&teams, actor, now_ms) {
            return Err(Refusal::NotGranted.into());
        }
        check_generation(Some(current.generation), Some(expected))?;
        let mut next = desktop.clone();
        next.generation = current.generation.saturating_add(1);
        write(
            &mut records,
            &self.book,
            &desktop_key(&next),
            schema::DESKTOP,
            next.to_body()?,
        )?;
        Ok(next)
    }

    /// Add, change or remove one desktop member. Owners only.
    pub fn set_desktop_member(
        &self,
        actor: &PrincipalId,
        desktop_id: &str,
        principal: &PrincipalId,
        role: Option<Role>,
        now_ms: i64,
    ) -> Result<Desktop, Error> {
        let mut records = self.records.lock().expect("records store poisoned");
        let mut desktop = read_desktop(&records, &self.book, desktop_id)?.ok_or(Error::NotFound)?;
        let teams = list::<Team>(&records, &self.book, "team:", schema::TEAM)?;
        let redemptions =
            list::<Redemption>(&records, &self.book, "redemption:", schema::REDEMPTION)?;
        let access = Access::new(&teams, &redemptions, now_ms);
        desktop.set_member(&access, actor, principal, role)?;
        write(
            &mut records,
            &self.book,
            &desktop_key(&desktop),
            schema::DESKTOP,
            desktop.to_body()?,
        )?;
        Ok(desktop)
    }

    /// Mint a one-item ticket for one principal, without making them a member.
    ///
    /// `request.id` is minted by the caller when it needs a stable handle; pass
    /// `None` to let the store mint a random one.
    pub fn issue_ticket(
        &self,
        actor: &PrincipalId,
        desktop_id: &str,
        mut request: TicketRequest,
        now_ms: i64,
    ) -> Result<Ticket, Error> {
        if request.id.is_empty() {
            request.id = minted_id();
        }
        let mut records = self.records.lock().expect("records store poisoned");
        let mut desktop = read_desktop(&records, &self.book, desktop_id)?.ok_or(Error::NotFound)?;
        let teams = list::<Team>(&records, &self.book, "team:", schema::TEAM)?;
        let redemptions =
            list::<Redemption>(&records, &self.book, "redemption:", schema::REDEMPTION)?;
        let access = Access::new(&teams, &redemptions, now_ms);
        let ticket = desktop.issue_ticket(&access, actor, request)?;
        write(
            &mut records,
            &self.book,
            &desktop_key(&desktop),
            schema::DESKTOP,
            desktop.to_body()?,
        )?;
        Ok(ticket)
    }

    /// Redeem one use of a ticket, recording the use.
    ///
    /// The principal must be the ticket's grantee. A ticket id that leaks is
    /// not enough on its own, which is why a ticket is an addressed offer and
    /// not a bearer token.
    pub fn redeem_ticket(
        &self,
        actor: &PrincipalId,
        desktop_id: &str,
        ticket_id: &str,
        now_ms: i64,
    ) -> Result<TicketGrant, Error> {
        let mut records = self.records.lock().expect("records store poisoned");
        let desktop = read_desktop(&records, &self.book, desktop_id)?.ok_or(Error::NotFound)?;
        let ticket = desktop
            .tickets
            .iter()
            .find(|ticket| ticket.id == ticket_id)
            .cloned()
            .ok_or(Error::NotFound)?;
        let redemptions =
            list::<Redemption>(&records, &self.book, "redemption:", schema::REDEMPTION)?;
        let used = redemptions
            .iter()
            .filter(|redemption| redemption.ticket == ticket.id)
            .count();
        let used = u32::try_from(used).unwrap_or(u32::MAX);
        ticket.redeem(actor, used, now_ms)?;

        let redemption = Redemption {
            ticket: ticket.id.clone(),
            principal: actor.clone(),
            at_ms: now_ms,
        };
        let key = format!("redemption:{}:{used}", redemption.ticket);
        write(
            &mut records,
            &self.book,
            &key,
            schema::REDEMPTION,
            redemption.to_body()?,
        )?;
        Ok(TicketGrant {
            ticket: ticket.id,
            item: ticket.item,
            role: ticket.role,
        })
    }

    /// The strongest role this principal holds for one item, now.
    pub fn access(
        &self,
        actor: &PrincipalId,
        desktop_id: &str,
        item: &Item,
        now_ms: i64,
    ) -> Result<Role, Error> {
        let records = self.records.lock().expect("records store poisoned");
        let desktop = read_desktop(&records, &self.book, desktop_id)?.ok_or(Error::NotFound)?;
        let teams = list::<Team>(&records, &self.book, "team:", schema::TEAM)?;
        let redemptions =
            list::<Redemption>(&records, &self.book, "redemption:", schema::REDEMPTION)?;
        let access = Access::new(&teams, &redemptions, now_ms);
        Ok(desktop.access(&access, actor, item)?)
    }
}

fn team_key(team: &Team) -> String {
    format!("team:{}", team.id)
}

fn desktop_key(desktop: &Desktop) -> String {
    format!("desktop:{}", desktop.id)
}

/// A random handle, not a secret. 128 bits is enough that guessing one is not a
/// way to reach a ticket; the grantee check is what makes a leaked id useless.
fn minted_id() -> String {
    format!("ticket-{}", uuid::Uuid::new_v4().simple())
}

fn check_generation(actual: Option<u64>, expected: Option<u64>) -> Result<(), Error> {
    match (actual, expected) {
        // Create-if-absent, but the record already exists.
        (Some(actual), None) => Err(Error::Conflict {
            expected: 0,
            actual,
        }),
        // Update a record that is gone.
        (None, Some(_)) => Err(Error::NotFound),
        (Some(actual), Some(expected)) if actual != expected => {
            Err(Error::Conflict { expected, actual })
        }
        _ => Ok(()),
    }
}

fn read_team(records: &Records, book: &str, id: &str) -> Result<Option<Team>, Error> {
    let Some(body) = read(records, book, &format!("team:{id}"), schema::TEAM)? else {
        return Ok(None);
    };
    Ok(Some(Team::from_body(&body)?))
}

fn read_desktop(records: &Records, book: &str, id: &str) -> Result<Option<Desktop>, Error> {
    let Some(body) = read(records, book, &format!("desktop:{id}"), schema::DESKTOP)? else {
        return Ok(None);
    };
    Ok(Some(Desktop::from_body(&body)?))
}

fn read(
    records: &Records,
    book: &str,
    key: &str,
    schema_id: &str,
) -> Result<Option<Vec<u8>>, Error> {
    let Some(record) = records.get_optional(book, key)? else {
        return Ok(None);
    };
    if record.deleted || record.schema_id != schema_id {
        return Ok(None);
    }
    Ok(Some(serde_json::to_vec(&record.fields)?))
}

fn list<T>(records: &Records, book: &str, prefix: &str, schema_id: &str) -> Result<Vec<T>, Error>
where
    T: serde::de::DeserializeOwned,
{
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = records.scan_records(book, cursor.as_deref(), PAGE)?;
        let Some(last) = page.last() else {
            break;
        };
        cursor = Some(last.entity_id.clone());
        for record in page {
            if record.deleted || !record.entity_id.starts_with(prefix) {
                continue;
            }
            if record.schema_id != schema_id {
                continue;
            }
            let body = serde_json::to_vec(&record.fields)?;
            out.push(serde_json::from_slice(&body)?);
        }
    }
    Ok(out)
}

fn write(
    records: &mut Records,
    book: &str,
    key: &str,
    schema_id: &str,
    body: Vec<u8>,
) -> Result<(), Error> {
    let fields: serde_json::Value = serde_json::from_slice(&body)?;
    // A local write; sync transports the resulting op. The caller holds the
    // mutex, so the read-check-write sequence above is one critical section.
    records.put_record(book, key, schema_id, 1, fields)?;
    Ok(())
}

fn authorize_owner(
    records: &Records,
    book: &str,
    actor: &PrincipalId,
    owner: &Owner,
    now_ms: i64,
) -> Result<(), Error> {
    match owner {
        Owner::Principal { principal } if principal == actor => Ok(()),
        Owner::Principal { .. } => Err(Refusal::NotGranted.into()),
        Owner::Team { team } => {
            let Some(team) = read_team(records, book, team.as_str())? else {
                return Err(Error::NotFound);
            };
            match team.relation(actor, now_ms) {
                Ok(Role::Owner) => Ok(()),
                Ok(_) => Err(Refusal::NotGranted.into()),
                Err(refusal) => Err(refusal.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soil_desktop::TicketRequest;

    fn principal(id: &str) -> PrincipalId {
        PrincipalId::from(id)
    }

    const NOW: i64 = 1_700_000_000_000;

    fn store() -> (tempfile::TempDir, DesktopStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = DesktopStore::open(dir.path(), "personal/avery/desktops").unwrap();
        (dir, store)
    }

    #[test]
    fn a_team_is_durable_across_reopen() {
        let (dir, store) = store();
        store
            .create_team(&principal("avery"), "fpl", "FPL")
            .unwrap();
        store
            .set_team_member(
                &principal("avery"),
                "fpl",
                &principal("kwame"),
                Some(Role::Writer),
                NOW,
            )
            .unwrap();
        drop(store);

        let reopened = DesktopStore::open(dir.path(), "personal/avery/desktops").unwrap();
        let team = reopened.team("fpl").unwrap().unwrap();
        assert_eq!(team.relation(&principal("kwame"), NOW), Ok(Role::Writer));
        assert_eq!(team.generation, 1);
    }

    #[test]
    fn a_non_owner_cannot_edit_a_team() {
        let (_dir, store) = store();
        store
            .create_team(&principal("avery"), "fpl", "FPL")
            .unwrap();
        store
            .set_team_member(
                &principal("avery"),
                "fpl",
                &principal("kwame"),
                Some(Role::Writer),
                NOW,
            )
            .unwrap();

        assert_eq!(
            store.set_team_member(
                &principal("kwame"),
                "fpl",
                &principal("dana"),
                Some(Role::Reader),
                NOW
            ),
            Err(Error::Refused(Refusal::NotGranted))
        );
    }

    #[test]
    fn a_desktop_owned_by_a_team_is_administered_by_its_owner() {
        let (_dir, store) = store();
        store
            .create_team(&principal("avery"), "fpl", "FPL")
            .unwrap();
        store
            .set_team_member(
                &principal("avery"),
                "fpl",
                &principal("kwame"),
                Some(Role::Writer),
                NOW,
            )
            .unwrap();

        // A team member cannot create a team-owned desktop on the team's behalf.
        assert_eq!(
            store.create_desktop(
                &principal("kwame"),
                "fpl-work",
                "FPL work",
                Scope::Team,
                Owner::team("fpl"),
                NOW
            ),
            Err(Error::Refused(Refusal::NotGranted))
        );
        // The team owner can, and the member inherits the workspace.
        let desktop = store
            .create_desktop(
                &principal("avery"),
                "fpl-work",
                "FPL work",
                Scope::Team,
                Owner::team("fpl"),
                NOW,
            )
            .unwrap();
        assert_eq!(desktop.scope, Scope::Team);
        assert_eq!(
            store.access(
                &principal("kwame"),
                "fpl-work",
                &Item::file("docs/q3.md"),
                NOW
            ),
            Ok(Role::Writer)
        );
    }

    #[test]
    fn a_write_fences_a_stale_generation() {
        let (_dir, store) = store();
        let desktop = store
            .create_desktop(
                &principal("avery"),
                "fpl",
                "FPL",
                Scope::Personal,
                Owner::principal("avery"),
                NOW,
            )
            .unwrap();
        let mut copy = desktop.clone();
        copy.name = "FPL two".to_owned();
        store
            .put_desktop(&principal("avery"), &copy, 0, NOW)
            .unwrap();

        assert_eq!(
            store.put_desktop(&principal("avery"), &desktop, 0, NOW),
            Err(Error::Conflict {
                expected: 0,
                actual: 1
            })
        );
    }

    #[test]
    fn a_ticket_shares_one_item_without_membership() {
        let (_dir, store) = store();
        store
            .create_desktop(
                &principal("avery"),
                "fpl",
                "FPL",
                Scope::Personal,
                Owner::principal("avery"),
                NOW,
            )
            .unwrap();
        let item = Item::file("docs/q3.md");
        let ticket = store
            .issue_ticket(
                &principal("avery"),
                "fpl",
                TicketRequest {
                    id: String::new(),
                    item: item.clone(),
                    grantee: principal("dana"),
                    role: Role::Reader,
                    expires_at_ms: Some(NOW + 60_000),
                    max_uses: 1,
                },
                NOW,
            )
            .unwrap();

        // Minted, not yet claimed: still not access.
        assert_eq!(
            store.access(&principal("dana"), "fpl", &item, NOW),
            Err(Error::Refused(Refusal::NotGranted))
        );
        // A different principal cannot claim it even with the id.
        assert_eq!(
            store.redeem_ticket(&principal("mallory"), "fpl", &ticket.id, NOW),
            Err(Error::Refused(Refusal::NotGranted))
        );
        assert_eq!(
            store.redeem_ticket(&principal("dana"), "fpl", &ticket.id, NOW),
            Ok(TicketGrant {
                ticket: ticket.id.clone(),
                item: item.clone(),
                role: Role::Reader
            })
        );
        assert_eq!(
            store.access(&principal("dana"), "fpl", &item, NOW),
            Ok(Role::Reader)
        );
        // Still not a member.
        assert_eq!(
            store.access(&principal("dana"), "fpl", &Item::file("notes/plan.md"), NOW),
            Err(Error::Refused(Refusal::NotAMember))
        );
        // And the use budget is spent.
        assert_eq!(
            store.redeem_ticket(&principal("dana"), "fpl", &ticket.id, NOW),
            Err(Error::Refused(Refusal::Exhausted))
        );
    }

    #[test]
    fn redemptions_are_records_not_a_counter() {
        let (dir, store) = store();
        store
            .create_desktop(
                &principal("avery"),
                "fpl",
                "FPL",
                Scope::Personal,
                Owner::principal("avery"),
                NOW,
            )
            .unwrap();
        let ticket = store
            .issue_ticket(
                &principal("avery"),
                "fpl",
                TicketRequest {
                    id: String::new(),
                    item: Item::record("contacts", "ada"),
                    grantee: principal("dana"),
                    role: Role::Reader,
                    expires_at_ms: None,
                    max_uses: 2,
                },
                NOW,
            )
            .unwrap();
        store
            .redeem_ticket(&principal("dana"), "fpl", &ticket.id, NOW)
            .unwrap();
        store
            .redeem_ticket(&principal("dana"), "fpl", &ticket.id, NOW)
            .unwrap();
        assert_eq!(store.redemptions().unwrap().len(), 2);
        drop(store);

        let reopened = DesktopStore::open(dir.path(), "personal/avery/desktops").unwrap();
        assert_eq!(reopened.redemptions().unwrap().len(), 2);
    }

    #[test]
    fn a_desktop_cannot_be_created_for_somebody_else() {
        let (_dir, store) = store();
        assert_eq!(
            store.create_desktop(
                &principal("mallory"),
                "fpl",
                "FPL",
                Scope::Personal,
                Owner::principal("avery"),
                NOW
            ),
            Err(Error::Refused(Refusal::NotGranted))
        );
    }

    #[test]
    fn a_manifest_that_violates_its_schema_never_reaches_the_store() {
        let (_dir, store) = store();
        assert!(matches!(
            store.create_desktop(
                &principal("avery"),
                "../fpl",
                "FPL",
                Scope::Personal,
                Owner::principal("avery"),
                NOW
            ),
            Err(Error::Invalid(_))
        ));
        assert!(store.desktops().unwrap().is_empty());
    }

    #[test]
    fn the_key_names_the_book_and_the_geode_slot() {
        let (_dir, store) = store();
        let key = store.key("fpl").unwrap();
        assert_eq!(key.records_book("crm").unwrap(), "desktop/fpl/crm");
        assert_eq!(key.geode("records").unwrap(), "geode://desktop/fpl/records");
    }
}
