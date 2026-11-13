//! Desktops and Teams: the unit of work, sharing and installation.
//!
//! Four nouns and one rule. **Principal** is who — and it is deliberately just
//! the `actor` string `soil:principal` already resolves, so nothing here
//! decides identity. **Device** is a machine, owned by one principal and
//! possibly assigned to another. **Team** is a named membership set that can
//! own a Desktop, so a Desktop does not have to be re-shared with every person
//! who joins or leaves. **Desktop** is the unit of work and sharing: a VFS
//! export, a set of members, a set of installs, the services it wants placed,
//! and the capabilities the people in it may reach.
//!
//! The rule is that a *relation to a desktop* is not the same as *access to an
//! object inside it*. Sharing one document with a guest must not quietly make
//! them a member, and membership must not be the only way to receive a file.
//! Keeping those two mechanisms apart is what lets both exist. A [`Ticket`] is
//! the second mechanism in its most disposable form: one item, one named
//! principal, a use budget, and an expiry. Redeeming it does not add anybody to
//! a Team or a Desktop.
//!
//! This crate is the model and nothing else: no I/O, no mounts, no network. It
//! exists so the rules can be argued with in tests before anything is built on
//! them. Persistence is [`schema`]: one record body per noun, with the schema id
//! naming the contract. The records-backed store lives in `soil-desktop-store`.
//!
//! Three deliberate absences, all worth knowing before extending it:
//!
//! - **No fleet noun.** A fleet is the set of devices that can reach a desktop,
//!   derived from relations, never stored. A stored one would be a second
//!   membership system to keep in step with this one.
//! - **No "global install".** An install is declared by a desktop and
//!   materialised by a device. A device-level install every desktop can see
//!   would be an invisible superset, and a shared desktop at someone's employer
//!   would see what they installed at home.
//! - **No stored book paths.** A Desktop names a records binding; the book and
//!   its Geode address are derived from the Desktop id by [`DesktopKey`]. One
//!   fact, one place.

use std::fmt;

use serde::{Deserialize, Serialize};

mod capability;
mod desktop;
mod key;
pub mod schema;
mod team;

pub use capability::{Capability, Device, Provider, Resolution, resolve_capability};
pub use desktop::{
    Access, Desktop, Install, Item, RecordsBinding, Redemption, Service, Share, Ticket,
    TicketRequest,
};
pub use key::DesktopKey;
pub use team::{Team, TeamId};

/// Who. Never more than the identity `soil:principal` resolved.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(String);

impl PrincipalId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PrincipalId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<String> for PrincipalId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a relation permits inside a team or desktop.
///
/// Ordered so a stronger role satisfies a weaker requirement without a match
/// table: `Writer >= Reader`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reader,
    Writer,
    Owner,
}

impl Role {
    /// Whether this role satisfies a requirement of `required`.
    #[must_use]
    pub fn covers(self, required: Self) -> bool {
        self >= required
    }
}

/// Why something was refused.
///
/// One enum for relations, shares, tickets and capabilities because a caller
/// needs to tell "you are not allowed" from "nobody offers this" — and because a
/// refusal with a reason is what a Spore can show a human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No live relation to this desktop or team.
    NotAMember,
    /// The relation or grant does not cover this.
    NotGranted,
    /// Nobody in this desktop offers the capability.
    NoProvider,
    /// A relation or grant existed and has run out.
    Expired,
    /// A ticket's use budget is spent.
    Exhausted,
    /// This desktop refuses this Spore outright.
    Denied,
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotAMember => "not a member",
            Self::NotGranted => "not granted",
            Self::NoProvider => "no provider",
            Self::Expired => "expired",
            Self::Exhausted => "exhausted",
            Self::Denied => "denied",
        })
    }
}

/// A principal and what they may do, until when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub principal: PrincipalId,
    pub role: Role,
    pub expires_at_ms: Option<i64>,
}

