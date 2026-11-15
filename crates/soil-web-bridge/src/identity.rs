//! Who is looking, and how the bridge comes to believe it.
//!
//! A browser cannot hold a certificate and the gateway is where OIDC terminates,
//! so the bridge never sees a login. Palisade (the FPL ingress) authenticates a
//! Vouch session and then either forwards identity headers or — for routes that
//! ask for it — a *signed actor*:
//!
//! ```text
//! x-fpl-actor:           <base64url {"sub","email","source","iat","exp"}>
//! x-fpl-actor-signature: v1=<base64url HMAC-SHA256(secret, payload)>
//! ```
//!
//! The signed actor is the one this bridge prefers: a header alone is a claim,
//! a MAC with a shared secret and an expiry is a statement the ingress made, and
//! it keeps working behind an ingress that is not on loopback. Plain headers are
//! still accepted for a local session, where there is nothing to forge across
//! and no login to have.
//!
//! What comes out is a [`Principal`]: the projection `soil:principal` defines,
//! so a Spore sees the same shape whether the identity came from a MAC, a
//! loopback header, or (one day) a Mycelium grant.

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Actor headers, spelled exactly as Palisade sets them.
pub const ACTOR: &str = "x-fpl-actor";
pub const ACTOR_SIGNATURE: &str = "x-fpl-actor-signature";
pub const USER_EMAIL: &str = "x-fpl-user-email";
pub const USER_SCOPES: &str = "x-fpl-user-scopes";
pub const AUTH_METHOD: &str = "x-fpl-auth-method";

/// A verified caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The durable user key. Palisade does not mint a separate subject, and its
    /// doc says so: key authorization and audit off the email.
    pub actor: String,
    /// Host-mapped roles, in the gateway's scope vocabulary.
    pub scopes: Vec<String>,
    /// How it was established: `vouch` for a signed actor, `loopback` for a
    /// header read on a local connection.
    pub method: String,
}

impl Identity {
    /// The name a per-principal book is keyed by: stable, filesystem-safe, and
    /// visibly derived from the actor rather than from a counter.
    pub fn slug(&self) -> String {
        let mut slug = String::with_capacity(self.actor.len());
        for character in self.actor.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character.to_ascii_lowercase());
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-').to_owned();
        if slug.is_empty() {
            "anonymous".to_owned()
        } else {
            slug
        }
    }
}

/// Why an identity claim was not believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The request carried no identity at all and this bridge requires one.
    Missing,
    /// The signature did not match the shared secret.
    BadSignature,
    /// The actor statement has expired, or was never dated.
    Stale,
    /// The actor statement could not be read.
    Malformed,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "no identity: sign in through the gateway",
            Self::BadSignature => "the actor signature does not verify",
            Self::Stale => "the actor statement has expired",
            Self::Malformed => "the actor statement is unreadable",
        }
    }
}

/// What a bridge is willing to believe, and how it checks.
#[derive(Debug, Clone, Default)]
pub struct Trust {
    /// The shared secret Palisade signs actors with. When set, a signed actor
    /// is the only thing accepted: a bare header would be a downgrade.
    pub actor_secret: Option<Vec<u8>>,
    /// Accept `x-fpl-user-email` from a loopback peer. Local development, where
    /// the ingress is not in the path and there is nothing to forge across.
    pub trust_loopback_headers: bool,
    /// Answer a loopback request with no identity as the person at this machine.
    /// A desktop session has one user, so a tab on the same machine is them —
    /// the same reasoning that lets the window run without a login.
    pub local_fallback: bool,
}

impl Trust {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify actors signed with `secret`.
    pub fn actor_secret(mut self, secret: impl Into<Vec<u8>>) -> Self {
        self.actor_secret = Some(secret.into());
        self
    }

    /// Take the identity headers at face value from a loopback peer.
    pub fn trust_loopback(mut self, trust: bool) -> Self {
        self.trust_loopback_headers = trust;
        self
    }

    /// Answer an anonymous loopback request as the local user.
    pub fn local_fallback(mut self, fallback: bool) -> Self {
        self.local_fallback = fallback;
        self
    }

    /// The caller, or why there is not one.
    ///
    /// `Ok(None)` means this bridge was told not to require identity at all and
    /// nobody presented one; every caller that does present one is verified.
    pub fn identify(
        &self,
        headers: &BTreeMap<String, String>,
        loopback: bool,
    ) -> Result<Option<Identity>, Refusal> {
        if let Some(secret) = &self.actor_secret {
            let Some(payload) = headers.get(ACTOR) else {
                // No signed actor: fine only if this host never asked for one.
                return if self.trust_loopback_headers && loopback {
                    Ok(None)
                } else {
                    Err(Refusal::Missing)
                };
            };
            let signature = headers.get(ACTOR_SIGNATURE).ok_or(Refusal::Missing)?;
            let claims = verify_actor(secret, payload, signature)?;
            return Ok(Some(Identity {
                actor: claims.actor,
                scopes: claims.scopes,
                method: "vouch".to_owned(),
            }));
        }
        let Some(email) = headers.get(USER_EMAIL).filter(|email| !email.is_empty()) else {
            return Ok(local(loopback, self.local_fallback));
        };
        if !loopback {
            // A header is a claim. Only a peer we can name may make it.
            return Err(Refusal::BadSignature);
        }
        Ok(Some(Identity {
            actor: email.clone(),
            scopes: headers
                .get(USER_SCOPES)
                .map(|scopes| scopes.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            method: headers
                .get(AUTH_METHOD)
                .cloned()
                .unwrap_or_else(|| "loopback".to_owned()),
        }))
    }

    /// Does this bridge insist on an identity?
    pub fn required(&self) -> bool {
        self.actor_secret.is_some()
    }
}

/// The person at this machine, when this bridge may fall back to them.
///
/// A desktop session has one user, so an unsigned request from loopback is
/// that user — the same reasoning that lets the window run without a login.
/// It is deliberately not a grant: a book still has to be keyed to the
/// principal, and a bridge that expects a signed actor never gets here.
fn local(loopback: bool, fallback: bool) -> Option<Identity> {
    if !(loopback && fallback) {
        return None;
    }
    let principal = soil::local_identity::local()?;
    Some(Identity {
        actor: principal.display_name.unwrap_or(principal.actor),
        scopes: principal.teams,
        method: "local".to_owned(),
    })
}

/// The claims Palisade signs.
#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    iat: u64,
    #[serde(default)]
    exp: u64,
}

