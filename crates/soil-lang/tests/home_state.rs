// SPDX-License-Identifier: MIT OR Apache-2.0
//! The centralized home snapshot composes the Hue and playback adapters.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use soil_lang::sexp::Value;
use soil_lang::{Host, Runtime};

#[derive(Default)]
struct FakeHome {
    deadwax_fail: bool,
    writes: Vec<(String, String)>,
}

impl FakeHome {
    fn fetch(&mut self, request: &Value) -> Result<Value, String> {
        let url = text(request, "url").unwrap_or_default();
        let method = text(request, "method").unwrap_or_default();
        let path = url
            .strip_prefix("http://bridge.local")
            .unwrap_or(&url)
            .to_string();
        self.writes.push((method.clone(), path.clone()));
        let body = match (method.as_str(), path.as_str()) {
            ("get", "/api/__CREDENTIAL__/groups") => json!({
                "81": { "name": "Living room", "lights": ["3"] }
            }),
            ("get", "/api/__CREDENTIAL__/groups/81") => json!({
                "name": "Living room",
                "lights": ["3"]
            }),
            ("get", "/api/__CREDENTIAL__/lights/3") => json!({
                "name": "Window Lamp",
                "capabilities": {
                    "control": { "colorgamut": [[0.6915,0.3083],[0.17,0.7],[0.1532,0.0475]] }
                },
                "state": {
                    "on": true,
                    "bri": 160,
                    "colormode": "xy",
                    "xy": [0.6418,0.3316]
                }
            }),
            ("get", "/api/__CREDENTIAL__/groups/81/action") => json!({ "on": true }),
            ("get", "/health") => {
                if self.deadwax_fail {
                    return Ok(response(500, &json!({ "error": "down" }), &url));
                }
                json!({ "ok": true })
            }
            ("get", "/mpd/status") => {
                if self.deadwax_fail {
                    return Ok(response(500, &json!({ "error": "down" }), &url));
                }
                json!({ "playing": true, "title": "Chan Chan", "uri": "chan-chan.flac" })
            }
            ("get", "/mpd/queue") => {
                if self.deadwax_fail {
                    return Ok(response(500, &json!({ "error": "down" }), &url));
                }
                json!({
                    "items": [{
                        "artist": "Buena Vista Social Club",
                        "album": "Buena Vista Social Club",
                        "title": "Chan Chan",
                        "current": true,
                        "uri": "chan-chan.flac"
                    }]
                })
            }
            ("post", _) => json!({}),
            other => return Err(format!("fake home: unexpected {other:?}")),
        };
        Ok(response(200, &body, &url))
    }
}

struct Shared(Arc<Mutex<FakeHome>>);

impl Host for Shared {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:http" && function == "fetch"
    }

    fn call(&mut self, interface: &str, function: &str, args: &[Value]) -> Result<Value, String> {
        if interface != "soil:http" || function != "fetch" {
            return Err(format!("fake host: unexpected {interface}/{function}"));
        }
        self.0
            .lock()
            .unwrap()
            .fetch(args.first().ok_or("missing request")?)
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

fn runtime(bridge: Arc<Mutex<FakeHome>>) -> Runtime {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut runtime = Runtime::new(Box::new(Shared(bridge)));
    for file in [
        "stdlib/prelude.soil",
        "bindings/soil-http.soil",
        "../../examples/hue/hue.soil",
        "../../examples/playback/playback.soil",
        "../../examples/home/home.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime
}

#[test]
fn snapshot_has_lights_media_and_an_honest_camera_placeholder() {
    let state = Arc::new(Mutex::new(FakeHome::default()));
    let mut runtime = runtime(state);
    let value = runtime.load("(home/snapshot (home/config \"http://bridge.local\" \"default\" \"Living room\" \"http://bridge.local\"))").unwrap();
    let room = field(&value, "room");
    let lights = field(room, "lights");
    assert_eq!(lights.as_list().unwrap().len(), 1, "{value}");
    assert_eq!(
        field(&lights.as_list().unwrap()[0], "name").as_str(),
        Some("Window Lamp")
    );
    let media = field(&value, "media");
    assert_eq!(
        field(field(media, "status"), "title").as_str(),
        Some("Chan Chan")
    );
    assert_eq!(
        field(field(media, "current"), "artist").as_str(),
        Some("Buena Vista Social Club")
    );
    let cameras = field(&value, "cameras").as_list().unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(field(&cameras[0], "status").as_str(), Some("unavailable"));
}

#[test]
fn a_dead_player_does_not_blank_the_lights() {
    let state = Arc::new(Mutex::new(FakeHome {
        deadwax_fail: true,
        writes: Vec::new(),
    }));
    let mut runtime = runtime(state);
    let value = runtime.load("(home/snapshot (home/config \"http://bridge.local\" \"default\" \"Living room\" \"http://bridge.local\"))").unwrap();
    let room = field(&value, "room");
    assert_eq!(
        field(field(&room, "room"), "name").as_str(),
        Some("Living room")
    );
    let media = field(&value, "media");
    assert!(media.as_list().unwrap().iter().any(|item| {
        item.as_list()
            .and_then(|pair| pair.first())
            .and_then(Value::as_keyword)
            == Some("error")
    }));
}

#[test]
fn a_media_command_reaches_deadwax() {
    let state = Arc::new(Mutex::new(FakeHome::default()));
    let mut runtime = runtime(state.clone());
    let value = runtime
        .load("(home/command (home/config \"http://bridge.local\" \"default\" \"Living room\" \"http://bridge.local\") (list (list :kind \"media\") (list :verb \"volume\") (list :value 55)))")
        .unwrap();
    assert_eq!(value.as_list().unwrap()[0].as_symbol(), Some("ok"));
    let writes = &state.lock().unwrap().writes;
    assert!(
        writes
            .iter()
            .any(|(method, path)| method == "post" && path == "/mpd/volume?level=55"),
        "{writes:?}"
    );
}
