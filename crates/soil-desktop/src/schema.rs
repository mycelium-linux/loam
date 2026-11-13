//! The stored body of each noun, and the schema id that names it.
//!
//! A record is a schema id plus an opaque body. These are the bodies. They are
//! JSON because records already stores JSON fields and a human debugging a
//! manifest should not need a decoder; the schema id is the contract, so a
//! decoder never guesses which noun it is looking at.

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Desktop, Invalid, Redemption, Team};

/// A team manifest: `team:<id>`.
pub const TEAM: &str = "soil.desktop/team@1";
/// A desktop manifest: `desktop:<id>`.
pub const DESKTOP: &str = "soil.desktop/desktop@1";
/// One append-only ticket redemption: `redemption:<ticket>:<n>`.
pub const REDEMPTION: &str = "soil.desktop/redemption@1";

/// Every schema this module knows, for a host that wants to validate a book.
pub const SCHEMAS: &[&str] = &[TEAM, DESKTOP, REDEMPTION];

#[derive(Debug)]
pub enum Error {
    Invalid(Invalid),
    Json(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(invalid) => invalid.fmt(formatter),
            Self::Json(error) => write!(formatter, "manifest body: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Invalid> for Error {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    Ok(serde_json::to_vec(value)?)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    Ok(serde_json::from_slice(bytes)?)
}

impl Team {
    pub fn to_body(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    pub fn from_body(bytes: &[u8]) -> Result<Self, Error> {
        let team: Self = decode(bytes)?;
        team.validate()?;
        Ok(team)
    }
}

impl Desktop {
    pub fn to_body(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    pub fn from_body(bytes: &[u8]) -> Result<Self, Error> {
        let desktop: Self = decode(bytes)?;
        desktop.validate()?;
        Ok(desktop)
    }
}

impl Redemption {
    pub fn to_body(&self) -> Result<Vec<u8>, Error> {
        encode(self)
    }

    pub fn from_body(bytes: &[u8]) -> Result<Self, Error> {
        let redemption: Self = decode(bytes)?;
        redemption.validate()?;
        Ok(redemption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Membership, Owner, PrincipalId, Role, Scope};
    use std::collections::BTreeSet;

    fn principal(id: &str) -> PrincipalId {
        PrincipalId::from(id)
    }

    #[test]
    fn a_team_round_trips_through_its_record_body() {
        let mut team = Team::new("fpl", "FPL", "avery");
        team.members
            .push(Membership::new(principal("kwame"), Role::Writer));
        let body = team.to_body().unwrap();
        assert_eq!(Team::from_body(&body).unwrap(), team);
    }

    #[test]
    fn a_desktop_round_trips_with_its_teams_records_services_and_tickets() {
        let desktop = Desktop {
            id: "fpl".to_owned(),
            name: "FPL".to_owned(),
            scope: Scope::Team,
            owner: Owner::team("fpl"),
            members: vec![Membership::new(principal("kwame"), Role::Writer)],
            shares: Vec::new(),
            tickets: Vec::new(),
            installs: Vec::new(),
            denied: BTreeSet::new(),
            services: Vec::new(),
            records: vec![crate::RecordsBinding {
                name: "crm".to_owned(),
                collection: "account".to_owned(),
            }],
            generation: 3,
        };
        let body = desktop.to_body().unwrap();
        assert_eq!(Desktop::from_body(&body).unwrap(), desktop);
    }

    #[test]
    fn a_body_that_violates_the_schema_is_refused_not_trusted() {
        let mut desktop = Desktop {
            id: "../fpl".to_owned(),
            name: "FPL".to_owned(),
            scope: Scope::Personal,
            owner: Owner::principal("avery"),
            members: Vec::new(),
            shares: Vec::new(),
            tickets: Vec::new(),
            installs: Vec::new(),
            denied: BTreeSet::new(),
            services: Vec::new(),
            records: Vec::new(),
            generation: 0,
        };
        desktop.id = "../fpl".to_owned();
        let body = serde_json::to_vec(&desktop).unwrap();
        assert!(Desktop::from_body(&body).is_err());
    }

    #[test]
    fn the_type_is_not_guessed_from_the_body() {
        let team = Team::new("fpl", "FPL", "avery");
        let body = team.to_body().unwrap();
        // A desktop decoder given a team body fails on the missing fields, and
        // the caller still has the schema id to say which one it meant.
        assert!(Desktop::from_body(&body).is_err());
    }
}
