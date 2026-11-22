// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Soil Hue CLI against a stateful fake bridge.
//!
//! The fake owns the group/light state, so these tests exercise the same
//! plan/apply/verify path the real bridge sees, including its silent `[]`
//! reply for a malformed write.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use soil_lang::sexp::Value;
use soil_lang::{Host, Runtime};

#[derive(Clone)]
struct FakeBridge(Arc<Mutex<State>>);

struct State {
    lights: BTreeMap<String, Json>,
    groups: Json,
    scenes: Json,
    writes: Vec<(String, Json)>,
    empty_replies: bool,
}

impl FakeBridge {
    fn new() -> Self {
        let gamut = json!([[0.6915, 0.3083], [0.17, 0.7], [0.1532, 0.0475]]);
        let light = |id: &str, name: &str, bri: u64| {
            json!({
                "id": id,
                "name": name,
                "capabilities": { "control": { "colorgamut": gamut } },
                "state": {
                    "on": true,
                    "bri": bri,
                    "colormode": "ct",
                    "ct": 446,
                    "xy": [0.5013, 0.4152]
                }
            })
        };
        let mut lights = BTreeMap::new();
        lights.insert("3".into(), light("3", "Window Lamp", 200));
        lights.insert("8".into(), light("8", "Window", 207));
        lights.insert("7".into(), light("7", "Buddha", 207));
        lights.insert("4".into(), light("4", "Interrogation Lamp", 207));
        Self(Arc::new(Mutex::new(State {
            lights,
            groups: json!({
                "81": { "name": "Living room", "lights": ["3", "8", "7", "4"] },
                "82": { "name": "TV", "lights": ["3", "8", "7", "4"] }
            }),
            scenes: json!({ "1": { "name": "Relax", "group": "81" } }),
            writes: Vec::new(),
            empty_replies: false,
        })))
    }

    fn set_empty_replies(&self, empty: bool) {
        self.0.lock().unwrap().empty_replies = empty;
    }

    fn writes(&self) -> Vec<(String, Json)> {
        self.0.lock().unwrap().writes.clone()
    }

    fn light(&self, id: &str) -> Json {
        self.0.lock().unwrap().lights.get(id).cloned().unwrap()
    }

