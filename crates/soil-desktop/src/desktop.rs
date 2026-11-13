//! Desktops: the unit of work, sharing and placement.
//!
//! A Desktop is a scope, not a grant. Membership says who may work in it;
//! [`Share`] and [`Ticket`] say what a non-member may reach. The two are kept
//! apart on purpose: receiving one document must not be a back door into the
//! rest of the tree.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    Capability, Device, Error, Invalid, Membership, Owner, PrincipalId, Refusal, Role, Scope, Team,
    valid_id, valid_name, valid_principal,
};

/// One item that can be shared without sharing the Desktop.
///
/// The token form is the wire and command-line shape, not a storage format:
/// `file:docs/q3.md` and `record:contacts#ada`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    File { path: String },
    Record { collection: String, id: String },
}

impl Item {
    #[must_use]
    pub fn file(path: impl Into<String>) -> Self {
        Self::File { path: path.into() }
    }

    #[must_use]
    pub fn record(collection: impl Into<String>, id: impl Into<String>) -> Self {
        Self::Record {
            collection: collection.into(),
            id: id.into(),
        }
    }

    /// Parse the command-line / link form.
    pub fn parse(token: &str) -> Result<Self, Invalid> {
        let item = if let Some(path) = token.strip_prefix("file:") {
            Self::file(path)
        } else if let Some(rest) = token.strip_prefix("record:") {
            let (collection, id) = rest
                .split_once('#')
                .ok_or_else(|| Invalid::new("item", "expected record:<collection>#<id>"))?;
            Self::record(collection, id)
        } else {
            return Err(Invalid::new(
                "item",
                "expected file:<path> or record:<collection>#<id>",
            ));
        };
        item.validate()?;
        Ok(item)
    }

    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::File { path } => format!("file:{path}"),
            Self::Record { collection, id } => format!("record:{collection}#{id}"),
        }
    }

    pub fn validate(&self) -> Result<(), Invalid> {
        match self {
            Self::File { path } => {
                if path.is_empty() || path.len() > 4096 {
                    return Err(Invalid::new("item.path", "expected 1..=4096 characters"));
                }
                if path.starts_with('/') || path.contains('\\') {
                    return Err(Invalid::new("item.path", "expected a relative path"));
                }
                if path.chars().any(char::is_control) {
                    return Err(Invalid::new(
                        "item.path",
                        "control characters are not allowed",
                    ));
                }
                let mut components = path.split('/');
                if components
                    .any(|component| component.is_empty() || component == "." || component == "..")
                {
                    return Err(Invalid::new(
                        "item.path",
                        "empty, `.` and `..` components are not allowed",
                    ));
                }
                Ok(())
            }
            Self::Record { collection, id } => {
                if !valid_name(collection) {
                    return Err(Invalid::new(
                        "item.collection",
                        "expected a records collection name",
                    ));
                }
                if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
                    return Err(Invalid::new("item.id", "expected 1..=512 characters"));
                }
                Ok(())
            }
        }
    }

    /// Whether a grant on `self` reaches `requested`.
    ///
    /// A file grant is a subtree, compared on component boundaries so `docs`
    /// covers `docs/q3.md` but not `docs-private/q3.md`. A record grant is
    /// exact: a collection is not a folder.
    #[must_use]
    pub fn covers(&self, requested: &Self) -> bool {
        match (self, requested) {
            (Self::File { path: base }, Self::File { path }) => {
                path == base
                    || path
                        .strip_prefix(base.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
            (
                Self::Record { collection, id },
                Self::Record {
                    collection: wanted_collection,
                    id: wanted_id,
                },
            ) => collection == wanted_collection && id == wanted_id,
            _ => false,
        }
    }
}

/// One object shared with someone who is not necessarily a member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub item: Item,
    pub grantee: PrincipalId,
    pub role: Role,
    pub expires_at_ms: Option<i64>,
}

impl Share {
    /// Whether this share grants the principal anything for `requested`, now.
    pub fn grant(
        &self,
        principal: &PrincipalId,
        requested: &Item,
        now_ms: i64,
    ) -> Result<Role, Refusal> {
        if &self.grantee != principal {
            return Err(Refusal::NotGranted);
        }
        if self.expires_at_ms.is_some_and(|at| at <= now_ms) {
            return Err(Refusal::Expired);
        }
        if !self.item.covers(requested) {
            return Err(Refusal::NotGranted);
        }
        Ok(self.role)
    }
}

/// A one-item, one-principal grant with a use budget and an expiry.
///
/// A ticket is the disposable form of a [`Share`]. It names the grantee rather
/// than relying on whoever opens the link, so a forwarded ticket is useless to
/// anyone else. The id is an opaque handle minted by the host; it is stored in
/// the Desktop manifest and presented back at redemption. It is not a bearer
/// secret: possession of the id is not enough, and the principal must match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: String,
    pub item: Item,
    pub grantee: PrincipalId,
    pub role: Role,
    pub issued_by: PrincipalId,
    pub issued_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    /// How many times the ticket may be redeemed. `1` is the common case.
    pub max_uses: u32,
}

