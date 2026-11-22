// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Soil playback CLI against a fake deadwax service.

use std::path::Path;

use serde_json::{Value as Json, json};
use soil_lang::sexp::Value;
use soil_lang::{Host, Runtime};

#[derive(Default)]
struct FakeDeadwax {
    requests: Vec<(String, String)>,
    fail: bool,
}

impl FakeDeadwax {
    fn fetch(&mut self, request: &Value) -> Result<Value, String> {
        let url = text(request, "url").unwrap_or_default();
        let method = text(request, "method").unwrap_or_default();
        let path = url
            .strip_prefix("http://bridge.local")
            .unwrap_or(&url)
            .to_string();
        self.requests.push((method.clone(), path.clone()));
        if self.fail {
            return Ok(response(500, &json!({ "error": "down" }), &url));
        }
        let body = match (method.as_str(), path.as_str()) {
            ("get", "/health") => json!({ "ok": true }),
            ("get", "/mpd/status") => json!({
                "playing": true,
                "title": "Chan Chan",
                "uri": "http://127.0.0.1:6601/stream?artist=Buena+Vista+Social+Club&title=Chan+Chan"
            }),
            ("get", "/mpd/queue") => json!({
                "items": [
                    {
                        "artist": "Buena Vista Social Club",
                        "album": "Buena Vista Social Club",
                        "title": "Chan Chan",
                        "current": true,
                        "uri": "chan-chan.flac"
                    },
                    {
                        "artist": "The Weeknd",
                        "album": "Thursday",
                        "title": "The Morning",
                        "current": false,
                        "uri": "the-morning.flac"
                    }
                ]
            }),
            ("get", "/playlists?coverage=1") => json!([
                {
                    "owner": "avery",
                    "playlist": { "name": "Rap", "kind": "Playlist" },
                    "coverage": { "owned": 10, "missing": 0 }
                }
            ]),
            ("get", path) if path.starts_with("/search?") => json!([
                { "artist": "Board of Canada", "title": "Roygbiv", "uri": "roygbiv.flac" }
            ]),
            ("get", path) if path.starts_with("/tracks?") => json!([
                { "artist": "Board of Canada", "title": "Roygbiv", "uri": "roygbiv.flac" }
            ]),
            ("post", _) => json!({}),
            other => return Err(format!("fake deadwax: unexpected {other:?}")),
        };
        Ok(response(200, &body, &url))
    }
}

impl Host for FakeDeadwax {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:http" && function == "fetch"
    }

    fn call(&mut self, interface: &str, function: &str, args: &[Value]) -> Result<Value, String> {
        if interface != "soil:http" || function != "fetch" {
            return Err(format!("fake host: unexpected {interface}/{function}"));
        }
        let request = args.first().ok_or("missing request")?;
        self.fetch(request)
    }
}

fn response(status: i64, body: &Json, url: &str) -> Value {
    let bytes = serde_json::to_vec(body).unwrap();
    Value::List(vec![
        Value::Symbol("ok".into()),
        Value::List(vec![
            pair("status", Value::Int(status)),
            pair("final-url", Value::String(url.into())),
            pair("headers", Value::Nil),
            pair(
                "body",
                Value::List(
                    bytes
                        .into_iter()
                        .map(|byte| Value::Int(i64::from(byte)))
                        .collect(),
                ),
            ),
        ]),
    ])
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.into()), value])
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_list()
        .and_then(|items| {
            items.iter().find_map(|item| {
                let pair = item.as_list()?;
                (pair.first()?.as_keyword()? == name).then(|| pair.get(1))?
            })
        })
        .unwrap_or(&Value::Nil)
}

fn text(value: &Value, name: &str) -> Option<String> {
    field(value, name).as_str().map(str::to_owned)
}

