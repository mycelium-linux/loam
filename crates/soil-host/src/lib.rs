//! Host implementations of the `soil:*` and `fungos:*` capability contracts.
//!
//! One module per contract: `vfs_host` serves `soil:vfs`, `soil:vfs-mut` and
//! `soil:index` over `fungos-vfs`; `http_host` serves `soil:http`;
//! `records_host` serves `soil:records`; `unibus` serves `soil:unibus`;
//! `mycelium` serves `fungos:mycelium/fleet`. `wasi` is the guest profile that
//! links them into one [`wasi::WasiHost`].
//!
//! They live in one crate because they are one host. A binary that links the
//! shell links all of them, and a crate per contract only bought a `Cargo.toml`,
//! a duplicated error type and a binary per experiment — the leaf-crate shape
//! this replaces.
//!
//! `mycelium` and `unibus` are behind features, on by default in the product,
//! because they front deployments a product may not ship. `vfs_host` and
//! `records_host` are fundamental: a Spore has a home directory and a store, so
//! there is no product that ships without them.

pub mod http_host;
pub mod mesh_identity;
#[cfg(feature = "mycelium")]
pub mod mycelium;
pub mod records_host;
#[cfg(feature = "unibus")]
pub mod unibus;
pub mod vfs_host;
pub mod wasi;

pub use http_host::HttpHost;
pub use records_host::RecordsBookHost;
pub use vfs_host::VfsHost;