/// What a caller asks a Desktop to mint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRequest {
    pub id: String,
    pub item: Item,
    pub grantee: PrincipalId,
    pub role: Role,
    pub expires_at_ms: Option<i64>,
    pub max_uses: u32,
}

/// One recorded redemption.
///
/// Redemptions are append-only records, not a counter on the ticket: two devices
/// redeeming the same ticket must be able to see each other's use after sync
/// instead of each writing `uses = 1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redemption {
    pub ticket: String,
    pub principal: PrincipalId,
    pub at_ms: i64,
}

impl Redemption {
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.ticket.is_empty() || self.ticket.len() > 128 {
            return Err(Invalid::new(
                "redemption.ticket",
                "expected 1..=128 characters",
            ));
        }
        if !valid_principal(self.principal.as_str()) {
            return Err(Invalid::new(
                "redemption.principal",
                "expected a non-empty principal",
            ));
        }
        Ok(())
    }
}

impl Ticket {
    /// Static checks: is this ticket for this principal, and is it live?
    ///
    /// Does not consider the use budget; [`Ticket::redeem`] does.
    pub fn grant(&self, principal: &PrincipalId, now_ms: i64) -> Result<Role, Refusal> {
        if &self.grantee != principal {
            return Err(Refusal::NotGranted);
        }
        if now_ms < self.issued_at_ms {
            return Err(Refusal::NotGranted);
        }
        if self.expires_at_ms.is_some_and(|at| at <= now_ms) {
            return Err(Refusal::Expired);
        }
        Ok(self.role)
    }

    /// Redeem one use. `used` is the number of redemptions already recorded.
    pub fn redeem(&self, principal: &PrincipalId, used: u32, now_ms: i64) -> Result<Role, Refusal> {
        let role = self.grant(principal, now_ms)?;
        if used >= self.max_uses {
            return Err(Refusal::Exhausted);
        }
        Ok(role)
    }

    pub fn validate(&self) -> Result<(), Invalid> {
        if self.id.len() < 16 || self.id.len() > 128 {
            return Err(Invalid::new("ticket.id", "expected 16..=128 characters"));
        }
        if self.id.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || character == '-' || character == '_')
        }) {
            return Err(Invalid::new(
                "ticket.id",
                "expected ASCII letters, digits, `-` or `_`",
            ));
        }
        self.item.validate()?;
        if !valid_principal(self.grantee.as_str()) {
            return Err(Invalid::new(
                "ticket.grantee",
                "expected a non-empty principal",
            ));
        }
        if !valid_principal(self.issued_by.as_str()) {
            return Err(Invalid::new(
                "ticket.issued_by",
                "expected a non-empty principal",
            ));
        }
        if self.max_uses == 0 || self.max_uses > 1024 {
            return Err(Invalid::new("ticket.max_uses", "expected 1..=1024"));
        }
        if self.expires_at_ms.is_some_and(|at| at <= self.issued_at_ms) {
            return Err(Invalid::new(
                "ticket.expires_at_ms",
                "expected after issued_at_ms",
            ));
        }
        Ok(())
    }
}

/// What a Desktop wants installed, and whether it insists.
///
/// `required` is the whole point of a desktop having an install list: a shared
/// desktop can pin a Spore and its grants, and a member that cannot satisfy the
/// pin is visibly non-compliant rather than quietly different.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Install {
    pub spore: String,
    /// The version or digest the desktop will accept, when it pins one.
    pub version: Option<String>,
    pub grants: std::collections::BTreeMap<String, String>,
    pub required: bool,
}

/// A records namespace this Desktop offers to Spores.
///
/// The book is *not* stored. [`crate::DesktopKey::records_book`] derives it
/// from the Desktop id and this name, so a Desktop cannot be moved to a
/// different book by editing a manifest field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordsBinding {
    /// The namespace a Spore asks for, e.g. `crm`.
    pub name: String,
    /// The collection inside that namespace, e.g. `account`.
    pub collection: String,
}

/// A long-running Spore this Desktop wants placed on a device.
///
/// Placement eligibility has two halves. `requires` is this half: facts the
/// device must offer. The other half — does the Desktop's principal or team
/// hold a grant on that device — is Mycelium's, and is passed to
/// [`Desktop::placement_candidates`] as a predicate rather than copied here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub spore: String,
    pub required: bool,
    pub requires: BTreeSet<Capability>,
}

/// The context an access decision needs that is not part of one Desktop.
#[derive(Clone, Copy, Debug)]
pub struct Access<'a> {
    pub teams: &'a [Team],
    pub redemptions: &'a [Redemption],
    pub now_ms: i64,
}

