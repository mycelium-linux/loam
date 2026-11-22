// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "engine")]
use wasmtime::{Engine, Instance, Module, Store, Val};

fn wasm(source: &str) -> Vec<u8> {
    let program = soil_lang::parse(source).expect("parse");
    soil_lang::compile(&program).expect("compile")
}

fn call(source: &str, name: &str, args: &[i64]) -> i64 {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm(source)).expect("module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instance");
    let function = instance.get_func(&mut store, name).expect("function");
    let params: Vec<Val> = args.iter().copied().map(Val::I64).collect();
    let mut results = [Val::I64(0)];
    function
        .call(&mut store, &params, &mut results)
        .expect("call");
    match results[0] {
        Val::I64(value) => value,
        other => panic!("unexpected result {other:?}"),
    }
}

#[test]
fn compiles_and_runs_arithmetic() {
    assert_eq!(call("(defun add (a b) (+ a b))", "add", &[2, 3]), 5);
}

#[test]
fn compiles_let_if_and_comparisons() {
    let source = r#"
        (defun mix (a b)
          (let ((x (+ a b))
                (y (* x 2)))
            (+ x y)))
        (defun less (a b)
          (if (< a b) 1 0))
    "#;
    assert_eq!(call(source, "mix", &[2, 3]), 15);
    assert_eq!(call(source, "less", &[2, 3]), 1);
    assert_eq!(call(source, "less", &[3, 2]), 0);
}

#[test]
fn compiles_cond_and_boolean_forms() {
    let source = r#"
        (defun classify (charging linked stale hr)
          (cond
            (charging -1)
            ((not linked) -2)
            (stale -3)
            ((and hr (not (= hr 0))) 9)
            (else 0)))
    "#;
    assert_eq!(call(source, "classify", &[1, 1, 0, 1]), -1);
    assert_eq!(call(source, "classify", &[0, 0, 0, 1]), -2);
    assert_eq!(call(source, "classify", &[0, 1, 1, 1]), -3);
    assert_eq!(call(source, "classify", &[0, 1, 0, 1]), 9);
    assert_eq!(call(source, "classify", &[0, 1, 0, 0]), 0);
}

#[test]
fn matches_the_interpreter_on_the_numeric_core() {
    let source = r#"
        (defun classify (charging linked stale hr)
          (cond
            (charging -1)
            ((not linked) -2)
            (stale -3)
            ((and hr (not (= hr 0))) 9)
            (else 0)))
    "#;
    let mut runtime = soil_lang::Runtime::new(Box::new(soil_lang::FakeHost::new()));
    runtime.load(source).expect("load interpreter");

    // The numeric core represents booleans as 1/0. The interpreter keeps
    // `#t`/`#f` distinct from numbers, so differential cases pass booleans.
    let cases: &[([bool; 4], [i64; 4])] = &[
        ([true, true, false, true], [1, 1, 0, 1]),
        ([false, false, false, true], [0, 0, 0, 1]),
        ([false, true, true, true], [0, 1, 1, 1]),
        ([false, true, false, true], [0, 1, 0, 1]),
        ([false, true, false, false], [0, 1, 0, 0]),
    ];

    for (values, numbers) in cases {
        let interpreted = runtime
            .call(
                "classify",
                &values
                    .iter()
                    .copied()
                    .map(soil_lang::sexp::Value::Bool)
                    .collect::<Vec<_>>(),
            )
            .expect("interpret");
        let compiled = call(source, "classify", numbers);
        assert_eq!(
            interpreted,
            soil_lang::sexp::Value::Int(compiled),
            "args {values:?}"
        );
    }
}

#[test]
fn compiles_defconst_values_into_the_numeric_core() {
    let source = r#"
        (defconst LIMIT 3)
        (defconst DERIVED (+ LIMIT 1))
        (defun at-limit () LIMIT)
        (defun derived () DERIVED)
        (defun under (x) (< x LIMIT))
    "#;
    assert_eq!(call(source, "at-limit", &[]), 3);
    assert_eq!(call(source, "derived", &[]), 4);
    assert_eq!(call(source, "under", &[2]), 1);
    assert_eq!(call(source, "under", &[4]), 0);
}

#[test]
fn rejects_values_outside_the_numeric_core() {
    let error = soil_lang::parse("(defun f () (list 1 2))").unwrap_err();
    assert!(error.to_string().contains("unknown function `list`"));
}
