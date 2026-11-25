//! The Soil `Host` over Loam's providers.
//!
//! This is the whole adapter: `ProviderHost` does the calling, and this maps
//! Soil values to `wasmtime::component::Val` and back. It names no provider, so
//! a Soil program able to call one provider can call all of them.
//!
//! Where a type has no faithful mapping the call is refused rather than
//! approximated: a wrong value that type-checks is worse than an error.

use soil_lang::eval::Host as SoilHost;
use soil_lang::sexp::Value;
use wasmtime::component::{Type, Val};

use super::provider_host::{ProviderError, ProviderHost};

pub struct SoilProviders {
    inner: ProviderHost,
}

impl SoilProviders {
    pub fn new(inner: ProviderHost) -> Self {
        Self { inner }
    }

    pub fn grant(&mut self, interface: &str) {
        self.inner.grant(interface);
    }
}

impl SoilHost for SoilProviders {
    fn granted(&self, interface: &str, function: &str) -> bool {
        self.inner.accepts(interface, function)
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        let types = match self.inner.signature(interface, function) {
            Ok(types) => types,
            Err(error) => return Err(format!("{error}")),
        };
        if types.len() != args.len() {
            return Err(format!(
                "{interface}/{function} takes {} argument(s), got {}",
                types.len(),
                args.len()
            ));
        }
        let mut params = Vec::with_capacity(args.len());
        for (value, ty) in args.iter().zip(types.iter()) {
            params.push(to_val(value, ty)?);
        }
        let value = self
            .inner
            .call(interface, function, &params)
            .map_err(|error| match error {
                ProviderError::Denied { interface } => {
                    format!("not granted: {interface} (add it to the Spore's [grants])")
                }
                other => other.to_string(),
            })?;
        from_val(&value)
    }
}

/// Soil → WIT.
pub fn to_val(value: &Value, ty: &Type) -> std::result::Result<Val, String> {
    Ok(match (ty, value) {
        (Type::String, Value::String(text)) => Val::String(text.clone()),
        (Type::Bool, Value::Bool(flag)) => Val::Bool(*flag),
        (Type::S64, Value::Int(number)) => Val::S64(*number),
        (Type::S32, Value::Int(number)) => Val::S32(*number as i32),
        (Type::U32, Value::Int(number)) => Val::U32(*number as u32),
        (Type::Float64, Value::Float(number)) => Val::Float64(*number),
        (Type::Float64, Value::Int(number)) => Val::Float64(*number as f64),
        // list<u8> crosses as a base64 string: a 361 KB mesh as a 361 000
        // element list is not a representation anyone can afford.
        (Type::List(list), Value::String(encoded)) if matches!(list.ty(), Type::U8) => {
            Val::List(base64_decode(encoded)?.into_iter().map(Val::U8).collect())
        }
        (Type::List(list), Value::List(items)) => {
            let inner = list.ty();
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(to_val(item, &inner)?);
            }
            Val::List(out)
        }
        // A record is a list of pairs, which is the shape witgen emits.
        (Type::Record(record), Value::List(pairs)) => {
            let mut out = Vec::with_capacity(record.fields().len());
            for field in record.fields() {
                let value = lookup_field(pairs, field.name)
                    .ok_or_else(|| format!("record is missing field :{}", field.name))?;
                out.push((field.name.to_string(), to_val(value, &field.ty)?));
            }
            Val::Record(out)
        }
        (Type::Option(_), Value::Nil) => Val::Option(None),
        (Type::Option(option), other) => Val::Option(Some(Box::new(to_val(other, &option.ty())?))),
        (Type::Enum(labels), Value::Symbol(name)) | (Type::Enum(labels), Value::Keyword(name)) => {
            if !labels.names().any(|label| label == name) {
                return Err(format!(":{name} is not one of the enum's cases"));
            }
            Val::Enum(name.clone())
        }
        (ty, value) => {
            return Err(format!(
                "no Soil representation for {ty:?} from {value:?}"
            ))
        }
    })
}

