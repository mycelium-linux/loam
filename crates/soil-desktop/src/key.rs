//! One address scheme for everything a Desktop owns.
//!
//! A Desktop's records book and its Geode key are not two configurations that
//! have to be kept in step. Both are derived from the Desktop id:
//!
//! ```text
//! records book   desktop/<id>/<name>
//! Geode address  geode://desktop/<id>/<slot>
//! VFS root       desktop/<id>/vfs
//! ```
//!
//! The three components mirror each other on purpose: the same id, in the same
//! position, so a key can never be filed under one Desktop and used by another.
//! `geode://` requires a realm, an account and a slot; `desktop` is the realm
//! and the Desktop id is the account.

use std::fmt;

use crate::{Invalid, valid_id, valid_name};

/// The derived address of one Desktop.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DesktopKey {
    id: String,
}

impl DesktopKey {
    pub fn parse(id: &str) -> Result<Self, Invalid> {
        if !valid_id(id) {
            return Err(Invalid::new(
                "desktop.id",
                "expected lowercase letters, digits or `-`",
            ));
        }
        Ok(Self { id: id.to_owned() })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The records book for one binding, e.g. `desktop/fpl/crm`.
    pub fn records_book(&self, name: &str) -> Result<String, Invalid> {
        if !valid_name(name) {
            return Err(Invalid::new(
                "records.name",
                "expected letters, digits, `-`, `_` or `.`",
            ));
        }
        Ok(format!("desktop/{}/{name}", self.id))
    }

    /// The Geode address for one key, e.g. `geode://desktop/fpl/records`.
    pub fn geode(&self, slot: &str) -> Result<String, Invalid> {
        if !valid_name(slot) {
            return Err(Invalid::new(
                "geode.slot",
                "expected letters, digits, `-`, `_` or `.`",
            ));
        }
        Ok(format!("geode://desktop/{}/{slot}", self.id))
    }

    /// The VFS root for this Desktop.
    #[must_use]
    pub fn vfs(&self) -> String {
        format!("desktop/{}/vfs", self.id)
    }
}

impl fmt::Display for DesktopKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_book_and_the_geode_address_name_the_same_desktop() {
        let key = DesktopKey::parse("fpl").unwrap();
        assert_eq!(key.records_book("crm").unwrap(), "desktop/fpl/crm");
        assert_eq!(key.geode("records").unwrap(), "geode://desktop/fpl/records");
        assert_eq!(key.vfs(), "desktop/fpl/vfs");
    }

    #[test]
    fn a_component_that_could_escape_the_address_is_refused() {
        assert!(DesktopKey::parse("../fpl").is_err());
        assert!(DesktopKey::parse("FPL").is_err());
        let key = DesktopKey::parse("fpl").unwrap();
        assert!(key.records_book("a/b").is_err());
        assert!(key.geode("..").is_err());
    }
}
