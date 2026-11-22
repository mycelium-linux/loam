// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native Soil Lisp host exposes the same bounded poll/publish contract
//! as the Wasm host, so an automation script can be exercised without a hub.

use std::path::Path;
use std::sync::Arc;

use soil::unibus::{Bus, MemoryBus};
use soil_lang::sexp::Value;
use soil_lang::{HostChain, Runtime, UnibusHost};

fn runtime(bus: Arc<MemoryBus>) -> Runtime {
    let host = HostChain::new(vec![Box::new(UnibusHost::from_bus(bus))]);
    let mut runtime = Runtime::new(Box::new(host));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    runtime
        .load_file(dir.join("bindings/soil-unibus.soil"))
        .unwrap();
    runtime
}

#[test]
fn soil_lisp_polls_a_frigate_frame() {
    let bus = Arc::new(MemoryBus::default());
    bus.publish(
        "frigate.event.v1",
        br#"{"event_id":"abc","type":"new","camera":"front_door","label":"person","score":0.9}"#,
        None,
        0,
    )
    .unwrap();
    let mut runtime = runtime(bus);
    runtime
        .load(
            r#"(defun poll-once ()
                 (let ((page (soil:unibus/bus.poll (list "frigate.event.v1") nil 8)))
                   (and
                     (ok? page)
                     (= (length (field (value page) :frames)) 1)
                     (= (field (first (field (value page) :frames)) :kind)
                        "frigate.event.v1"))))"#,
        )
        .unwrap();
    let result = runtime.call("poll-once", &[]).unwrap();
    assert!(
        !matches!(result, Value::Nil | Value::Bool(false)),
        "{result}"
    );
}

#[test]
fn soil_lisp_publishes_a_json_object() {
    let bus = Arc::new(MemoryBus::default());
    let mut runtime = runtime(Arc::clone(&bus));
    runtime
        .load(
            r#"(defun publish-one ()
                 (let ((receipt
                         (soil:unibus/bus.publish
                           "frigate.action.v1"
                           (string->bytes "{\"event_id\":\"abc\"}")
                           nil
                           0)))
                   (ok? receipt)))"#,
        )
        .unwrap();
    let result = runtime.call("publish-one", &[]).unwrap();
    assert!(
        !matches!(result, Value::Nil | Value::Bool(false)),
        "{result}"
    );
    let frames = bus.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].kind, "frigate.action.v1");
    assert_eq!(frames[0].body, br#"{"event_id":"abc"}"#);
}