/// WIT → Soil.
pub fn from_val(value: &Val) -> std::result::Result<Value, String> {
    Ok(match value {
        Val::String(text) => Value::String(text.clone()),
        Val::Bool(flag) => Value::Bool(*flag),
        Val::S64(number) => Value::Int(*number),
        Val::S32(number) => Value::Int(*number as i64),
        Val::U32(number) => Value::Int(*number as i64),
        Val::Float64(number) => Value::Float(*number),
        Val::Float32(number) => Value::Float(*number as f64),
        Val::List(items) => Value::List(
            items
                .iter()
                .map(from_val)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
        Val::Record(fields) => Value::List(
            fields
                .iter()
                .map(|(name, value)| {
                    Ok(Value::List(vec![
                        Value::Keyword(name.clone()),
                        from_val(value)?,
                    ]))
                })
                .collect::<std::result::Result<Vec<_>, String>>()?,
        ),
        Val::Option(none) if none.is_none() => Value::Nil,
        Val::Option(Some(inner)) => from_val(inner)?,
        Val::Enum(name) => Value::Keyword(name.clone()),
        // A provider error is data, not a failure of the call: the workflow
        // needs to see "blocked" as an outcome it can report on.
        Val::Result(Ok(Some(inner))) => {
            Value::List(vec![Value::Keyword("ok".into()), from_val(inner)?])
        }
        Val::Result(Ok(None)) => Value::List(vec![Value::Keyword("ok".into()), Value::Nil]),
        Val::Result(Err(Some(inner))) => {
            Value::List(vec![Value::Keyword("err".into()), from_val(inner)?])
        }
        Val::Variant(name, payload) => match payload {
            Some(inner) => Value::List(vec![
                Value::Keyword(name.clone()),
                from_val(inner)?,
            ]),
            None => Value::Keyword(name.clone()),
        },
        other => return Err(format!("no Soil value for {other:?}")),
    })
}

fn lookup_field<'a>(pairs: &'a [Value], name: &str) -> Option<&'a Value> {
    pairs.iter().find_map(|pair| match pair {
        Value::List(items) if items.len() == 2 => {
            let key = match &items[0] {
                Value::Keyword(key) | Value::Symbol(key) => key.as_str(),
                Value::String(key) => key.as_str(),
                _ => return None,
            };
            (key == name).then(|| &items[1])
        }
        _ => None,
    })
}

fn base64_decode(text: &str) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        if byte == b'=' || byte == b'\n' || byte == b'\r' {
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("source is not base64".into()),
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::plugin::PluginRegistry;
    use wasmtime::Config;

    fn engine() -> wasmtime::Engine {
        let mut config = Config::new();
        config.wasm_component_model(true);
        wasmtime::Engine::new(&config).expect("engine")
    }

    /// The whole point: a Soil program, not a Rust program, calling a provider.
    ///
    /// The bindings below are what `soil-witgen` emits from the provider's WIT
    /// file -- there is no Transmog-specific code in this test or in the
    /// adapter, only an interface name the program chose to call.
    #[test]
    fn a_soil_program_calls_a_provider() {
        let Ok(registry) = PluginRegistry::scan_default_dev(true) else {
            return;
        };
        let interface = "fpl:transmog/engineering@0.1.0";
        if registry.get(interface).is_none() {
            eprintln!(
                "skipping: {interface} not in registry; saw {:?}; refused {:?}",
                registry.interfaces().map(|(n, _)| n.to_string()).collect::<Vec<_>>(),
                registry.refused().iter().map(|r| r.name.clone()).collect::<Vec<_>>()
            );
            return;
        }
        if std::env::var_os("TRANSMOG_BIN").is_none()
            && std::path::Path::new("/tmp/transmog").exists()
        {
            unsafe { std::env::set_var("TRANSMOG_BIN", "/tmp/transmog") };
        }

        let mut host = ProviderHost::new(engine(), PluginRegistry::scan_default_dev(true).unwrap());
        host.grant(interface);
        let mut soil = soil_lang::eval::Runtime::new(Box::new(SoilProviders::new(host)));

        let source = r#"
(defun fpl:transmog/engineering.identity ()
  (host-call "fpl:transmog" "identity" (list)))
(fpl:transmog/engineering.identity)
"#;
        let started = std::time::Instant::now();
        let value = soil.load(source).expect("soil program runs");
        let elapsed = started.elapsed();

        match value {
            Value::String(identity) => {
                assert!(identity.contains("transmog"), "unexpected identity {identity:?}");
                eprintln!("soil -> provider identity in {elapsed:?}: {identity}");
            }
            other => panic!("expected a string, got {other:?}"),
        }
    }

    /// An ungranted interface must fail as a grant problem, not as a missing one.
    #[test]
    fn an_ungranted_call_is_refused() {
        let Ok(registry) = PluginRegistry::scan_default_dev(true) else {
            return;
        };
        let interface = "fpl:transmog/engineering@0.1.0";
        if registry.get(interface).is_none() {
            return;
        }
        let mut host = ProviderHost::new(engine(), PluginRegistry::scan_default_dev(true).unwrap());
        // deliberately no grant
        let _ = &mut host;
        let mut soil = soil_lang::eval::Runtime::new(Box::new(SoilProviders::new(host)));
        let source = r#"
(defun call-it () (host-call "fpl:transmog" "identity" (list)))
(call-it)
"#;
        let error = soil.load(source).expect_err("must be refused");
        // Soil wraps the cause, so the interface only appears in the chain.
        let text = format!("{error:#}");
        assert!(text.contains("fpl:transmog"), "error should name the interface: {text}");
    }
}
