//! The person at this machine, as the kernel spells it.
//!
//! Every identity this host has is either *presented* (a signed actor from the
//! ingress) or *established* (a peer credential, a login). A desktop session is
//! the second kind and it is settled before any Spore starts: the process runs
//! as one uid, in one group set, for its whole life. So this module derives the
//! principal from the process instead of accepting one — nothing a launcher
//! passes can change who the window is signed in as, and a `soil` started from
//! `mush`, from `LaunchServices`, or from a terminal in the same session answers
//! the same principal.
//!
//! What it is *not*: an authorization check. A uid says who, never what. Exact
//! WIT grants and book grants still decide what that person may reach, exactly
//! as they do for a signed-in browser.

use crate::principal_host::{Principal, PrincipalKind};

/// The principal this process runs as, when the platform can say.
///
/// `None` on a platform with no POSIX identity to read, which a caller must
/// treat as "no principal" rather than inventing one.
pub fn local() -> Option<Principal> {
    #[cfg(unix)]
    {
        Some(unix_identity())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The actor string for the local user: the uid, because it is the one name
/// that cannot drift. The login name is presentation, and a rename must not
/// silently make someone a different author.
pub fn actor() -> Option<String> {
    local().map(|principal| principal.actor)
}

#[cfg(unix)]
fn unix_identity() -> Principal {
    use std::ffi::CStr;

    // SAFETY: `geteuid`/`getegid` take no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    // SAFETY: `getpwuid` returns a pointer into a static buffer that the next
    // libc identity call may overwrite; it is read to a Rust `String` here and
    // never held across another call.
    let name = unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() || (*entry).pw_name.is_null() {
            None
        } else {
            CStr::from_ptr((*entry).pw_name)
                .to_str()
                .ok()
                .map(str::to_owned)
        }
    };
    let mut teams = Vec::new();
    if let Some(name) = group_name(gid) {
        teams.push(name);
    }
    teams.extend(supplementary_groups(&name));
    teams.sort();
    teams.dedup();

    Principal {
        actor: format!("unix:{uid}"),
        kind: PrincipalKind::Human,
        display_name: name,
        device: hostname(),
        teams,
        roles: Vec::new(),
        // The generation changes when the identity behind it does. A uid does
        // not change and neither does a group set mid-session, so this is a
        // constant per process rather than something to poll.
        generation: 1,
    }
}

#[cfg(unix)]
fn group_name(gid: u32) -> Option<String> {
    use std::ffi::CStr;
    // SAFETY: as above — read into a `String` immediately.
    unsafe {
        let entry = libc::getgrgid(gid);
        if entry.is_null() || (*entry).gr_name.is_null() {
            return None;
        }
        CStr::from_ptr((*entry).gr_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// The supplementary groups of `user`, which is what a shared book would key on.
///
/// Failure is not an error: a process may be in no extra groups, and a host that
/// silently lost its group list would be worse than one that says it has none.
#[cfg(unix)]
fn supplementary_groups(user: &Option<String>) -> Vec<String> {
    let Some(user) = user else {
        return Vec::new();
    };
    let Ok(output) = std::process::Command::new("id")
        .arg("-Gn")
        .arg(user)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The machine this identity is bound to, so two principals with the same actor
/// are still distinguishable.
fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    #[cfg(unix)]
    {
        // SAFETY: `gethostname` writes at most `buffer.len()` bytes and
        // nul-terminates within them on success.
        let result = unsafe {
            libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len())
        };
        if result != 0 {
            return None;
        }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        return std::str::from_utf8(&buffer[..end]).ok().map(str::to_owned);
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_principal_is_the_process_it_runs_in() {
        let principal = local().expect("a POSIX identity");
        assert!(principal.actor.starts_with("unix:"), "{}", principal.actor);
        assert_eq!(principal.kind, PrincipalKind::Human);
        assert!(principal.display_name.is_some());
        // Two reads of the same process agree: this is not a session secret.
        assert_eq!(local().unwrap().actor, principal.actor);
    }
}
