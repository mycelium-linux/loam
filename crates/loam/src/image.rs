//! Image host planning.
//!
//! An image Spore is a live Soil image. Loam does not mutate the image; it
//! starts the Soil image runtime and gives it the WIT imports its grants and
//! installed providers allow. This module turns those two inputs into a plan
//! that is inspectable before a process is started.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    command::plugin::{PluginEntry, PluginRegistry},
};

/// One provider selected for an image import.
#[derive(Debug, Clone, Serialize)]
pub struct ImageProvider {
    pub id: String,
    pub version: String,
    pub package: PathBuf,
    pub module: PathBuf,
    pub module_sha256: String,
    pub lock_digest: String,
    pub sidecar: Option<PathBuf>,
    pub sidecar_sha256: Option<String>,
}

/// One granted WIT import and the provider that satisfies it.
#[derive(Debug, Clone, Serialize)]
pub struct ImageImport {
    pub interface: String,
    pub grant: String,
    pub provider: ImageProvider,
}

/// Why one imported interface did not make it into the plan.
#[derive(Debug, Clone, Serialize)]
pub struct ImageRefusal {
    pub interface: String,
    pub reason: String,
}

/// Everything Loam needs to start an image host without a string-dispatch
/// host-call table.
#[derive(Debug, Clone, Serialize)]
pub struct ImageHostPlan {
    pub spore_id: String,
    pub spore_version: String,
    pub package: PathBuf,
    pub grants: BTreeMap<String, String>,
    pub imports: Vec<ImageImport>,
    pub refused: Vec<ImageRefusal>,
}

impl ImageHostPlan {
    /// Build a plan from a Spore package and the installed provider registry.
    ///
    /// A module's non-WASI imported interfaces are the candidates. A grant that
    /// names an exact provider interface is also a candidate when the image has
    /// no compiled module. A candidate without a covering grant is refused; a
    /// granted candidate without a provider is refused. Nothing is linked by
    /// name alone.
    pub fn build(package: &Path, providers: &PluginRegistry) -> Result<Self> {
        let manifest = crate::spore::load_source(package)?;
        let mut candidates = std::collections::BTreeSet::new();
        let mut module_imports = false;
        if let Some(module) = &manifest.module {
            module_imports = true;
            let bytes = std::fs::read(package.join(&module.path))
                .with_context(|| format!("read image module {}", module.path))?;
            for interface in crate::spore::imported_interfaces(&bytes)? {
                if !interface.starts_with("wasi:") {
                    candidates.insert(interface);
                }
            }
        }
        if !module_imports {
            for grant in manifest.grants.keys() {
                if grant.contains('/') && providers.get(grant).is_some() {
                    candidates.insert(grant.clone());
                }
            }
        }

        let mut imports = Vec::new();
        let mut refused = Vec::new();
        for interface in candidates {
            let Some(grant) = granting_key(&manifest.grants, &interface) else {
                refused.push(ImageRefusal {
                    interface,
                    reason: "no covering [grants] entry".into(),
                });
                continue;
            };
            let Some(provider) = providers.get(&interface) else {
                refused.push(ImageRefusal {
                    reason: format!("no provider for {interface}"),
                    interface,
                });
                continue;
            };
            let provider = provider_digest(provider)?;
            imports.push(ImageImport {
                interface,
                grant: grant.to_owned(),
                provider,
            });
        }

        Ok(Self {
            spore_id: manifest.spore.id,
            spore_version: manifest.spore.version,
            package: package.to_path_buf(),
            grants: manifest.grants,
            imports,
            refused,
        })
    }

    /// Interfaces the image host is allowed to link.
    pub fn granted_interfaces(&self) -> Vec<&str> {
        self.imports
            .iter()
            .map(|import| import.interface.as_str())
            .collect()
    }
}

fn granting_key<'a>(grants: &'a BTreeMap<String, String>, interface: &str) -> Option<&'a str> {
    grants.keys().find_map(|grant| {
        let covered = grant == interface
            || interface.strip_prefix(grant).is_some_and(|rest| {
                rest.starts_with('/') || rest.starts_with('@') || rest.starts_with('.')
            });
        covered.then_some(grant.as_str())
    })
}

