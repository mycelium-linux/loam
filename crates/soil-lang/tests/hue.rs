// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hue semantics exercised without a bridge.
//!
//! The fake host records the request values the library produces and returns
//! canned Hue JSON. The real `soil:http` transport is covered by `http.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use soil_lang::Runtime;
use soil_lang::sexp::Value;

#[derive(Default)]
struct FakeHttp {
    requests: Rc<RefCell<Vec<Value>>>,
    responses: Rc<RefCell<VecDeque<String>>>,
}

impl FakeHttp {
    fn new(responses: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            requests: Rc::new(RefCell::new(Vec::new())),
            responses: Rc::new(RefCell::new(
                responses.into_iter().map(str::to_owned).collect(),
            )),
        }
    }
}

impl soil_lang::Host for FakeHttp {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:http" && function == "fetch"
    }

    fn call(&mut self, _interface: &str, _function: &str, args: &[Value]) -> Result<Value, String> {
        let request = args.first().cloned().ok_or("no request")?;
        self.requests.borrow_mut().push(request);
        let body = self
            .responses
            .borrow_mut()
            .pop_front()
            .ok_or("no canned response")?;
        Ok(pair(
            "ok",
            Value::List(vec![
                field("status", Value::Int(200)),
                field("final-url", Value::String("http://bridge.local".into())),
                field("headers", Value::Nil),
                field(
                    "body",
                    Value::List(
                        body.bytes()
                            .map(|byte| Value::Int(i64::from(byte)))
                            .collect(),
                    ),
                ),
            ]),
        ))
    }
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Symbol(name.into()), value])
}

fn field(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.into()), value])
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

fn runtime(host: FakeHttp) -> Runtime {
    let mut runtime = Runtime::new(Box::new(host));
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    runtime.load_file(dir.join("stdlib/prelude.soil")).unwrap();
    runtime
        .load_file(dir.join("bindings/soil-http.soil"))
        .unwrap();
    runtime
        .load_file(dir.join("../../examples/hue/hue.soil"))
        .unwrap();
    runtime
}

fn bridge(runtime: &mut Runtime) -> Value {
    runtime
        .call(
            "hue/bridge",
            &[
                Value::String("http://bridge.local/".into()),
                Value::String("default".into()),
            ],
        )
        .unwrap()
}

#[test]
fn hue_reads_lights_through_the_binding() {
    let host = FakeHttp::new([r#"{"1":{"name":"Desk","state":{"on":true}}}"#]);
    let requests = host.requests.clone();
    let mut runtime = runtime(host);
    let bridge = bridge(&mut runtime);

    let result = runtime.call("hue/lights", &[bridge]).unwrap();
    assert!(matches!(
        result
            .as_list()
            .and_then(|items| items.first())
            .and_then(Value::as_symbol),
        Some("ok")
    ));

    let requests = requests.borrow();
    let request = &requests[0];
    assert_eq!(
        record_field(request, "url").as_str(),
        Some("http://bridge.local/api/__CREDENTIAL__/lights")
    );
    assert_eq!(
        record_field(request, "credential").as_str(),
        Some("default")
    );
}

#[test]
fn hue_scene_looks_up_the_name_and_puts_its_id() {
    let host = FakeHttp::new([
        r#"{"7":{"name":"Relax","group":"1"}}"#,
        r#"[{"success":{}}]"#,
    ]);
    let requests = host.requests.clone();
    let mut runtime = runtime(host);
    let bridge = bridge(&mut runtime);
    let group = runtime
        .call("hue/group", &[Value::String("1".into())])
        .unwrap();

    let result = runtime
        .call("hue/scene", &[bridge, group, Value::String("Relax".into())])
        .unwrap();
    assert!(matches!(
        result
            .as_list()
            .and_then(|items| items.first())
            .and_then(Value::as_symbol),
        Some("ok")
    ));

    let requests = requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        record_field(&requests[1], "url").as_str(),
        Some("http://bridge.local/api/__CREDENTIAL__/groups/1/action")
    );
    assert_eq!(
        record_field(&requests[1], "body").as_str(),
        Some(r#"{"scene":"7"}"#)
    );
}

#[test]
fn hue_converts_rgb_to_the_bridge_xy_contract() {
    let mut runtime = runtime(FakeHttp::default());
    let red = runtime
        .call(
            "hue/rgb->xy",
            &[Value::Int(255), Value::Int(0), Value::Int(0)],
        )
        .unwrap();
    let red = red.as_list().unwrap();
    let x = red[0].as_f64().unwrap();
    let y = red[1].as_f64().unwrap();
    assert!((x - 0.7006).abs() < 0.001, "{x}");
    assert!((y - 0.2993).abs() < 0.001, "{y}");
}
