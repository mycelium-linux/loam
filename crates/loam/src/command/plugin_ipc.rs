//! The host side of `loam:plugin/ipc`.
//!
//! A wasm adapter exports a capability WIT and imports this narrow request
//! surface. The host owns the sidecar process; the adapter never sees a pid,
//! a socket, or an ambient environment.

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result, bail};
use wasmtime::component::{HasSelf, Linker};

wasmtime::component::bindgen!({
    path: "wit/plugin",
    world: "host",
});

pub use self::loam::plugin::ipc;

/// Largest request or response line Loam will read from a sidecar.
///
/// A plugin cannot make the host allocate without bound by writing an
/// unterminated line. `Sidecar::spawn_with_limit` exists for tests that need a
/// smaller bound.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// One sidecar invocation. Implementations are host-owned.
pub trait Backend: Send {
    fn request(&mut self, method: &str, payload: &[u8]) -> Result<Vec<u8>, ipc::Error>;
}

/// Register `loam:plugin/ipc`. Absent a backend, calls answer `unavailable`.
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> Result<()>
where
    T: ipc::Host + 'static,
{
    ipc::add_to_linker::<T, HasSelf<T>>(linker, |state| state)?;
    Ok(())
}

/// A line-delimited JSON sidecar.
pub struct Sidecar {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
    limit: usize,
}

impl Sidecar {
    pub fn spawn(program: &str, args: &[String], cwd: Option<&std::path::Path>) -> Result<Self> {
        Self::spawn_with_limit(program, args, cwd, MAX_LINE_BYTES)
    }

    pub fn spawn_with_limit(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        limit: usize,
    ) -> Result<Self> {
        if limit == 0 {
            bail!("sidecar IPC line limit must be positive");
        }
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn plugin sidecar {program}"))?;
        let input = child.stdin.take().context("sidecar stdin")?;
        let output = child.stdout.take().context("sidecar stdout")?;
        Ok(Self {
            child,
            input,
            output: BufReader::new(output),
            limit,
        })
    }
}

impl Backend for Sidecar {
    fn request(&mut self, method: &str, payload: &[u8]) -> Result<Vec<u8>, ipc::Error> {
        if !valid_method(method) {
            return Err(ipc::Error::Invalid);
        }
        let payload: serde_json::Value = if payload.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(payload).map_err(|_| ipc::Error::Invalid)?
        };
        // `payload` is canonical. `params` is the Drawbridge-compatible alias:
        // sending both costs one duplicate key and lets an existing host keep
        // working while new hosts read `payload`.
        let request = serde_json::json!({
            "method": method,
            "payload": payload,
            "params": payload,
        });
        let request = serde_json::to_vec(&request).map_err(|_| ipc::Error::Io)?;
        if request.len() + 1 > self.limit {
            return Err(ipc::Error::Invalid);
        }
        self.input.write_all(&request).map_err(|_| ipc::Error::Io)?;
        self.input.write_all(b"\n").map_err(|_| ipc::Error::Io)?;
        self.input.flush().map_err(|_| ipc::Error::Io)?;
        let line = read_bounded_line(&mut self.output, self.limit)?;
        let response: serde_json::Value =
            serde_json::from_slice(&line).map_err(|_| ipc::Error::Invalid)?;
        if response.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
            return Err(
                match response.get("error").and_then(serde_json::Value::as_str) {
                    Some("denied") => ipc::Error::Denied,
                    Some("invalid") => ipc::Error::Invalid,
                    Some("io") => ipc::Error::Io,
                    _ => ipc::Error::Unavailable,
                },
            );
        }
        // `payload` is canonical; `result` is accepted from older sidecars.
        let payload = response
            .get("payload")
            .or_else(|| response.get("result"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_vec(&payload).map_err(|_| ipc::Error::Io)
    }
}