impl<'a> Access<'a> {
    #[must_use]
    pub fn new(teams: &'a [Team], redemptions: &'a [Redemption], now_ms: i64) -> Self {
        Self {
            teams,
            redemptions,
            now_ms,
        }
    }
}

/// The unit of work and sharing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desktop {
    pub id: String,
    /// Presentation only. The id is identity, including across devices.
    pub name: String,
    pub scope: Scope,
    pub owner: Owner,
    pub members: Vec<Membership>,
    pub shares: Vec<Share>,
    pub tickets: Vec<Ticket>,
    pub installs: Vec<Install>,
    /// Spores this desktop refuses, whatever a device has installed.
    ///
    /// The mirror of `required`: a desktop that can insist on one Spore should
    /// be able to refuse another, or "shared desktop" means only "shared
    /// installs". `spore.toml` already carries a `denied` map for capabilities;
    /// this is the same intent one level up.
    pub denied: BTreeSet<String>,
    /// Services this Desktop wants placed. The Desktop declares them; Mycelium
    /// decides which eligible device runs each one.
    pub services: Vec<Service>,
    /// Records namespaces the Desktop offers.
    pub records: Vec<RecordsBinding>,
    /// Changes on every membership, share, ticket, install or service edit.
    pub generation: u64,
}

impl Desktop {
    /// The relation this principal has to the desktop, if any.
    ///
    /// A share or ticket deliberately does not appear here: receiving a
    /// document is not membership, and a guest must not be able to read the
    /// rest of the tree by virtue of having been sent one file.
    pub fn relation(
        &self,
        teams: &[Team],
        principal: &PrincipalId,
        now_ms: i64,
    ) -> Result<Role, Refusal> {
        let mut best: Option<Role> = None;
        let mut expired = false;

        match &self.owner {
            Owner::Principal { principal: owner } if owner == principal => return Ok(Role::Owner),
            Owner::Team { team } => {
                if let Some(team) = teams.iter().find(|candidate| &candidate.id == team) {
                    match team.relation(principal, now_ms) {
                        Ok(role) => best = Some(role),
                        Err(Refusal::Expired) => expired = true,
                        Err(_) => {}
                    }
                }
            }
            Owner::Principal { .. } => {}
        }

        for member in &self.members {
            if &member.principal != principal {
                continue;
            }
            if member.live(now_ms) {
                best = Some(best.map_or(member.role, |role| role.max(member.role)));
            } else {
                expired = true;
            }
        }

        best.ok_or(if expired {
            Refusal::Expired
        } else {
            Refusal::NotAMember
        })
    }

    /// Whether this principal may administer the desktop.
    pub fn is_owner(&self, teams: &[Team], principal: &PrincipalId, now_ms: i64) -> bool {
        self.relation(teams, principal, now_ms) == Ok(Role::Owner)
    }

    /// The strongest grant this principal has for one item, now.
    ///
    /// Membership covers the whole export, so it participates in the comparison
    /// like any share. A ticket only counts after it has been redeemed: a minted
    /// but unclaimed ticket is an offer, not access.
    pub fn access(
        &self,
        access: &Access<'_>,
        principal: &PrincipalId,
        item: &Item,
    ) -> Result<Role, Refusal> {
        let mut best: Option<Role> = None;
        let mut expired = false;
        let mut candidate = false;

        match self.relation(access.teams, principal, access.now_ms) {
            Ok(role) => best = Some(role),
            Err(Refusal::Expired) => expired = true,
            Err(_) => {}
        }

        for share in &self.shares {
            if &share.grantee != principal {
                continue;
            }
            candidate = true;
            match share.grant(principal, item, access.now_ms) {
                Ok(role) => best = Some(best.map_or(role, |current| current.max(role))),
                Err(Refusal::Expired) => expired = true,
                Err(_) => {}
            }
        }

        for ticket in &self.tickets {
            if &ticket.grantee != principal || !ticket.item.covers(item) {
                continue;
            }
            candidate = true;
            match ticket.grant(principal, access.now_ms) {
                Ok(role) => {
                    let redeemed = access.redemptions.iter().any(|redemption| {
                        redemption.ticket == ticket.id && &redemption.principal == principal
                    });
                    if redeemed {
                        best = Some(best.map_or(role, |current| current.max(role)));
                    }
                }
                Err(Refusal::Expired) => expired = true,
                Err(_) => {}
            }
        }

        if let Some(role) = best {
            return Ok(role);
        }
        if expired {
            return Err(Refusal::Expired);
        }
        if candidate {
            return Err(Refusal::NotGranted);
        }
        Err(self
            .relation(access.teams, principal, access.now_ms)
            .err()
            .unwrap_or(Refusal::NotGranted))
    }

