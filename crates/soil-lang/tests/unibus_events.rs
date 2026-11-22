// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "native")]
//! The Soil events library over an in-process Unibus.

use std::path::Path;
use std::sync::Arc;

use soil::unibus::MemoryBus;
use soil_lang::sexp::Value;
use soil_lang::{Runtime, UnibusHost};

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

fn runtime() -> Runtime {
    let bus = Arc::new(MemoryBus::default());
    let mut runtime = Runtime::new(Box::new(UnibusHost::from_bus(bus)));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "stdlib/prelude.soil",
        "stdlib/events.soil",
        "bindings/soil-unibus.soil",
    ] {
        runtime.load_file(dir.join(file)).unwrap();
    }
    runtime
}

fn ok(value: &Value) -> &Value {
    let items = value.as_list().expect("tagged list");
    assert_eq!(
        items.first().and_then(Value::as_symbol),
        Some("ok"),
        "{value}"
    );
    &items[1]
}

#[test]
fn publish_then_poll_decodes_the_body_and_advances_the_subscription() {
    let mut runtime = runtime();
    let published = runtime
        .load("(events/publish \"test.event.v1\" (list (list :id \"a\") (list :n 2)) nil 0)")
        .unwrap();
    assert!(field(ok(&published), "request-id").as_str().is_some());

    let page = runtime
        .load("(events/next (value (events/subscribe (list \"test.event.v1\") 8)))")
        .unwrap();
    let page = ok(&page);
    let events = field(page, "events").as_list().unwrap();
    assert_eq!(events.len(), 1, "{page}");
    assert_eq!(field(&events[0], "kind").as_str(), Some("test.event.v1"));
    let body = field(&events[0], "body");
    assert_eq!(field(body, "id").as_str(), Some("a"));
    assert!(matches!(field(body, "n"), Value::Int(2)));
    let subscription = field(page, "subscription");
    assert!(matches!(field(subscription, "limit"), Value::Int(8)));
}

#[test]
fn a_non_object_body_is_rejected_before_the_bus() {
    let mut runtime = runtime();
    let value = runtime
        .load("(events/publish \"test.event.v1\" (list 1 2) nil 0)")
        .unwrap();
    let items = value.as_list().unwrap();
    assert_eq!(items.first().and_then(Value::as_symbol), Some("err"));
    assert!(items[1].as_str().unwrap().contains("JSON object"));
}

#[test]
fn an_empty_subscription_is_rejected() {
    let mut runtime = runtime();
    let value = runtime.load("(events/subscribe (list) 8)").unwrap();
    let items = value.as_list().unwrap();
    assert_eq!(items.first().and_then(Value::as_symbol), Some("err"));
    assert!(items[1].as_str().unwrap().contains("at least one kind"));
}