    fn fetch(&self, request: &Value) -> Result<Value, String> {
        let url = text(request, "url").unwrap_or_default();
        let method = text(request, "method").unwrap_or_default();
        let body = text(request, "body").unwrap_or_default();
        let path = url.split_once("/api/").map(|(_, tail)| tail).unwrap_or("");
        let path = path.strip_prefix("__CREDENTIAL__").unwrap_or(path);
        let mut state = self.0.lock().unwrap();
        let body: Json = if body.is_empty() {
            Json::Null
        } else {
            serde_json::from_str(&body).map_err(|error| error.to_string())?
        };
        let response = match (method.as_str(), path) {
            ("get", "") => json!({
                "config": { "name": "Fake Hue" },
                "lights": state.lights.clone(),
                "groups": state.groups.clone()
            }),
            ("get", "/groups") => state.groups.clone(),
            ("get", "/groups/0") => json!({
                "name": "All lights",
                "lights": state.lights.keys().cloned().collect::<Vec<_>>()
            }),
            ("get", "/groups/81") => json!({
                "name": "Living room",
                "lights": ["3", "8", "7", "4"]
            }),
            ("get", "/lights") => json!(state.lights.clone()),
            ("get", "/scenes") => state.scenes.clone(),
            ("get", path) if path.starts_with("/lights/") => {
                let id = path.trim_start_matches("/lights/");
                state.lights.get(id).cloned().ok_or("missing light")?
            }
            ("put", path) if path.starts_with("/lights/") && path.ends_with("/state") => {
                let id = path
                    .trim_start_matches("/lights/")
                    .trim_end_matches("/state");
                let light = state.lights.get_mut(id).ok_or("missing light")?;
                apply_body(&mut light["state"], &body);
                state.writes.push((path.to_string(), body.clone()));
                if state.empty_replies {
                    json!([])
                } else {
                    json!([{ "success": { "ok": true } }])
                }
            }
            ("put", path) if path.starts_with("/groups/") && path.ends_with("/action") => {
                state.writes.push((path.to_string(), body.clone()));
                if state.empty_replies {
                    json!([])
                } else {
                    json!([{ "success": { "ok": true } }])
                }
            }
            ("post", "") => json!([{ "success": { "username": "paired-key" } }]),
            other => return Err(format!("fake bridge: unexpected {other:?}")),
        };
        let bytes = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
        Ok(Value::List(vec![
            Value::Symbol("ok".into()),
            Value::List(vec![
                pair("status", Value::Int(200)),
                pair("final-url", Value::String(url)),
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
        ]))
    }
}

fn apply_body(state: &mut Json, body: &Json) {
    let Some(body) = body.as_object() else { return };
    for (key, value) in body {
        state[key] = value.clone();
    }
    if body.contains_key("xy") {
        state["colormode"] = json!("xy");
    }
}

impl Host for FakeBridge {
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

fn runtime(bridge: &FakeBridge) -> Runtime {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut runtime = Runtime::new(Box::new(bridge.clone()));
    for file in [
        "stdlib/prelude.soil",
        "stdlib/cli.soil",
        "bindings/wasi-cli.soil",
        "bindings/soil-http.soil",
        "../../examples/hue/hue.soil",
        "../../examples/hue/hue_cli.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime
}

fn run(args: &[&str]) -> (Value, FakeBridge) {
    let bridge = FakeBridge::new();
    let mut runtime = runtime(&bridge);
    runtime.set_args(args.iter().map(|arg| (*arg).to_string()).collect());
    let value = runtime
        .call(
            "hue/cli-main",
            &[
                Value::String("http://bridge.local".into()),
                Value::String("default".into()),
            ],
        )
        .unwrap();
    (value, bridge)
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

#[test]
fn mood_applies_integer_brightness_to_a_case_insensitive_room() {
    let (value, bridge) = run(&["mood", "sunset", "--room", "living room"]);
    assert_eq!(ok_text(&value), "applied sunset to Living room (4 lights)");
    let writes = bridge.writes();
    assert_eq!(writes.len(), 4, "{writes:?}");
    for (path, body) in writes {
        assert!(path.starts_with("/lights/"), "{path}");
        assert!(body["bri"].is_i64(), "bri must be an integer: {body}");
        assert!(body["transitiontime"].is_i64(), "{body}");
        assert!(body["xy"].is_array(), "{body}");
    }
    assert_eq!(bridge.light("3")["state"]["colormode"], "xy");
}

#[test]
fn a_silent_empty_reply_is_a_failure() {
    let bridge = FakeBridge::new();
    bridge.set_empty_replies(true);
    let mut runtime = runtime(&bridge);
    runtime.set_args(vec![
        "mood".into(),
        "sunset".into(),
        "--room".into(),
        "Living room".into(),
    ]);
    let value = runtime
        .call(
            "hue/cli-main",
            &[
                Value::String("http://bridge.local".into()),
                Value::String("default".into()),
            ],
        )
        .unwrap();
    let items = value.as_list().unwrap();
    assert_eq!(items.first().and_then(Value::as_symbol), Some("err"));
    assert!(items[1].as_str().unwrap().contains("empty success reply"));
}

#[test]
fn plan_does_not_write() {
    let (value, bridge) = run(&["plan", "sunset", "--room", "Living room"]);
    let text = ok_text(&value);
    assert!(text.contains("Window Lamp"), "{text}");
    assert!(bridge.writes().is_empty());
}

#[test]
fn snapshot_and_restore_round_trip() {
    let (snapshot, _bridge) = run(&["snapshot", "--room", "Living room"]);
    let snapshot = ok_text(&snapshot);
    assert!(snapshot.contains("\"lights\""), "{snapshot}");

    // Move the room, then put it back from the snapshot.
    let (_, bridge2) = run(&["mood", "ocean", "--room", "Living room"]);
    assert_eq!(bridge2.writes().len(), 4);
    // The fake is process-local; restore against the first bridge's snapshot
    // shape by replaying the same snapshot text through a fresh runtime.
    let bridge3 = FakeBridge::new();
    let mut runtime = runtime(&bridge3);
    runtime.set_args(vec!["restore".into(), "--snapshot-json".into(), snapshot]);
    let value = runtime
        .call(
            "hue/cli-main",
            &[
                Value::String("http://bridge.local".into()),
                Value::String("default".into()),
            ],
        )
        .unwrap();
    assert_eq!(ok_text(&value), "restored 4 lights");
    assert_eq!(bridge3.light("3")["state"]["bri"], 200);
}