    /// Mint a ticket for one item, to one principal.
    ///
    /// The issuer cannot hand out more than they hold: the requested role must
    /// be covered by the issuer's own access to that item. A ticket does not
    /// make the grantee a member.
    pub fn issue_ticket(
        &mut self,
        access: &Access<'_>,
        actor: &PrincipalId,
        request: TicketRequest,
    ) -> Result<Ticket, Error> {
        let ticket = Ticket {
            id: request.id,
            item: request.item,
            grantee: request.grantee,
            role: request.role,
            issued_by: actor.clone(),
            issued_at_ms: access.now_ms,
            expires_at_ms: request.expires_at_ms,
            max_uses: request.max_uses,
        };
        ticket.validate()?;
        if self.tickets.iter().any(|existing| existing.id == ticket.id) {
            return Err(Invalid::new("ticket.id", "already exists").into());
        }
        let held = self.access(access, actor, &ticket.item)?;
        if !held.covers(ticket.role) {
            return Err(Refusal::NotGranted.into());
        }
        self.tickets.push(ticket.clone());
        self.bump();
        Ok(ticket)
    }

    /// Withdraw a ticket. The issuer may withdraw their own; an owner may
    /// withdraw any.
    pub fn revoke_ticket(
        &mut self,
        access: &Access<'_>,
        actor: &PrincipalId,
        ticket_id: &str,
    ) -> Result<(), Error> {
        let Some(ticket) = self.tickets.iter().find(|ticket| ticket.id == ticket_id) else {
            return Err(Invalid::new("ticket.id", "not found").into());
        };
        let owner = self.is_owner(access.teams, actor, access.now_ms);
        if !owner && ticket.issued_by != *actor {
            return Err(Refusal::NotGranted.into());
        }
        self.tickets.retain(|ticket| ticket.id != ticket_id);
        self.bump();
        Ok(())
    }

    /// Add a durable share for one item.
    ///
    /// Same rule as a ticket: the issuer cannot grant more than they hold.
    pub fn add_share(
        &mut self,
        access: &Access<'_>,
        actor: &PrincipalId,
        share: Share,
    ) -> Result<(), Error> {
        share.item.validate()?;
        let held = self.access(access, actor, &share.item)?;
        if !held.covers(share.role) {
            return Err(Refusal::NotGranted.into());
        }
        self.shares.push(share);
        self.bump();
        Ok(())
    }

    /// Add, change or remove a desktop member. Owners only.
    pub fn set_member(
        &mut self,
        access: &Access<'_>,
        actor: &PrincipalId,
        principal: &PrincipalId,
        role: Option<Role>,
    ) -> Result<(), Error> {
        if !self.is_owner(access.teams, actor, access.now_ms) {
            return Err(Refusal::NotGranted.into());
        }
        let changed = match role {
            Some(role) => match self
                .members
                .iter_mut()
                .find(|member| &member.principal == principal)
            {
                Some(member) if member.role != role => {
                    member.role = role;
                    true
                }
                Some(_) => false,
                None => {
                    self.members.push(Membership::new(principal.clone(), role));
                    true
                }
            },
            None => {
                let before = self.members.len();
                self.members.retain(|member| &member.principal != principal);
                self.members.len() != before
            }
        };
        if changed {
            self.bump();
        }
        Ok(())
    }

