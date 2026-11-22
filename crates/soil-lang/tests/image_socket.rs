// SPDX-License-Identifier: MIT OR Apache-2.0
//! Session-scoped `soil-image` over a local Unix socket.

#![cfg(all(unix, feature = "native"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use soil::vfs_host::{VfsHost, vfs::Host as VfsHostTrait};
use soil_lang::{Host, NativeHost};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_socket(path: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return stream;
        }
        if Instant::now() >= deadline {
            panic!("soil-image did not create {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn request(stream: &mut UnixStream, request: Json) -> Json {
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn two_clients_share_one_session_image() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let socket = root.path().join("image.sock");
    let prelude = Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib/prelude.soil");
    let child = Command::new(env!("CARGO_BIN_EXE_soil-image"))
        .arg("--socket")
        .arg(&socket)
        .env("SOIL_PRELUDE", prelude)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);

    let attach = json!({
        "op": "attach",
        "session": "session-1",
        "desktop": "fpl",
        "project": "suite",
        "workspace": workspace,
        "records": records,
    });

    let mut first = wait_for_socket(&socket);
    let attached = request(&mut first, attach.clone());
    assert_eq!(attached["ok"], true);
    assert_eq!(attached["result"]["existing"], false);

    let defined = request(
        &mut first,
        json!({
            "op": "eval",
            "session": "session-1",
            "source": "(defun shared (x) (+ x 1))",
        }),
    );
    assert_eq!(defined["ok"], true);

    // A second agent attaching to the same session joins the live image; it
    // does not replace it with a fresh runtime.
    let mut second = wait_for_socket(&socket);
    let joined = request(&mut second, attach);
    assert_eq!(joined["ok"], true);
    assert_eq!(joined["result"]["existing"], true);

    let called = request(
        &mut second,
        json!({
            "op": "eval",
            "session": "session-1",
            "source": "(shared 41)",
        }),
    );
    assert_eq!(called["ok"], true);
    assert_eq!(called["result"]["result"], "42");

    let status = request(&mut second, json!({"op": "status", "session": "session-1"}));
    assert_eq!(status["ok"], true);
    assert_eq!(status["result"]["desktop"], "fpl");
    assert_eq!(status["result"]["project"], "suite");
}

#[test]
fn compile_writes_a_wasm_artifact_to_vfs() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let socket = root.path().join("image.sock");
    let prelude = Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib/prelude.soil");
    let child = Command::new(env!("CARGO_BIN_EXE_soil-image"))
        .arg("--socket")
        .arg(&socket)
        .env("SOIL_PRELUDE", prelude)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);

    let mut stream = wait_for_socket(&socket);
    let attached = request(
        &mut stream,
        json!({
            "op": "attach",
            "session": "s3",
            "workspace": workspace,
            "records": records,
        }),
    );
    assert_eq!(attached["ok"], true);
    let compiled = request(
        &mut stream,
        json!({
            "op": "compile",
            "session": "s3",
            "source": "(defun add (a b) (+ a b))",
        }),
    );
    assert_eq!(compiled["ok"], true);
    assert_eq!(compiled["result"]["functions"], json!(["add"]));
    assert!(compiled["result"]["bytes"].as_u64().unwrap() > 0);
    let path = compiled["result"]["path"].as_str().unwrap();
    assert!(path.starts_with("compiled/"));

    let mut vfs = VfsHost::open(&workspace).unwrap();
    let entry = VfsHostTrait::stat(&mut vfs, path.to_string()).unwrap();
    let page = VfsHostTrait::read(&mut vfs, path.to_string(), 0, entry.size as u32).unwrap();
    assert_eq!(&page.data[..4], b"\0asm");

    let by_source = request(
        &mut stream,
        json!({
            "op": "call-compiled",
            "session": "s3",
            "source": "(defun add (a b) (+ a b))",
            "function": "add",
            "args": [20, 22],
        }),
    );
    assert_eq!(by_source["ok"], true, "{by_source}");
    assert_eq!(by_source["result"]["result"], 42);
    assert_eq!(by_source["result"]["compiled"], true);

    let by_path = request(
        &mut stream,
        json!({
            "op": "call-compiled",
            "session": "s3",
            "path": path,
            "function": "add",
            "args": [1, 2],
        }),
    );
    assert_eq!(by_path["ok"], true);
    assert_eq!(by_path["result"]["result"], 3);
}

#[test]
fn load_uses_vfs_and_reports_provenance() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = root.path().join("workspace");
    let records = root.path().join("records");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&records).unwrap();

    let mut host = NativeHost::open(&workspace, &records, "soil").unwrap();
    host.write_source("lib/hello.soil", "(defun greet () \"hello\")\n")
        .unwrap();
    drop(host);

    let socket = root.path().join("image.sock");
    let prelude = Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib/prelude.soil");
    let child = Command::new(env!("CARGO_BIN_EXE_soil-image"))
        .arg("--socket")
        .arg(&socket)
        .env("SOIL_PRELUDE", prelude)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);

    let mut stream = wait_for_socket(&socket);
    let attached = request(
        &mut stream,
        json!({
            "op": "attach",
            "session": "s2",
            "workspace": workspace,
            "records": records,
        }),
    );
    assert_eq!(attached["ok"], true);
    let loaded = request(
        &mut stream,
        json!({"op": "load", "session": "s2", "path": "lib/hello.soil"}),
    );
    assert_eq!(loaded["ok"], true);
    let called = request(
        &mut stream,
        json!({"op": "eval", "session": "s2", "source": "(greet)"}),
    );
    assert_eq!(called["result"]["result"], "\"hello\"");
    let status = request(&mut stream, json!({"op": "status", "session": "s2"}));
    assert_eq!(status["result"]["dirty"], json!([]));
    assert_eq!(status["result"]["unattached"], json!([]));
    assert_eq!(status["result"]["loaded"], json!(["lib/hello.soil"]));
}
