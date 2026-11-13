//! Capabilities a device provides, and the narrowest resolution order.
//!
//! A capability is a newtype rather than an enum so a new capability is a new
//! string, not a breaking change to every match in the tree.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Access, Desktop, PrincipalId, Refusal};

/// A capability a device can provide or a Spore can ask for, e.g. `soil:audio`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capability(String);

impl Capability {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Capability {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

/// A machine.
///
/// Owned by one principal, possibly assigned to another: an employer's laptop
/// is *owned* by the employer and *used* by a person, and both statements have
/// to be able to be true at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub owner: PrincipalId,
    pub assigned_to: Option<PrincipalId>,
    /// What this machine can provide itself.
    pub capabilities: BTreeSet<Capability>,
}

/// A capability offered by a machine, for a desktop that admitted it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub device: String,
    pub capability: Capability,
}

/// Where a capability came from, or why it did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The host implements the interface itself: a function call, no boundary.
    InProcess,
    /// Another process on this machine owns the resource.
    Local {
        device: String,
    },
    /// A machine elsewhere in the desktop owns the resource.
    Peer {
        device: String,
    },
    Refused(Refusal),
}

/// Resolve a capability request, first match wins.
///
/// The order is cheapest-and-narrowest first: the host's own implementation,
/// then a provider on this machine, then one elsewhere in the desktop.
///
/// The important property is that **this machine is not a shortcut**. A local
/// provider is a remote that happens to be nearby: it is reached over a
/// provider protocol by a different process, so it passes the same relation
/// check as a peer would. If locality short-circuited authorization, moving a
/// provider onto this box would silently escalate it.
#[must_use]
pub fn resolve_capability(
    desktop: &Desktop,
    access: &Access<'_>,
    principal: &PrincipalId,
    device: &Device,
    capability: &Capability,
    providers: &[Provider],
    host: &BTreeSet<Capability>,
) -> Resolution {
    if let Err(refusal) = desktop.relation(access.teams, principal, access.now_ms) {
        return Resolution::Refused(refusal);
    }
    if host.contains(capability) {
        return Resolution::InProcess;
    }
    if device.capabilities.contains(capability) {
        return Resolution::Local {
            device: device.id.clone(),
        };
    }
    match providers
        .iter()
        .find(|provider| &provider.capability == capability)
    {
        Some(provider) => Resolution::Peer {
            device: provider.device.clone(),
        },
        None => Resolution::Refused(Refusal::NoProvider),
    }
}