fn run_with_handle(args: &[String]) -> (Value, std::sync::Arc<std::sync::Mutex<FakeDeadwax>>) {
    use std::sync::{Arc, Mutex};
    let bridge = Arc::new(Mutex::new(FakeDeadwax::default()));
    let mut runtime = Runtime::new(Box::new(Shared(bridge.clone())));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "stdlib/prelude.soil",
        "stdlib/cli.soil",
        "bindings/wasi-cli.soil",
        "bindings/soil-http.soil",
        "../../examples/playback/playback.soil",
        "../../examples/playback/playback_cli.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime.set_args(args.to_vec());
    let value = runtime.call("playback/cli-main", &[]).unwrap();
    (value, bridge)
}

struct Shared(std::sync::Arc<std::sync::Mutex<FakeDeadwax>>);

impl Host for Shared {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:http" && function == "fetch"
    }

    fn call(&mut self, interface: &str, function: &str, args: &[Value]) -> Result<Value, String> {
        self.0.lock().unwrap().call(interface, function, args)
    }
}

fn ok_text(value: &Value) -> String {
    let items = value.as_list().expect("tagged list");
    assert_eq!(
        items.first().and_then(Value::as_symbol),
        Some("ok"),
        "{value}"
    );
    items[1].as_str().expect("string result").to_owned()
}

fn base(args: &[&str]) -> Vec<String> {
    let mut all = vec!["--source".to_string(), "http://bridge.local".to_string()];
    all.extend(args.iter().map(|arg| (*arg).to_string()));
    all
}

#[test]
fn status_reads_the_mpd_state() {
    let args = base(&["status"]);
    let (value, _) = run_with_handle(&args);
    assert!(ok_text(&value).contains("playing"));
    assert!(ok_text(&value).contains("Chan Chan"));
}

#[test]
fn now_uses_the_current_queue_item() {
    let args = base(&["now"]);
    let (value, _) = run_with_handle(&args);
    let text = ok_text(&value);
    assert!(text.contains("Buena Vista Social Club"), "{text}");
    assert!(text.contains("Chan Chan"), "{text}");
}

#[test]
fn playlists_are_read_from_the_nested_playlist_record() {
    let args = base(&["playlists"]);
    let (value, _) = run_with_handle(&args);
    assert_eq!(ok_text(&value), "avery\tRap\tPlaylist");
}

#[test]
fn playlist_posts_the_owner_and_name() {
    let args = base(&["playlist", "avery", "Rap"]);
    let (value, bridge) = run_with_handle(&args);
    assert_eq!(ok_text(&value), "playing avery / Rap");
    let requests = &bridge.lock().unwrap().requests;
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].0, "post");
    assert_eq!(requests[0].1, "/queue/playlist?user=avery&name=Rap");
}

#[test]
fn volume_and_search_encode_the_query() {
    let args = base(&["volume", "65"]);
    let (value, bridge) = run_with_handle(&args);
    assert_eq!(ok_text(&value), "volume ok");
    assert_eq!(bridge.lock().unwrap().requests[0].1, "/mpd/volume?level=65");

    let args = base(&["search", "board of canada"]);
    let (value, bridge) = run_with_handle(&args);
    assert!(ok_text(&value).contains("Roygbiv"));
    assert_eq!(
        bridge.lock().unwrap().requests[0].1,
        "/search?q=board+of+canada&limit=20"
    );
}

#[test]
fn an_http_error_becomes_a_failure() {
    use std::sync::{Arc, Mutex};
    let bridge = Arc::new(Mutex::new(FakeDeadwax {
        requests: Vec::new(),
        fail: true,
    }));
    let mut runtime = Runtime::new(Box::new(Shared(bridge)));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "stdlib/prelude.soil",
        "stdlib/cli.soil",
        "bindings/wasi-cli.soil",
        "bindings/soil-http.soil",
        "../../examples/playback/playback.soil",
        "../../examples/playback/playback_cli.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime.set_args(vec![
        "--source".into(),
        "http://bridge.local".into(),
        "status".into(),
    ]);
    let value = runtime.call("playback/cli-main", &[]).unwrap();
    let items = value.as_list().unwrap();
    assert_eq!(items.first().and_then(Value::as_symbol), Some("err"));
    assert!(items[1].as_str().unwrap().contains("HTTP 500"));
}