impl Membership {
    #[must_use]
    pub fn new(principal: PrincipalId, role: Role) -> Self {
        Self {
            principal,
            role,
            expires_at_ms: None,
        }
    }

    #[must_use]
    pub fn until(self, expires_at_ms: i64) -> Self {
        Self {
            expires_at_ms: Some(expires_at_ms),
            ..self
        }
    }

    #[must_use]
    pub fn live(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_none_or(|at| at > now_ms)
    }
}

/// Who owns a Team or Desktop: one principal, or one team.
///
/// A team owner is not an alias for its members: when the team changes, the
/// desktop's membership changes with it, and a principal who leaves the team
/// loses the desktop without anybody editing the desktop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Owner {
    Principal { principal: PrincipalId },
    Team { team: TeamId },
}

impl Owner {
    #[must_use]
    pub fn principal(principal: impl Into<PrincipalId>) -> Self {
        Self::Principal {
            principal: principal.into(),
        }
    }

    #[must_use]
    pub fn team(team: impl Into<TeamId>) -> Self {
        Self::Team { team: team.into() }
    }

    #[must_use]
    pub fn as_principal(&self) -> Option<&PrincipalId> {
        match self {
            Self::Principal { principal } => Some(principal),
            Self::Team { .. } => None,
        }
    }

    #[must_use]
    pub fn as_team(&self) -> Option<&TeamId> {
        match self {
            Self::Principal { .. } => None,
            Self::Team { team } => Some(team),
        }
    }
}

/// How far a Desktop reaches.
///
/// Scope is a bound, not a second owner. `Personal` is a desktop with one
/// member; `Team` is a desktop whose owner is a team; `Org` is a desktop owned
/// by a principal or a team but placed at the organisation level. The owner is
/// still the authority; this field tells a host which placement and sharing
/// rules to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Personal,
    Team,
    Org,
}

/// A malformed value, named by the field that failed and why.
///
/// A refusal is an authorization answer; this is a configuration or input
/// error. Keeping them apart stops a typo from being reported as `denied`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    field: &'static str,
    reason: &'static str,
}

impl Invalid {
    #[must_use]
    pub fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }

    #[must_use]
    pub fn field(&self) -> &'static str {
        self.field
    }

    #[must_use]
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for Invalid {}

/// A model operation that either refused or rejected its input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Refused(Refusal),
    Invalid(Invalid),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::Invalid(invalid) => invalid.fmt(formatter),
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

/// One lowercase component: letters, digits and `-`, starting alphanumeric.
///
/// Used for desktop, team and service ids because they end up in a records book
/// path and a Geode reference, where a slash or a dot would change the address.
pub(crate) fn valid_id(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if value.len() > 64 || !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    characters.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

/// One path-safe name inside a desktop: letters, digits, `-`, `_` and `.`.
///
/// Records collections and Geode slots use this shape. A slash is never allowed
/// because both addresses use it as the separator.
pub(crate) fn valid_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 || value.starts_with('.') {
        return false;
    }
    value.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || character == '-'
            || character == '_'
            || character == '.'
    })
}

pub(crate) fn valid_principal(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_satisfies_a_reader_requirement() {
        assert!(Role::Writer.covers(Role::Reader));
        assert!(!Role::Reader.covers(Role::Writer));
        assert!(Role::Owner.covers(Role::Owner));
    }

    #[test]
    fn ids_that_would_change_an_address_are_refused() {
        assert!(valid_id("fpl"));
        assert!(valid_id("agent-1"));
        assert!(!valid_id("../fpl"));
        assert!(!valid_id("FPL"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
        assert!(valid_name("crm"));
        assert!(valid_name("account.v2"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("a/b"));
    }

    #[test]
    fn a_lapsed_membership_reads_as_expired_not_absent() {
        let membership = Membership::new(PrincipalId::from("kwame"), Role::Writer).until(999);
        assert!(membership.live(998));
        assert!(!membership.live(999));
    }
}