struct Actor {
    actor: String,
    scopes: Vec<String>,
}

/// Check `v1=<base64url HMAC-SHA256(secret, payload)>` and the statement's time.
fn verify_actor(secret: &[u8], payload: &str, signature: &str) -> Result<Actor, Refusal> {
    let encoded = signature
        .strip_prefix("v1=")
        .ok_or(Refusal::Malformed)?;
    let presented = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|_| Refusal::Malformed)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| Refusal::Malformed)?;
    mac.update(payload.as_bytes());
    // `verify_slice` is constant-time and refuses a length mismatch, so a
    // truncated signature is a refusal rather than a panic.
    mac.verify_slice(&presented)
        .map_err(|_| Refusal::BadSignature)?;

    let claims: Claims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload.trim())
            .map_err(|_| Refusal::Malformed)?,
    )
    .map_err(|_| Refusal::Malformed)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    if claims.exp == 0 || claims.exp < now {
        return Err(Refusal::Stale);
    }
    // A statement dated in the future is a clock problem, not an identity.
    if claims.iat > now + 60 {
        return Err(Refusal::Stale);
    }
    // A shared secret can sign anything. The statement says who signed it, and
    // a bridge that expects the ingress should not accept another signer's word
    // for the same secret.
    if !claims.source.is_empty() && claims.source != "palisade" {
        return Err(Refusal::Malformed);
    }
    let actor = if claims.email.is_empty() {
        claims.sub
    } else {
        claims.email
    };
    if actor.is_empty() {
        return Err(Refusal::Malformed);
    }
    Ok(Actor {
        actor,
        scopes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], claims: serde_json::Value) -> (String, String) {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(payload.as_bytes());
        let signature = format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()));
        (payload, signature)
    }

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn a_signed_actor_becomes_a_principal() {
        let (payload, signature) = sign(
            b"sekrit",
            serde_json::json!({
                "sub": "alice@example.com",
                "email": "alice@example.com",
                "source": "palisade",
                "iat": now(),
                "exp": now() + 300,
            }),
        );
        let trust = Trust::new().actor_secret(b"sekrit");
        let identity = trust
            .identify(
                &headers(&[(ACTOR, &payload), (ACTOR_SIGNATURE, &signature)]),
                false,
            )
            .expect("verified")
            .expect("an identity");
        assert_eq!(identity.actor, "alice@example.com");
        assert_eq!(identity.method, "vouch");
        assert_eq!(identity.slug(), "alice-example-com");
    }

    #[test]
    fn a_forged_actor_is_refused() {
        let (payload, _) = sign(
            b"other",
            serde_json::json!({ "email": "mallory@example.com", "iat": now(), "exp": now() + 300 }),
        );
        let (_, signature) = sign(
            b"sekrit",
            serde_json::json!({ "email": "alice@example.com", "iat": now(), "exp": now() + 300 }),
        );
        let trust = Trust::new().actor_secret(b"sekrit");
        assert_eq!(
            trust.identify(
                &headers(&[(ACTOR, &payload), (ACTOR_SIGNATURE, &signature)]),
                true
            ),
            Err(Refusal::BadSignature)
        );
    }

    #[test]
    fn an_expired_actor_is_refused() {
        let (payload, signature) = sign(
            b"sekrit",
            serde_json::json!({ "email": "alice@example.com", "iat": now() - 900, "exp": now() - 600 }),
        );
        let trust = Trust::new().actor_secret(b"sekrit");
        assert_eq!(
            trust.identify(
                &headers(&[(ACTOR, &payload), (ACTOR_SIGNATURE, &signature)]),
                true
            ),
            Err(Refusal::Stale)
        );
    }

    #[test]
    fn a_bare_header_is_only_believed_on_loopback() {
        let trust = Trust::new();
        let claim = headers(&[(USER_EMAIL, "alice@example.com"), (USER_SCOPES, "docs write")]);
        let local = trust.identify(&claim, true).unwrap().unwrap();
        assert_eq!(local.actor, "alice@example.com");
        assert_eq!(local.scopes, vec!["docs".to_owned(), "write".to_owned()]);
        assert_eq!(trust.identify(&claim, false), Err(Refusal::BadSignature));
    }
}
