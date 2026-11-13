//! Teams: a named membership set that can own Desktops.
//!
//! A team is not a desktop and not a role. It answers one question — *who
//! belongs* — so a Desktop can be owned by `team:fpl` instead of by a list that
//! every departure has to remember to edit. The authority to change a team is
//! the team's own owner or a member with [`Role::Owner`], never the desktop it
//! happens to own.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, Invalid, Membership, PrincipalId, Refusal, Role, valid_id, valid_principal};

/// A stable team id. Safe as a records path component and a Geode slot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamId(String);

impl TeamId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TeamId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<String> for TeamId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for TeamId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A named membership set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: TeamId,
    /// Presentation only. The id is identity.
    pub name: String,
    /// The principal that may always administer the team.
    pub owner: PrincipalId,
    pub members: Vec<Membership>,
    /// Changes on every membership edit, so a writer can fence a stale copy.
    pub generation: u64,
}

impl Team {
    #[must_use]
    pub fn new(
        id: impl Into<TeamId>,
        name: impl Into<String>,
        owner: impl Into<PrincipalId>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            owner: owner.into(),
            members: Vec::new(),
            generation: 0,
        }
    }

    /// The relation this principal has to the team, if any.
    ///
    /// A lapsed membership is reported as [`Refusal::Expired`] rather than
    /// absent, because "we admitted you and it ran out" is a different answer
    /// from "we never admitted you" — and the first one is actionable.
    pub fn relation(&self, principal: &PrincipalId, now_ms: i64) -> Result<Role, Refusal> {
        if &self.owner == principal {
            return Ok(Role::Owner);
        }
        let mut expired = false;
        let mut best: Option<Role> = None;
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

    /// Add, change or remove one member.
    ///
    /// Only an owner (the `owner` principal or a member carrying
    /// [`Role::Owner`]) may edit the set. Removing the owner's own membership
    /// does not remove ownership; that is the `owner` field, not a role.
    pub fn set_member(
        &mut self,
        actor: &PrincipalId,
        principal: &PrincipalId,
        role: Option<Role>,
        now_ms: i64,
    ) -> Result<(), Error> {
        if self.relation(actor, now_ms) != Ok(Role::Owner) {
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
            self.generation = self.generation.saturating_add(1);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), Invalid> {
        if !valid_id(self.id.as_str()) {
            return Err(Invalid::new(
                "team.id",
                "expected lowercase letters, digits or `-`",
            ));
        }
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err(Invalid::new("team.name", "expected 1..=200 characters"));
        }
        if !valid_principal(self.owner.as_str()) {
            return Err(Invalid::new("team.owner", "expected a non-empty principal"));
        }
        for member in &self.members {
            if !valid_principal(member.principal.as_str()) {
                return Err(Invalid::new(
                    "team.member",
                    "expected a non-empty principal",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn principal(id: &str) -> PrincipalId {
        PrincipalId::from(id)
    }

    fn team() -> Team {
        Team::new("fpl", "FPL", "avery")
    }

    #[test]
    fn the_owner_is_a_member_without_being_listed() {
        assert_eq!(team().relation(&principal("avery"), NOW), Ok(Role::Owner));
    }

    #[test]
    fn a_lapsed_team_membership_is_expired_not_absent() {
        let mut team = team();
        team.members
            .push(Membership::new(principal("kwame"), Role::Writer).until(NOW - 1));
        assert_eq!(
            team.relation(&principal("kwame"), NOW),
            Err(Refusal::Expired)
        );
        assert_eq!(
            team.relation(&principal("nobody"), NOW),
            Err(Refusal::NotAMember)
        );
    }

    #[test]
    fn the_strongest_live_membership_wins() {
        let mut team = team();
        team.members
            .push(Membership::new(principal("kwame"), Role::Reader).until(NOW - 1));
        team.members
            .push(Membership::new(principal("kwame"), Role::Writer));
        assert_eq!(team.relation(&principal("kwame"), NOW), Ok(Role::Writer));
    }

    #[test]
    fn only_an_owner_may_edit_the_membership() {
        let mut team = team();
        team.members
            .push(Membership::new(principal("kwame"), Role::Writer));

        assert_eq!(
            team.set_member(
                &principal("kwame"),
                &principal("dana"),
                Some(Role::Reader),
                NOW
            ),
            Err(Refusal::NotGranted.into())
        );
        assert!(
            team.set_member(
                &principal("avery"),
                &principal("dana"),
                Some(Role::Reader),
                NOW
            )
            .is_ok()
        );
        assert_eq!(team.relation(&principal("dana"), NOW), Ok(Role::Reader));
    }

    #[test]
    fn a_co_owner_can_administer_and_a_removal_bumps_the_generation() {
        let mut team = team();
        team.members
            .push(Membership::new(principal("kwame"), Role::Owner));
        let before = team.generation;

        team.set_member(
            &principal("kwame"),
            &principal("dana"),
            Some(Role::Reader),
            NOW,
        )
        .unwrap();
        assert!(
            team.generation > before,
            "a membership edit is a new generation"
        );

        let before = team.generation;
        team.set_member(&principal("avery"), &principal("dana"), None, NOW)
            .unwrap();
        assert!(team.generation > before);
        assert_eq!(
            team.relation(&principal("dana"), NOW),
            Err(Refusal::NotAMember)
        );
    }

    #[test]
    fn an_id_that_would_change_an_address_is_refused() {
        let mut team = team();
        team.id = TeamId::from("../fpl");
        assert!(team.validate().is_err());
    }
}