/// Throw away the rest of an oversized frame so the next request starts at a
/// line boundary. The sidecar stays alive.
fn discard_line<R: BufRead>(reader: &mut R) -> Result<(), ipc::Error> {
    loop {
        let buffer = reader.fill_buf().map_err(|_| ipc::Error::Io)?;
        if buffer.is_empty() {
            return Ok(());
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(newline + 1);
            return Ok(());
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

fn valid_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= 128
        && method
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

/// Read one newline-delimited frame without letting a sidecar force an
/// unbounded allocation.
fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> Result<Vec<u8>, ipc::Error> {
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(|_| ipc::Error::Io)?;
        if buffer.is_empty() {
            return if out.is_empty() {
                Err(ipc::Error::Unavailable)
            } else {
                Err(ipc::Error::Io)
            };
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            if out.len() + newline > limit {
                reader.consume(newline + 1);
                return Err(ipc::Error::Invalid);
            }
            out.extend_from_slice(&buffer[..newline]);
            reader.consume(newline + 1);
            return Ok(out);
        }
        if out.len() + buffer.len() > limit {
            let consumed = buffer.len();
            reader.consume(consumed);
            discard_line(reader)?;
            return Err(ipc::Error::Invalid);
        }
        out.extend_from_slice(buffer);
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Dispatch `loam:plugin/ipc` calls to the configured backend.
pub fn handle(
    backend: &mut Option<Box<dyn Backend>>,
    method: &str,
    payload: &[u8],
) -> Result<Vec<u8>, ipc::Error> {
    if !valid_method(method) {
        return Err(ipc::Error::Invalid);
    }
    match backend {
        Some(backend) => backend.request(method, payload),
        None => Err(ipc::Error::Unavailable),
    }
}

/// A small helper for callers that need the WIT error as a string.
pub fn describe(error: ipc::Error) -> &'static str {
    match error {
        ipc::Error::Denied => "denied",
        ipc::Error::Unavailable => "unavailable",
        ipc::Error::Invalid => "invalid",
        ipc::Error::Io => "io",
    }
}

/// Fail closed when a package names a sidecar that is missing.
pub fn require_sidecar(program: &str) -> Result<&str> {
    if program.trim().is_empty() {
        bail!("sidecar command is empty");
    }
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(script: &str, limit: usize) -> Sidecar {
        Sidecar::spawn_with_limit("sh", &["-c".to_string(), script.to_string()], None, limit)
            .expect("spawn test sidecar")
    }

    #[test]
    fn canonical_payload_and_result_alias_cross_the_wire() {
        let mut sidecar = sidecar(
            r#"while IFS= read -r line; do
                 if printf '%s' "$line" | grep -q '"payload":' && printf '%s' "$line" | grep -q '"params":'; then
                   printf '%s\n' '{"ok":true,"result":{"seen":true}}'
                 else
                   printf '%s\n' '{"ok":false,"error":"invalid"}'
                 fi
               done"#,
            MAX_LINE_BYTES,
        );
        let reply = sidecar.request("poll", br#"{"x":1}"#).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"seen": true})
        );
    }

    #[test]
    fn malformed_and_oversized_frames_do_not_kill_the_sidecar() {
        let mut sidecar = sidecar(
            r#"while IFS= read -r line; do
                 if printf '%s' "$line" | grep -q '"method":"bad"'; then
                   printf '%s\n' 'not json'
                 elif printf '%s' "$line" | grep -q '"method":"large"'; then
                   printf '%0100d\n' 0
                 else
                   printf '%s\n' '{"ok":true,"payload":{"still":"alive"}}'
                 fi
               done"#,
            64,
        );
        assert_eq!(sidecar.request("bad", b"null"), Err(ipc::Error::Invalid));
        assert_eq!(sidecar.request("large", b"null"), Err(ipc::Error::Invalid));
        assert_eq!(
            sidecar.request("request-too-large", br#"{"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#),
            Err(ipc::Error::Invalid)
        );
        assert_eq!(sidecar.request("bad-json", b"{"), Err(ipc::Error::Invalid));
        let reply = sidecar.request("good", b"null").unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"still": "alive"})
        );
    }

    #[test]
    fn eof_and_error_names_map_to_unavailable() {
        let mut eof = sidecar("exit 0", MAX_LINE_BYTES);
        assert_eq!(eof.request("poll", b"null"), Err(ipc::Error::Unavailable));

        let mut denied = sidecar(
            "read line; printf '%s\\n' '{\"ok\":false,\"error\":\"denied\"}'",
            MAX_LINE_BYTES,
        );
        assert_eq!(denied.request("poll", b"null"), Err(ipc::Error::Denied));

        let mut invalid = sidecar(
            "read line; printf '%s\\n' '{\"ok\":false,\"error\":\"invalid\"}'",
            MAX_LINE_BYTES,
        );
        assert_eq!(invalid.request("poll", b"null"), Err(ipc::Error::Invalid));
    }

    #[test]
    fn empty_and_unknown_shaped_methods_are_invalid() {
        assert!(!valid_method(""));
        assert!(!valid_method("bad method"));
        assert!(valid_method("channel.list"));
        let mut backend: Option<Box<dyn Backend>> = None;
        assert_eq!(handle(&mut backend, "", b"null"), Err(ipc::Error::Invalid));
    }
}
