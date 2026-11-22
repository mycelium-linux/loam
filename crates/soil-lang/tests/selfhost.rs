// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Soil-written numeric-core compiler must emit the same Wasm as the
//! Rust reference compiler.

use std::path::Path;

use soil_lang::sexp::Value;
use soil_lang::{FakeHost, Runtime};

fn compiler() -> Runtime {
    let mut runtime = Runtime::new(Box::new(FakeHost::new()));
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    runtime
        .load_file(root.join("stdlib/prelude.soil"))
        .expect("prelude");
    runtime
        .load_file(root.join("../../examples/compiler/core.soil"))
        .expect("compiler");
    runtime.clear_provenance();
    runtime
}

fn bytes(value: Value) -> Vec<u8> {
    let Value::List(values) = value else {
        panic!("compiler did not return a byte list: {value:?}");
    };
    values
        .into_iter()
        .map(|value| match value {
            Value::Int(byte) if (0..=255).contains(&byte) => byte as u8,
            other => panic!("not a byte: {other:?}"),
        })
        .collect()
}

fn check(source: &str) {
    let mut runtime = compiler();
    let produced = bytes(
        runtime
            .call(
                "compiler/compile-wasm",
                &[Value::String(source.to_string())],
            )
            .expect("soil compiler"),
    );
    let reference = soil_lang::compile(&soil_lang::parse(source).expect("parse")).expect("compile");
    assert_eq!(
        produced,
        reference,
        "soil and rust compilers disagree on {source}"
    );
}

#[test]
fn matches_the_reference_compiler() {
    check("(defun add (a b) (+ a b))");
    check("(defun less (a b) (if (< a b) 1 0))");
    check("(defun twice (x) (+ x x)) (defun four () (twice (twice 1)))");
    check("(defconst LIMIT 3) (defun under (x) (< x LIMIT))");
    check("(defun mix (a b) (let ((x (+ a b)) (y (* x 2))) (+ x y)))");
    check("(defun classify (a b) (cond ((< a b) 1) (else 0)))");
    check("(defun both (a b) (and (not (= a 0)) (or (= b 1) (= b 2))))");
}
