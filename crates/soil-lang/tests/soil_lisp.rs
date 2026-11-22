// SPDX-License-Identifier: MIT OR Apache-2.0
//! One Rust harness for tests written in Soil.
//!
//! Each `tests/soil/*.soil` file may define functions named `test/...`.
//! The harness loads the common prelude/bindings, calls every test, and treats
//! any nil/false result as a failure.

use std::path::{Path, PathBuf};

use soil_lang::sexp::Value;
use soil_lang::{Host, NativeHost, Runtime, parse_document};

fn soil_tests() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/soil");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("tests/soil")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("soil"))
        .collect();
    files.sort();
    files
}

fn runtime(root: &Path) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("hello.txt"), b"hello from vfs\n").unwrap();
    let host = NativeHost::open(&workspace, root, "soil").unwrap();
    let mut runtime = Runtime::new(Box::new(MockHttpHost { inner: host }));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "stdlib/prelude.soil",
        "stdlib/records.soil",
        "stdlib/perf.soil",
        "stdlib/ui.soil",
        "stdlib/highlight.soil",
        "stdlib/doc.soil",
        "stdlib/cli.soil",
        "bindings/wasi-cli.generated.soil",
        "bindings/soil-time.soil",
        "bindings/soil-records.soil",
        "bindings/soil-http.soil",
        "bindings/wasi-cli.soil",
        "../../examples/hue/hue.soil",
        "../../examples/hue/hue_cli.soil",
        "../../examples/playback/playback.soil",
        "../../examples/playback/playback_cli.soil",
        "../../examples/home/home.soil",
        "../../examples/home/ui.soil",
        "examples/frigate_automation.soil",
        "boot/boot.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime
}

fn test_names(source: &str) -> Vec<String> {
    parse_document(source)
        .expect("parse Soil test")
        .forms
        .into_iter()
        .filter_map(|form| {
            let items = form.value.as_list()?;
            if items.first().and_then(Value::as_symbol) != Some("defun") {
                return None;
            }
            let name = items.get(1).and_then(Value::as_symbol)?;
            name.starts_with("test/").then(|| name.to_string())
        })
        .collect()
}

/// A scoped `soil:http` host that echoes the request as JSON.
///
/// The test assertions live in `tests/soil/*.soil`; this only supplies a
/// deterministic transport so a Soil test can read the URL, method and body a
/// capability would have sent.
struct MockHttpHost {
    inner: NativeHost,
}

impl Host for MockHttpHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        (interface == "soil:http" && function == "fetch") || self.inner.granted(interface, function)
    }

    fn call(&mut self, interface: &str, function: &str, args: &[Value]) -> Result<Value, String> {
        if interface == "soil:http" && function == "fetch" {
            return echo_http(args);
        }
        self.inner.call(interface, function, args)
    }
}

fn echo_http(args: &[Value]) -> Result<Value, String> {
    let request = args.first().ok_or("soil:http/fetch needs a request")?;
    let url = record_text(request, "url").unwrap_or_default();
    let method = record_text(request, "method").unwrap_or_default();
    let body = match record_field(request, "body") {
        Value::String(text) => text.clone(),
        Value::List(bytes) => bytes
            .iter()
            .filter_map(|byte| match byte {
                Value::Int(byte) if (0..=255).contains(byte) => Some(*byte as u8),
                _ => None,
            })
            .map(char::from)
            .collect(),
        _ => String::new(),
    };
    let echoed = serde_json::json!({
        "url": url,
        "method": method,
        "body": body,
    });
    let body = serde_json::to_vec(&echoed).unwrap();
    Ok(Value::List(vec![
        Value::Symbol("ok".into()),
        Value::List(vec![
            pair("status", Value::Int(200)),
            pair("final-url", Value::String(url)),
            pair("headers", Value::Nil),
            pair(
                "body",
                Value::List(
                    body.into_iter()
                        .map(|byte| Value::Int(i64::from(byte)))
                        .collect(),
                ),
            ),
        ]),
    ]))
}

fn record_field<'a>(record: &'a Value, name: &str) -> &'a Value {
    let Some(items) = record.as_list() else {
        return &Value::Nil;
    };
    for item in items {
        let Some(pair) = item.as_list() else { continue };
        if pair
            .first()
            .and_then(Value::as_keyword)
            .is_some_and(|field| field == name)
        {
            return pair.get(1).unwrap_or(&Value::Nil);
        }
    }
    &Value::Nil
}

fn record_text(record: &Value, name: &str) -> Option<String> {
    match record_field(record, name) {
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => Some(text.clone()),
        Value::Nil => None,
        other => Some(other.to_string()),
    }
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.into()), value])
}

#[test]
fn soil_test_files_pass() {
    // A recursive Soil test can use more stack than libtest's default. Run the
    // whole corpus on an explicit stack so a new test does not fail the suite
    // for the size of its recursion rather than its result.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run_soil_tests)
        .expect("spawn soil test runner")
        .join()
        .expect("soil test runner panicked");
}

fn run_soil_tests() {
    for file in soil_tests() {
        let source = std::fs::read_to_string(&file).unwrap();
        let names = test_names(&source);
        assert!(
            !names.is_empty(),
            "{} has no test/... defun",
            file.display()
        );

        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        runtime
            .load_file(&file)
            .unwrap_or_else(|error| panic!("load {}: {error:#}", file.display()));

        for name in names {
            let result = runtime
                .call(&name, &[])
                .unwrap_or_else(|error| panic!("{}::{name}: {error:#}", file.display()));
            assert!(
                !matches!(result, Value::Nil | Value::Bool(false)),
                "{}::{name} returned {result}",
                file.display()
            );
        }
    }
}
