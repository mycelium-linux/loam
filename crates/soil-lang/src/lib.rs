// SPDX-License-Identifier: MIT OR Apache-2.0
//! Soil's Lisp: the S-expression reader, interpreter, typed core, WIT bindings,
//! and Wasm backends.

pub mod core;
#[cfg(feature = "witgen")]
pub mod dispatchgen;
pub mod eval;
pub mod lisp;
#[cfg(feature = "native")]
pub mod native_host;
pub mod sexp;
#[cfg(feature = "native")]
pub mod unibus_host;
pub mod wasm;
#[cfg(feature = "witgen")]
pub mod witgen;

pub use core::{Expr, Function, Program, parse};
#[cfg(feature = "witgen")]
pub use dispatchgen::{GeneratedDispatch, generate as generate_dispatch};
pub use eval::{
    FakeHost, Host, HostChain, Runtime, json_to_value as from_json, value_to_json as to_json,
};
pub use lisp::{Compiled as CompiledLisp, compile_source as compile_lisp};
#[cfg(feature = "native")]
pub use native_host::NativeHost;
pub use sexp::{Comment, Document, Form, Span, parse_document, print_document};
#[cfg(feature = "native")]
pub use soil::time::SystemTimeHost;
#[cfg(feature = "native")]
pub use unibus_host::UnibusHost;
pub use wasm::compile;
#[cfg(feature = "witgen")]
pub use witgen::{Generated, generate};
