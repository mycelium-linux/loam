//! The host side of `loam:plugin/ipc`.
//!
//! A wasm adapter exports a capability WIT and imports this narrow request
//! surface. The host owns the sidecar process; the adapter never sees a pid,
//! a socket, or an ambient environment.

use anyhow::{Context, Result, bail};
use wasmtime::component::{HasSelf, Linker};

wasmtime::component::bindgen!({
    path: "wit/plugin",
    world: "host",
});

pub use self::loam::plugin::ipc;

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
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl Sidecar {
    pub fn spawn(program: &str, args: &[String], cwd: Option<&std::path::Path>) -> Result<Self> {
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
            output: std::io::BufReader::new(output),
        })
    }
}

impl Backend for Sidecar {
    fn request(&mut self, method: &str, payload: &[u8]) -> Result<Vec<u8>, ipc::Error> {
        use std::io::{BufRead, Write};

        let payload: serde_json::Value = if payload.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(payload).map_err(|_| ipc::Error::Invalid)?
        };
        let request = serde_json::json!({ "method": method, "payload": payload });
        serde_json::to_writer(&mut self.input, &request).map_err(|_| ipc::Error::Io)?;
        self.input.write_all(b"\n").map_err(|_| ipc::Error::Io)?;
        self.input.flush().map_err(|_| ipc::Error::Io)?;
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .map_err(|_| ipc::Error::Io)?;
        if line.is_empty() {
            return Err(ipc::Error::Unavailable);
        }
        let response: serde_json::Value =
            serde_json::from_str(&line).map_err(|_| ipc::Error::Invalid)?;
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
        let payload = response
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_vec(&payload).map_err(|_| ipc::Error::Io)
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
    if method.is_empty() {
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
