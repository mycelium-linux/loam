//! Bring a bridge up from a Spore package.
//!
//! This is the one thing the `soil` runner needs from a bridge: open the book
//! the Spore was granted, keep the manifest's `[grants]` table as the whole
//! policy, and hand back a listening server. Printing and parking belong to the
//! caller, which is why nothing here writes to stdout.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::bindings::{Bindings, Fixed, PerPrincipal};
use crate::{Auth, Bridge, GrantSet, RecordsCapability, Registry, Server};

/// One book the bridge serves as one guest-facing collection.
#[derive(Debug, Clone)]
pub struct Records {
    /// Root of the `fungos-records` store the bridge reads.
    pub root: PathBuf,
    /// Book served as this bridge's collection.
    pub book: String,
    /// Collection a client must name in `key.collection`.
    pub collection: String,
}

/// How this bridge decides who is looking, and what a looker may see.
#[derive(Debug, Clone, Default)]
pub struct Access {
    /// Where identity comes from: a signed actor, or a header we only believe
    /// from a loopback peer.
    pub trust: crate::Trust,
    /// Keep one book per principal instead of one book for the process.
    pub per_principal: bool,
}

impl Access {
    /// A desktop bridge: loopback, one user, books per principal.
    ///
    /// This is what a window and a tab on the same machine should agree on —
    /// the person sitting here — so a local Spore does not need a login to know
    /// who is commenting.
    pub fn desktop() -> Self {
        Self {
            trust: crate::Trust::new().trust_loopback(true).local_fallback(true),
            per_principal: true,
        }
    }
}

/// What a bridge was asked to serve.
#[derive(Debug, Clone)]
pub struct Options {
    /// Directory containing the Spore's `spore.toml`.
    pub spore: PathBuf,
    /// The records book, when this Spore was granted `soil:records`. A Spore
    /// that asks for no records still gets a bridge: an empty registry refuses
    /// that call instead of the whole surface.
    pub records: Option<Records>,
    /// Who is looking, and whether their books follow them.
    pub access: Access,
    pub bind: SocketAddr,
    /// Bearer token. Without one, only loopback clients are accepted.
    pub token: Option<String>,
    /// Origins allowed to script a session.
    pub origins: Vec<String>,
}

/// A running bridge, and what a caller should say about it.
pub struct Served {
    pub spore: String,
    pub collection: Option<String>,
    pub book: Option<String>,
    pub token: bool,
    pub origins: Vec<String>,
    /// Whether a book was kept per principal rather than per process.
    pub per_principal: bool,
    /// The server stops when this is dropped.
    pub server: Server,
}

pub fn serve(options: Options) -> anyhow::Result<Served> {
    if !options.bind.ip().is_loopback() && options.token.is_none() {
        bail!(
            "{} is not loopback; a non-loopback bridge needs a token \
             (or put it behind the gateway and keep this listener on loopback)",
            options.bind
        );
    }

    let manifest = soil_spore::Manifest::load(&options.spore).with_context(|| {
        format!(
            "read {}",
            options.spore.join("spore.toml").display()
        )
    })?;
    let spore = manifest.spore.id.clone();
    let grants = GrantSet::from_manifest(&manifest);

    let records = match &options.records {
        Some(records) => Some(
            RecordsCapability::open(&records.root, &records.book, &records.collection)
                .with_context(|| {
                    format!(
                        "open records book {:?} at {}",
                        records.book,
                        records.root.display()
                    )
                })?,
        ),
        None => None,
    };
    let bindings: Arc<dyn Bindings> = match (&options.records, options.access.per_principal) {
        // A book per principal, opened when that principal first calls.
        (Some(records), true) => Arc::new(PerPrincipal::new(
            &records.root,
            &records.book,
            &records.collection,
        )),
        _ => {
            let mut registry = Registry::new();
            if let Some(records) = records {
                registry = registry.with(records);
            }
            Arc::new(Fixed(Arc::new(registry)))
        }
    };

    let mut auth = match &options.token {
        Some(token) => Auth::with_token(token),
        None => Auth::loopback_only(),
    };
    for origin in &options.origins {
        auth = auth.allow_origin(origin.clone());
    }

    // A browser host runs the guest, so it has to be handed the guest: the
    // manifest projection (which imports to provide) and the module bytes.
    let module = manifest
        .module
        .as_ref()
        .and_then(|module| std::fs::read(options.spore.join(&module.path)).ok());
    let bridge = Arc::new(
        Bridge::new(spore.clone(), grants, Registry::new())
            .with_bindings(bindings)
            .with_trust(options.access.trust.clone())
            .with_auth(auth)
            .with_package(manifest.info_json(), module),
    );
    let server = bridge.serve(options.bind)?;
    Ok(Served {
        spore,
        collection: options.records.as_ref().map(|records| records.collection.clone()),
        book: options.records.as_ref().map(|records| records.book.clone()),
        token: options.token.is_some(),
        origins: options.origins,
        per_principal: options.access.per_principal,
        server,
    })
}
