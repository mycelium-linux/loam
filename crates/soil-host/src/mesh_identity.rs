//! This node's Mycelium enrolment, as a TLS client identity.
//!
//! A control plane on the mesh authenticates its callers by certificate: every
//! enrolled node already holds `pki/node.pem`, its key and the CA that signed
//! it. A Spore cannot hold those, and must not be able to — so the host holds
//! them and presents them on the Spore's behalf, which is the same arrangement
//! as every other credential in this crate.
//!
//! The guest never names a key, a path, or a password. It gets the effect of an
//! identity it cannot read, which is the only version of this that is safe to
//! hand to wasm.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};

/// Where Mycelium installs an enrolment: `$MYCELIUM_HOME`, else `~/.mycelium`.
pub fn home() -> PathBuf {
    if let Some(path) = std::env::var_os("MYCELIUM_HOME") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".mycelium")
}

/// Trust the mesh CA, present this node's certificate.
pub fn client_config() -> Result<Arc<ClientConfig>> {
    let home = home();
    let certificate = home.join("pki/node.pem");
    let key = home.join("pki/node-key.pem");
    let ca = home.join("pki/ca.pem");
    for path in [&certificate, &key, &ca] {
        if !path.is_file() {
            bail!(
                "{} is missing: this node has no mesh identity to present. Enrol it \
                 (`mycelium setup --claim …`) or point MYCELIUM_HOME at the home it was \
                 enrolled under",
                path.display()
            );
        }
    }
    let mut roots = RootCertStore::empty();
    for root in load_certificates(&ca)? {
        roots
            .add(root)
            .with_context(|| format!("{} is not a usable CA", ca.display()))?;
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(load_certificates(&certificate)?, load_key(&key)?)
        .with_context(|| format!("load {}", certificate.display()))?;
    Ok(Arc::new(config))
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse certificates in {}", path.display()))?;
    if certificates.is_empty() {
        bail!("{} holds no PEM certificate", path.display());
    }
    Ok(certificates)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("parse the private key in {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("{} holds no private key", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unenrolled_node_is_told_what_is_missing() {
        // No MYCELIUM_HOME in the test process, so this is the real home; point
        // the lookup at a directory that cannot hold an enrolment.
        unsafe { std::env::set_var("MYCELIUM_HOME", "/nonexistent-mycelium") };
        let error = client_config().unwrap_err().to_string();
        unsafe { std::env::remove_var("MYCELIUM_HOME") };
        assert!(error.contains("mesh identity"), "{error}");
    }

    #[test]
    fn the_home_follows_the_environment() {
        unsafe { std::env::set_var("MYCELIUM_HOME", "/tmp/mesh-test") };
        let home = home();
        unsafe { std::env::remove_var("MYCELIUM_HOME") };
        assert_eq!(home, PathBuf::from("/tmp/mesh-test"));
    }
}