    /// Devices that could run this service, before Mycelium picks one.
    ///
    /// `granted` answers the half of eligibility this model does not own: does
    /// the desktop's principal or team hold a grant on this device? Both halves
    /// must hold, so an eligible device nobody granted is still not a candidate.
    #[must_use]
    pub fn placement_candidates<'a>(
        &self,
        service: &Service,
        devices: &'a [Device],
        granted: impl Fn(&Owner, &str) -> bool,
    ) -> Vec<&'a Device> {
        devices
            .iter()
            .filter(|device| {
                service.requires.is_subset(&device.capabilities) && granted(&self.owner, &device.id)
            })
            .collect()
    }

    /// Required installs this device does not have.
    #[must_use]
    pub fn unsatisfied(&self, installed: &BTreeSet<String>) -> Vec<&Install> {
        self.installs
            .iter()
            .filter(|install| install.required && !installed.contains(&install.spore))
            .collect()
    }

    /// Whether this desktop lets the Spore run at all.
    ///
    /// Denial is checked before any install state, so installing a refused
    /// Spore is not a way to run it. This is the one rule that has to hold
    /// against a device that has been tampered with, and it is why the answer
    /// is a refusal rather than a boolean the caller can forget to check.
    pub fn permits(&self, spore: &str) -> Result<(), Refusal> {
        if self.denied.contains(spore) {
            return Err(Refusal::Denied);
        }
        Ok(())
    }

    /// Configurations this manifest names but could never satisfy.
    ///
    /// Not an error the model can throw on its own — a desktop is configured by
    /// people — but a contradiction it can *name*, so a host can refuse to load
    /// it instead of silently letting denial win and leaving a required install
    /// permanently unsatisfiable.
    #[must_use]
    pub fn contradictions(&self) -> Vec<String> {
        let mut contradictions = Vec::new();
        for install in &self.installs {
            if install.required && self.denied.contains(&install.spore) {
                contradictions.push(format!("{} is both required and denied", install.spore));
            }
        }
        match (&self.scope, &self.owner) {
            (Scope::Personal, Owner::Team { team }) => {
                contradictions.push(format!("a personal desktop cannot be owned by team {team}"))
            }
            (Scope::Team, Owner::Principal { principal }) => contradictions.push(format!(
                "a team desktop cannot be owned by principal {principal}"
            )),
            _ => {}
        }
        contradictions
    }

    pub fn validate(&self) -> Result<(), Invalid> {
        if !valid_id(&self.id) {
            return Err(Invalid::new(
                "desktop.id",
                "expected lowercase letters, digits or `-`",
            ));
        }
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err(Invalid::new("desktop.name", "expected 1..=200 characters"));
        }
        match &self.owner {
            Owner::Principal { principal } => {
                if !valid_principal(principal.as_str()) {
                    return Err(Invalid::new(
                        "desktop.owner",
                        "expected a non-empty principal",
                    ));
                }
            }
            Owner::Team { team } => {
                if !valid_id(team.as_str()) {
                    return Err(Invalid::new("desktop.owner.team", "expected a team id"));
                }
            }
        }
        for member in &self.members {
            if !valid_principal(member.principal.as_str()) {
                return Err(Invalid::new(
                    "desktop.member",
                    "expected a non-empty principal",
                ));
            }
        }
        for share in &self.shares {
            share.item.validate()?;
            if !valid_principal(share.grantee.as_str()) {
                return Err(Invalid::new(
                    "desktop.share.grantee",
                    "expected a non-empty principal",
                ));
            }
        }
        for ticket in &self.tickets {
            ticket.validate()?;
        }
        let unique: BTreeSet<&str> = self
            .tickets
            .iter()
            .map(|ticket| ticket.id.as_str())
            .collect();
        if unique.len() != self.tickets.len() {
            return Err(Invalid::new("desktop.tickets", "duplicate ticket id"));
        }
        for install in &self.installs {
            if install.spore.trim().is_empty() {
                return Err(Invalid::new("desktop.install.spore", "expected a Spore id"));
            }
        }
        let mut records = BTreeSet::new();
        for binding in &self.records {
            if !valid_name(&binding.name) || !records.insert(binding.name.as_str()) {
                return Err(Invalid::new(
                    "desktop.records.name",
                    "expected a unique namespace name",
                ));
            }
            if !valid_name(&binding.collection) {
                return Err(Invalid::new(
                    "desktop.records.collection",
                    "expected a collection name",
                ));
            }
        }
        let mut services = BTreeSet::new();
        for service in &self.services {
            if !valid_name(&service.name) || !services.insert(service.name.as_str()) {
                return Err(Invalid::new(
                    "desktop.services.name",
                    "expected a unique service name",
                ));
            }
            if service.spore.trim().is_empty() {
                return Err(Invalid::new(
                    "desktop.services.spore",
                    "expected a Spore id",
                ));
            }
        }
        Ok(())
    }

    fn bump(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Device, Owner, PrincipalId, Scope, Team};

    const NOW: i64 = 1_000_000;

    fn principal(id: &str) -> PrincipalId {
        PrincipalId::from(id)
    }

    fn desktop(owner: Owner, members: &[(&str, Role)]) -> Desktop {
        let scope = match &owner {
            Owner::Team { .. } => Scope::Team,
            Owner::Principal { .. } => Scope::Personal,
        };
        Desktop {
            id: "fpl".to_owned(),
            name: "FPL".to_owned(),
            scope,
            owner,
            members: members
                .iter()
                .map(|(who, role)| Membership::new(principal(who), *role))
                .collect(),
            shares: Vec::new(),
            tickets: Vec::new(),
            installs: Vec::new(),
            denied: BTreeSet::new(),
            services: Vec::new(),
            records: Vec::new(),
            generation: 0,
        }
    }

    fn access<'a>(teams: &'a [Team]) -> Access<'a> {
        Access::new(teams, &[], NOW)
    }

    #[test]
    fn the_owner_is_a_member_without_being_listed() {
        let fpl = desktop(Owner::principal("avery"), &[]);
        assert_eq!(fpl.relation(&[], &principal("avery"), NOW), Ok(Role::Owner));
    }

    #[test]
    fn a_team_owner_makes_team_members_desktop_members() {
        let mut teams = vec![Team::new("fpl", "FPL", "avery")];
        teams[0]
            .set_member(
                &principal("avery"),
                &principal("kwame"),
                Some(Role::Writer),
                NOW,
            )
            .unwrap();
        let desktop = desktop(Owner::team("fpl"), &[]);

        assert_eq!(
            desktop.relation(&teams, &principal("kwame"), NOW),
            Ok(Role::Writer)
        );
        assert_eq!(
            desktop.relation(&teams, &principal("dana"), NOW),
            Err(Refusal::NotAMember)
        );
    }

    #[test]
    fn a_lapsed_membership_reads_as_expired_not_absent() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.members
            .push(Membership::new(principal("kwame"), Role::Writer).until(NOW - 1));
        assert_eq!(
            fpl.relation(&[], &principal("kwame"), NOW),
            Err(Refusal::Expired)
        );
        assert_eq!(
            fpl.relation(&[], &principal("nobody"), NOW),
            Err(Refusal::NotAMember),
            "never admitted is a different answer from admitted and lapsed"
        );
    }

    #[test]
    fn the_strongest_live_membership_wins() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.members
            .push(Membership::new(principal("kwame"), Role::Reader).until(NOW - 1));
        fpl.members
            .push(Membership::new(principal("kwame"), Role::Writer));
        assert_eq!(
            fpl.relation(&[], &principal("kwame"), NOW),
            Ok(Role::Writer)
        );
    }

    #[test]
    fn receiving_a_shared_file_does_not_make_you_a_member() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.shares.push(Share {
            item: Item::file("docs/q3.md"),
            grantee: principal("dana"),
            role: Role::Reader,
            expires_at_ms: None,
        });

        // The object is readable...
        assert_eq!(
            fpl.access(&access(&[]), &principal("dana"), &Item::file("docs/q3.md")),
            Ok(Role::Reader)
        );
        // ...and the desktop is still not hers, so the rest of the tree is not
        // reachable by having been sent one file.
        assert_eq!(
            fpl.relation(&[], &principal("dana"), NOW),
            Err(Refusal::NotAMember)
        );
        assert_eq!(
            fpl.access(
                &access(&[]),
                &principal("dana"),
                &Item::file("notes/plan.md")
            ),
            Err(Refusal::NotGranted)
        );
    }

    #[test]
    fn a_folder_share_is_a_subtree_on_component_boundaries() {
        let share = Share {
            item: Item::file("docs"),
            grantee: principal("dana"),
            role: Role::Reader,
            expires_at_ms: None,
        };
        assert_eq!(
            share.grant(&principal("dana"), &Item::file("docs/q3.md"), NOW),
            Ok(Role::Reader)
        );
        assert_eq!(
            share.grant(&principal("dana"), &Item::file("docs-private/q3.md"), NOW),
            Err(Refusal::NotGranted)
        );
    }

    #[test]
    fn an_expired_share_is_refused_and_says_which_kind_of_no() {
        let share = Share {
            item: Item::file("docs/q3.md"),
            grantee: principal("dana"),
            role: Role::Reader,
            expires_at_ms: Some(NOW - 1),
        };
        assert_eq!(
            share.grant(&principal("dana"), &Item::file("docs/q3.md"), NOW),
            Err(Refusal::Expired)
        );
        assert_eq!(
            share.grant(&principal("someone-else"), &Item::file("docs/q3.md"), NOW),
            Err(Refusal::NotGranted)
        );
    }

    fn ticket(id: &str, grantee: &str, item: Item, max_uses: u32) -> Ticket {
        Ticket {
            id: id.to_owned(),
            item,
            grantee: principal(grantee),
            role: Role::Reader,
            issued_by: principal("avery"),
            issued_at_ms: NOW,
            expires_at_ms: None,
            max_uses,
        }
    }

    #[test]
    fn a_ticket_is_an_offer_until_it_is_redeemed() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.tickets.push(ticket(
            "ticket-0123456789abcdef",
            "dana",
            Item::file("docs/q3.md"),
            1,
        ));
        let item = Item::file("docs/q3.md");

        // Minted, but not claimed: still not access.
        assert_eq!(
            fpl.access(&access(&[]), &principal("dana"), &item),
            Err(Refusal::NotGranted)
        );

        let redemptions = vec![Redemption {
            ticket: "ticket-0123456789abcdef".to_owned(),
            principal: principal("dana"),
            at_ms: NOW,
        }];
        let claimed = Access::new(&[], &redemptions, NOW);
        assert_eq!(
            fpl.access(&claimed, &principal("dana"), &item),
            Ok(Role::Reader)
        );
        // Still not a member.
        assert_eq!(
            fpl.relation(&[], &principal("dana"), NOW),
            Err(Refusal::NotAMember)
        );
    }

    #[test]
    fn a_ticket_named_for_one_principal_does_not_work_for_another() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.tickets.push(ticket(
            "ticket-0123456789abcdef",
            "dana",
            Item::record("contacts", "ada"),
            1,
        ));
        let ticket = &fpl.tickets[0];
        assert_eq!(
            ticket.redeem(&principal("mallory"), 0, NOW),
            Err(Refusal::NotGranted)
        );
    }

    #[test]
    fn a_ticket_use_budget_is_spent_not_reset() {
        let ticket = ticket(
            "ticket-0123456789abcdef",
            "dana",
            Item::file("docs/q3.md"),
            1,
        );
        assert_eq!(ticket.redeem(&principal("dana"), 0, NOW), Ok(Role::Reader));
        assert_eq!(
            ticket.redeem(&principal("dana"), 1, NOW),
            Err(Refusal::Exhausted)
        );
    }

    #[test]
    fn an_expired_ticket_is_refused_before_it_is_counted() {
        let mut ticket = ticket(
            "ticket-0123456789abcdef",
            "dana",
            Item::file("docs/q3.md"),
            1,
        );
        ticket.expires_at_ms = Some(NOW - 1);
        assert_eq!(
            ticket.redeem(&principal("dana"), 0, NOW),
            Err(Refusal::Expired)
        );
    }

    #[test]
    fn issuing_a_ticket_cannot_exceed_the_issuers_own_access() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.shares.push(Share {
            item: Item::file("docs/q3.md"),
            grantee: principal("kwame"),
            role: Role::Reader,
            expires_at_ms: None,
        });
        let context = access(&[]);
        let request = TicketRequest {
            id: "ticket-0123456789abcdef".to_owned(),
            item: Item::file("docs/q3.md"),
            grantee: principal("dana"),
            role: Role::Writer,
            expires_at_ms: None,
            max_uses: 1,
        };

        // A reader cannot mint a writer.
        assert_eq!(
            fpl.issue_ticket(&context, &principal("kwame"), request.clone()),
            Err(Refusal::NotGranted.into())
        );
        // An owner can, and the ticket is recorded in the manifest.
        let issued = fpl
            .issue_ticket(&context, &principal("avery"), request)
            .unwrap();
        assert_eq!(issued.grantee, principal("dana"));
        assert_eq!(fpl.tickets.len(), 1);
        assert!(fpl.generation > 0);
    }

    #[test]
    fn an_owner_can_revoke_any_ticket_but_an_issuer_only_their_own() {
        let mut fpl = desktop(Owner::principal("avery"), &[("kwame", Role::Owner)]);
        fpl.tickets.push(ticket(
            "ticket-0123456789abcdef",
            "dana",
            Item::file("docs/q3.md"),
            1,
        ));
        let context = access(&[]);

        assert_eq!(
            fpl.revoke_ticket(&context, &principal("kwame"), "ticket-0123456789abcdef"),
            Ok(())
        );
        assert!(fpl.tickets.is_empty());
    }

    #[test]
    fn capabilities_resolve_narrowest_first() {
        let fpl = desktop(Owner::principal("avery"), &[]);
        let audio = Capability::from("soil:audio");
        let studio = Device {
            id: "studio".to_owned(),
            owner: principal("avery"),
            assigned_to: None,
            capabilities: ["soil:audio".into()].into_iter().collect(),
        };
        let remote = crate::Provider {
            device: "agora-one".to_owned(),
            capability: Capability::from("soil:audio"),
        };

        let host: BTreeSet<Capability> = [audio.clone()].into_iter().collect();
        assert_eq!(
            crate::resolve_capability(
                &fpl,
                &access(&[]),
                &principal("avery"),
                &studio,
                &audio,
                std::slice::from_ref(&remote),
                &host
            ),
            crate::Resolution::InProcess
        );

        let empty = BTreeSet::new();
        assert_eq!(
            crate::resolve_capability(
                &fpl,
                &access(&[]),
                &principal("avery"),
                &studio,
                &audio,
                std::slice::from_ref(&remote),
                &empty
            ),
            crate::Resolution::Local {
                device: "studio".to_owned()
            }
        );

        let thin = Device {
            id: "phone".to_owned(),
            owner: principal("avery"),
            assigned_to: None,
            capabilities: BTreeSet::new(),
        };
        assert_eq!(
            crate::resolve_capability(
                &fpl,
                &access(&[]),
                &principal("avery"),
                &thin,
                &audio,
                &[remote],
                &empty
            ),
            crate::Resolution::Peer {
                device: "agora-one".to_owned()
            }
        );

        assert_eq!(
            crate::resolve_capability(
                &fpl,
                &access(&[]),
                &principal("avery"),
                &thin,
                &audio,
                &[],
                &empty
            ),
            crate::Resolution::Refused(Refusal::NoProvider)
        );
    }

    #[test]
    fn a_local_provider_is_not_a_shortcut_for_a_non_member() {
        let fpl = desktop(Owner::principal("avery"), &[]);
        let studio = Device {
            id: "studio".to_owned(),
            owner: principal("avery"),
            assigned_to: None,
            capabilities: ["soil:audio".into()].into_iter().collect(),
        };
        let audio = Capability::from("soil:audio");
        let remote = crate::Provider {
            device: "agora-one".to_owned(),
            capability: Capability::from("soil:audio"),
        };

        assert_eq!(
            crate::resolve_capability(
                &fpl,
                &access(&[]),
                &principal("dana"),
                &studio,
                &audio,
                std::slice::from_ref(&remote),
                &BTreeSet::new()
            ),
            crate::Resolution::Refused(Refusal::NotAMember)
        );
    }

    #[test]
    fn a_shared_desktop_can_require_a_spore() {
        let mut fpl = desktop(Owner::principal("avery"), &[("kwame", Role::Writer)]);
        fpl.installs.push(Install {
            spore: "dev.fungos.mail".to_owned(),
            version: Some("1.4".to_owned()),
            grants: std::collections::BTreeMap::from([(
                "soil:http".to_owned(),
                "mail.example".to_owned(),
            )]),
            required: true,
        });
        fpl.installs.push(Install {
            spore: "dev.fungos.sheets".to_owned(),
            version: None,
            grants: std::collections::BTreeMap::new(),
            required: false,
        });

        let nothing: BTreeSet<String> = BTreeSet::new();
        let missing = fpl.unsatisfied(&nothing);
        assert_eq!(missing.len(), 1, "only the required one is compliance");
        assert_eq!(missing[0].spore, "dev.fungos.mail");

        let mail_only: BTreeSet<String> = ["dev.fungos.mail".to_owned()].into_iter().collect();
        assert!(fpl.unsatisfied(&mail_only).is_empty());
    }

    #[test]
    fn a_desktop_refuses_a_spore_even_where_it_is_installed() {
        let mut fpl = desktop(Owner::principal("avery"), &[("kwame", Role::Writer)]);
        fpl.denied.insert("dev.fungos.chatmail".to_owned());

        assert_eq!(fpl.permits("dev.fungos.chatmail"), Err(Refusal::Denied));
        assert_eq!(fpl.permits("dev.fungos.sheets"), Ok(()));
    }

    #[test]
    fn a_desktop_that_both_requires_and_refuses_a_spore_can_say_so() {
        let mut fpl = desktop(Owner::principal("avery"), &[("kwame", Role::Writer)]);
        fpl.denied.insert("dev.fungos.chatmail".to_owned());
        fpl.installs.push(Install {
            spore: "dev.fungos.chatmail".to_owned(),
            version: None,
            grants: std::collections::BTreeMap::new(),
            required: true,
        });

        assert_eq!(
            fpl.contradictions(),
            vec!["dev.fungos.chatmail is both required and denied".to_owned()]
        );
        assert_eq!(fpl.permits("dev.fungos.chatmail"), Err(Refusal::Denied));
    }

    #[test]
    fn a_personal_desktop_owned_by_a_team_is_named_as_a_contradiction() {
        let mut fpl = desktop(Owner::team("fpl"), &[]);
        fpl.scope = Scope::Personal;
        assert_eq!(fpl.contradictions().len(), 1);
        assert!(
            fpl.validate().is_ok(),
            "the contradiction is named, not hidden"
        );
    }

    #[test]
    fn placement_requires_both_device_facts_and_a_grant() {
        let fpl = desktop(Owner::team("fpl"), &[]);
        let service = Service {
            name: "operator".to_owned(),
            spore: "dev.fungos.operator".to_owned(),
            required: true,
            requires: ["resource.gpu".into()].into_iter().collect(),
        };
        let gpu = Device {
            id: "studio".to_owned(),
            owner: principal("avery"),
            assigned_to: None,
            capabilities: ["resource.gpu".into()].into_iter().collect(),
        };
        let cpu = Device {
            id: "laptop".to_owned(),
            owner: principal("avery"),
            assigned_to: None,
            capabilities: BTreeSet::new(),
        };

        // Capable, but no grant on the device: not a candidate.
        assert!(
            fpl.placement_candidates(&service, std::slice::from_ref(&gpu), |_, _| false)
                .is_empty()
        );
        // Granted, but cannot provide the fact: not a candidate.
        assert!(
            fpl.placement_candidates(&service, std::slice::from_ref(&cpu), |_, _| true)
                .is_empty()
        );
        // Both: a candidate, and the model keeps the owner so Mycelium can ask
        // about the team rather than an individual member.
        let devices = [gpu, cpu];
        let candidates = fpl.placement_candidates(&service, &devices, |owner, device| {
            owner.as_team().is_some() && device == "studio"
        });
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "studio");
    }

    #[test]
    fn a_manifest_that_names_a_bad_share_is_refused() {
        let mut fpl = desktop(Owner::principal("avery"), &[]);
        fpl.shares.push(Share {
            item: Item::file("../escape"),
            grantee: principal("dana"),
            role: Role::Reader,
            expires_at_ms: None,
        });
        assert!(fpl.validate().is_err());
    }

    #[test]
    fn items_round_trip_through_their_token() {
        let file = Item::parse("file:docs/q3.md").unwrap();
        assert_eq!(file.token(), "file:docs/q3.md");
        let record = Item::parse("record:contacts#ada").unwrap();
        assert_eq!(record.token(), "record:contacts#ada");
        assert!(Item::parse("docs/q3.md").is_err());
        assert!(Item::parse("file:../escape").is_err());
    }
}
