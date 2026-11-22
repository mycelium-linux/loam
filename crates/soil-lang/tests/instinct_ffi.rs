// SPDX-License-Identifier: MIT OR Apache-2.0
use std::path::Path;

use soil_lang::sexp::Value;
use soil_lang::{NativeHost, Runtime};

#[test]
fn instinct_runs_as_soil_over_the_records_wit() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let host = NativeHost::open(&workspace, root.path(), "soil").unwrap();
    let mut runtime = Runtime::new(Box::new(host));

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    runtime
        .load_file(dir.join("bindings/soil-records.soil"))
        .unwrap();
    runtime
        .load_file(dir.join("bindings/soil-time.soil"))
        .unwrap();
    runtime.load_file(dir.join("stdlib/prelude.soil")).unwrap();
    runtime
        .load_file(dir.join("examples/seed_instinct.soil"))
        .unwrap();
    runtime
        .load_file(dir.join("examples/instinct.soil"))
        .unwrap();

    let result = runtime.call("main", &[]).unwrap();
    let text = result.to_string();
    assert!(text.contains("(:worn 0.9)"), "{text}");
    assert!(text.contains("(:worn 0.8)"), "{text}");
    assert!(text.contains("(:worn 0.85)"), "{text}");
    assert!(text.contains("(:in-use 0.85)"), "{text}");
    assert!(text.contains("(:present 0.9)"), "{text}");
    assert!(matches!(result, Value::List(_)));
}
