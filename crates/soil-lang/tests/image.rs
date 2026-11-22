// SPDX-License-Identifier: MIT OR Apache-2.0
//! Worktree-scoped image provenance and VFS flush.

use std::path::Path;

use soil_lang::sexp::Value;
use soil_lang::{Host, NativeHost, Runtime};

fn prelude() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    dir.join("stdlib/prelude.soil").display().to_string()
}

#[test]
fn image_flushes_dirty_definitions_back_to_vfs() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let mut host = NativeHost::open(&workspace, &records, "soil").unwrap();
    host.write_source(
        "lib/hello.soil",
        "(defun greet (name) (str \"hello \" name))\n",
    )
    .unwrap();

    let mut runtime = Runtime::new(Box::new(host));
    runtime.load_file(prelude()).unwrap();
    runtime.clear_provenance();
    runtime.load_vfs("lib/hello.soil").unwrap();
    assert!(runtime.dirty_definitions().is_empty());

    runtime
        .load("(defun greet (name) (str \"hi \" name))")
        .unwrap();
    assert_eq!(runtime.dirty_definitions(), vec!["greet".to_string()]);

    let report = runtime.flush_preview().unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].source.contains("hi "));

    let report = runtime.flush_apply().unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert!(runtime.dirty_definitions().is_empty());

    let greeting = runtime
        .call("greet", &[Value::String("soil".into())])
        .unwrap();
    assert_eq!(greeting, Value::String("hi soil".into()));
}

#[test]
fn image_append_unattached_definition_to_vfs() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let host = NativeHost::open(&workspace, &records, "soil").unwrap();
    let mut runtime = Runtime::new(Box::new(host));
    runtime.load_file(prelude()).unwrap();
    runtime.clear_provenance();
    runtime.load("(defun scratch (x) (+ x 1))").unwrap();
    assert_eq!(
        runtime.unattached_definitions(),
        vec!["scratch".to_string()]
    );

    let file = runtime
        .flush_unattached("scratch.soil", &["scratch".to_string()])
        .unwrap();
    assert!(file.source.contains("(defun scratch"));
    assert!(runtime.unattached_definitions().is_empty());
}

#[test]
fn image_detects_a_vfs_change_and_reloads_it() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let mut host = NativeHost::open(&workspace, &records, "soil").unwrap();
    host.write_source("lib/hello.soil", "(defun greet () \"first\")\n")
        .unwrap();
    let mut runtime = Runtime::new(Box::new(host));
    runtime.load_file(prelude()).unwrap();
    runtime.clear_provenance();
    runtime.load_vfs("lib/hello.soil").unwrap();
    let before = runtime.source_hash("lib/hello.soil").unwrap();
    assert_eq!(
        runtime.call("greet", &[]).unwrap(),
        Value::String("first".into())
    );

    let mut writer = NativeHost::open(&workspace, &records, "soil").unwrap();
    writer
        .write_source("lib/hello.soil", "(defun greet () \"second\")\n")
        .unwrap();
    let after = runtime.source_hash("lib/hello.soil").unwrap();
    assert_ne!(before, after);

    runtime.load_vfs("lib/hello.soil").unwrap();
    assert_eq!(
        runtime.call("greet", &[]).unwrap(),
        Value::String("second".into())
    );
}
