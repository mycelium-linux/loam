// SPDX-License-Identifier: MIT OR Apache-2.0
//! In-process `soil:unibus` host for the native Soil Lisp runner.
//!
//! This adapter is deliberately thin: it shapes the same `poll` / `publish`
//! contract the Wasm host exposes. The bus owns the hub session, token, and
//! retention window; a Soil program sees only kinds, cursors, and JSON-object
//! body bytes.

use std::path::Path;
use std::sync::Arc;

use ::soil::unibus::{Bus, BusError, Frame, Page, UnibusBus};

use crate::Host;
use crate::sexp::Value;

pub struct UnibusHost {
    bus: Arc<dyn Bus>,
}

impl UnibusHost {
    /// Connect from a JSON `ClientConfig`, with ordered hub fallback.
    pub fn connect(path: impl AsRef<Path>, hubs: &[String]) -> anyhow::Result<Self> {
        let bus = UnibusBus::from_config_file_with_hubs(path.as_ref(), hubs)?;
        Ok(Self { bus: Arc::new(bus) })
    }

    /// Use an already-constructed bus. Tests use this with `MemoryBus`.
    pub fn from_bus(bus: Arc<dyn Bus>) -> Self {
        Self { bus }
    }

    fn poll(&mut self, args: &[Value]) -> std::result::Result<Value, String> {
        let kinds = args
            .first()
            .and_then(Value::as_list)
            .ok_or_else(|| "soil:unibus/poll needs a list of kinds".to_string())?
            .iter()
            .map(|kind| {
                kind.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "soil:unibus/poll kinds must be strings".to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let cursor = match args.get(1) {
            None | Some(Value::Nil) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| "soil:unibus/poll cursor must be a string".to_string())?
                    .to_owned(),
            ),
        };
        let limit = args
            .get(2)
            .and_then(Value::as_f64)
            .map(|value| value.max(0.0).min(f64::from(u32::MAX)) as u32)
            .unwrap_or(64);
        match self.bus.poll(&kinds, cursor.as_deref(), limit) {
            Ok(page) => Ok(ok(page_value(page))),
            Err(error) => Ok(bus_error(error)),
        }
    }

    fn publish(&mut self, args: &[Value]) -> std::result::Result<Value, String> {
        let kind = args
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| "soil:unibus/publish needs a kind".to_string())?;
        let body = value_bytes(
            args.get(1)
                .ok_or_else(|| "soil:unibus/publish needs a body".to_string())?,
        )?;
        let destination = match args.get(2) {
            None | Some(Value::Nil) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| "soil:unibus/publish destination must be a string".to_string())?
                    .to_owned(),
            ),
        };
        let ttl_ms = args
            .get(3)
            .and_then(Value::as_f64)
            .map(|value| value.max(0.0).min(u64::MAX as f64) as u64)
            .unwrap_or(0);
        match self
            .bus
            .publish(kind, &body, destination.as_deref(), ttl_ms)
        {
            Ok(receipt) => Ok(ok(Value::List(vec![pair(
                "request-id",
                Value::String(receipt.request_id),
            )]))),
            Err(error) => Ok(bus_error(error)),
        }
    }
}

impl Host for UnibusHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:unibus" && matches!(function, "poll" | "publish")
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        if interface != "soil:unibus" {
            return Err(format!("unknown interface `{interface}`"));
        }
        match function {
            "poll" => self.poll(args),
            "publish" => self.publish(args),
            other => Err(format!("unknown soil:unibus function `{other}`")),
        }
    }
}

fn frame_value(frame: Frame) -> Value {
    Value::List(vec![
        pair("kind", Value::String(frame.kind)),
        pair("producer", Value::String(frame.producer)),
        pair("issued-at-ms", Value::Int(frame.issued_at_ms as i64)),
        pair(
            "body",
            Value::List(
                frame
                    .body
                    .into_iter()
                    .map(|byte| Value::Int(i64::from(byte)))
                    .collect(),
            ),
        ),
    ])
}

fn page_value(page: Page) -> Value {
    Value::List(vec![
        pair(
            "frames",
            Value::List(page.frames.into_iter().map(frame_value).collect()),
        ),
        pair("next", page.next.map(Value::String).unwrap_or(Value::Nil)),
    ])
}

fn bus_error(error: BusError) -> Value {
    let code = match error {
        BusError::Denied => "denied",
        BusError::Invalid => "invalid",
        BusError::OverBudget => "over-budget",
        BusError::Stale => "stale",
        BusError::Unavailable => "unavailable",
    };
    err(code)
}

fn value_bytes(value: &Value) -> std::result::Result<Vec<u8>, String> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                _ => Err("soil:unibus/publish body must be a list of bytes".to_string()),
            })
            .collect(),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => Err("soil:unibus/publish body must be a list of bytes or a string".into()),
    }
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.into()), value])
}

fn ok(value: Value) -> Value {
    Value::List(vec![Value::Symbol("ok".into()), value])
}

fn err(code: &str) -> Value {
    Value::List(vec![
        Value::Symbol("err".into()),
        Value::Symbol(code.into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::soil::unibus::MemoryBus;

    #[test]
    fn poll_maps_the_wit_page_shape() {
        let bus = Arc::new(MemoryBus::default());
        bus.publish(
            "frigate.event.v1",
            br#"{"event_id":"abc","type":"new"}"#,
            None,
            0,
        )
        .unwrap();
        let mut host = UnibusHost::from_bus(bus);
        let kinds = Value::List(vec![Value::String("frigate.event.v1".into())]);
        let value = host
            .call("soil:unibus", "poll", &[kinds, Value::Nil, Value::Int(8)])
            .unwrap();
        assert!(
            matches!(&value, Value::List(items) if items.first().and_then(Value::as_symbol) == Some("ok"))
        );
        let Value::List(page) = value else {
            unreachable!()
        };
        let page = &page[1];
        let frames = page
            .as_list()
            .unwrap()
            .iter()
            .find(|item| {
                item.as_list()
                    .and_then(|pair| pair.first())
                    .and_then(Value::as_keyword)
                    == Some("frames")
            })
            .unwrap();
        let frames = frames.as_list().unwrap()[1].as_list().unwrap();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        let kind = frame
            .as_list()
            .unwrap()
            .iter()
            .find(|item| {
                item.as_list()
                    .and_then(|pair| pair.first())
                    .and_then(Value::as_keyword)
                    == Some("kind")
            })
            .unwrap();
        assert_eq!(
            kind.as_list().unwrap()[1].as_str(),
            Some("frigate.event.v1")
        );
    }
}