fn provider_digest(provider: &PluginEntry) -> Result<ImageProvider> {
    let module = std::fs::read(&provider.module)
        .with_context(|| format!("read provider module {}", provider.module.display()))?;
    let sidecar_sha256 = provider
        .sidecar_path
        .as_ref()
        .map(|path| {
            std::fs::read(path)
                .map(|bytes| sha256(&bytes))
                .with_context(|| format!("read provider sidecar {}", path.display()))
        })
        .transpose()?;
    Ok(ImageProvider {
        id: provider.id.clone(),
        version: provider.version.clone(),
        package: provider.package.clone(),
        module: provider.module.clone(),
        module_sha256: sha256(&module),
        lock_digest: provider.lock_digest.clone(),
        sidecar: provider.sidecar_path.clone(),
        sidecar_sha256,
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const PROVIDER: &str = r#"
        (component
          (core module $m (func (export "answer") (result i32) i32.const 42))
          (core instance $i (instantiate $m))
          (func $answer (result u32) (canon lift (core func $i "answer")))
          (instance $api (export "answer" (func $answer)))
          (export "soil:echo/echo@0.1.0" (instance $api))
        )
    "#;

    const IMAGE: &str = r#"
        (component
          (import "soil:echo/echo@0.1.0"
            (instance $api (export "answer" (func (result u32)))))
          (core func $answer (canon lower (func $api "answer")))
          (core module $m
            (import "" "answer" (func $answer (result i32)))
            (func (export "run") (result i32) call $answer)
          )
          (core instance $i
            (instantiate $m (with "" (instance (export "answer" (func $answer))))))
          (func (export "run") (result u32) (canon lift (core func $i "run")))
        )
    "#;

    fn package(root: &Path, name: &str, wat: &str, spore: &str) -> PathBuf {
        let package = root.join(name);
        fs::create_dir_all(package.join("signatures")).unwrap();
        fs::write(package.join("module.wasm"), wat::parse_str(wat).unwrap()).unwrap();
        fs::write(package.join("spore.toml"), spore).unwrap();
        let manifest = crate::spore::load_source(&package).unwrap();
        let lock = crate::spore::compute_lock(&package, &manifest).unwrap();
        fs::write(package.join("spore.lock"), lock.render()).unwrap();
        fs::write(package.join("signatures/test.asc"), b"test").unwrap();
        package
    }

    fn provider(root: &Path) -> PathBuf {
        package(
            root,
            "provider",
            PROVIDER,
            "schema = 1\n[spore]\nid = \"test.echo\"\nname = \"echo\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\nsummary = \"provider\"\n[engine]\nsoil = \">=0.1\"\ntarget = \"wasm32-wasip2\"\n[module]\npath = \"module.wasm\"\n[launch]\ndefault = \"canvas\"\nsurfaces = [\"canvas\"]\n",
        )
    }

    fn image(root: &Path, grants: &str) -> PathBuf {
        package(
            root,
            "image",
            IMAGE,
            &format!(
                "schema = 1\n[spore]\nid = \"test.image\"\nname = \"image\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\nsummary = \"image\"\n[engine]\nsoil = \">=0.1\"\ntarget = \"wasm32-wasip2\"\n[module]\npath = \"module.wasm\"\n[launch]\ndefault = \"canvas\"\nsurfaces = [\"canvas\"]\n{grants}"
            ),
        )
    }

    #[test]
    fn plans_a_granted_provider_and_its_digest() {
        let providers = tempfile::tempdir().unwrap();
        provider(providers.path());
        let registry = PluginRegistry::scan_roots(&[providers.path().to_path_buf()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let image = image(directory.path(), "[grants]\n\"soil:echo\" = \"echo\"\n");
        let plan = ImageHostPlan::build(&image, &registry).unwrap();
        assert_eq!(plan.imports.len(), 1);
        assert_eq!(plan.imports[0].interface, "soil:echo/echo@0.1.0");
        assert_eq!(plan.imports[0].provider.id, "test.echo");
        assert_eq!(plan.imports[0].provider.module_sha256.len(), 64);
        assert!(plan.refused.is_empty());
    }

    #[test]
    fn refuses_an_import_without_a_covering_grant() {
        let providers = tempfile::tempdir().unwrap();
        provider(providers.path());
        let registry = PluginRegistry::scan_roots(&[providers.path().to_path_buf()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let image = image(directory.path(), "");
        let plan = ImageHostPlan::build(&image, &registry).unwrap();
        assert!(plan.imports.is_empty());
        assert!(plan.refused[0].reason.contains("grants"));
    }
}
