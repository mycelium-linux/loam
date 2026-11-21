// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native `soil:http` grant: origin policy, credential substitution, and
//! the Value conversion the REPL uses.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use soil_lang::sexp::Value;
use soil_lang::{NativeHost, Runtime};

#[test]
fn native_http_substitutes_the_path_credential_and_decodes_json() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request).into_owned();
        tx.send(request).unwrap();
        let body = br#"{"ok":true,"name":"Desk"}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });

    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let host = NativeHost::open(&workspace, root.path(), "soil")
        .unwrap()
        .with_http(
            vec![format!("http://{address}")],
            Some("test-key".to_string()),
        )
        .unwrap();
    let mut runtime = Runtime::new(Box::new(host));
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    runtime.load_file(dir.join("stdlib/prelude.soil")).unwrap();
    runtime
        .load_file(dir.join("bindings/soil-http.soil"))
        .unwrap();
    let source = r#"
(defun test/fetch ()
  (let ((response
          (soil:http/fetch
            (list
              (list :url "http://__ORIGIN__/api/__CREDENTIAL__/lights")
              (list :method "get")
              (list :headers nil)
              (list :body nil)
              (list :credential "default")
              (list :max-bytes 4096)
              (list :timeout-ms 5000)))))
    (and
      (ok? response)
      (= (field (value response) :status) 200)
      (= (field (json-decode (field (value response) :body)) :ok) #t))))
"#
    .replace("__ORIGIN__", &address.to_string());
    runtime.load(&source).unwrap();

    assert_eq!(runtime.call("test/fetch", &[]).unwrap(), Value::Bool(true));
    let request = rx.recv().unwrap();
    assert!(
        request.starts_with("GET /api/test-key/lights HTTP/1.1"),
        "{request}"
    );
    assert!(!request.contains("__CREDENTIAL__"), "{request}");
    server.join().unwrap();
}
