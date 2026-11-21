// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native capability host.
//!
//! This is the in-process REPL/runner adapter. It does not implement
//! `soil:vfs`; it calls Soil's existing `VfsHost`, and delegates records and
//! time to the same native hosts the Wasm host uses.

use std::{sync::Arc, time::Duration};

use soil::http_host::{
    CredentialProvider, Error as HttpError, HttpHost, HttpPolicy, Method as HttpMethod,
    Request as HttpRequest,
};
use soil::vfs_host::vfs::Host as VfsHostTrait;
use soil::vfs_host::{VfsHost, vfs};

use crate::sexp::Value;
use crate::{Host, SoilRecordsHost, SystemTimeHost};

pub struct NativeHost {
    vfs: VfsHost,
    records: SoilRecordsHost,
    time: SystemTimeHost,
    http: Option<HttpHost>,
}

impl NativeHost {
    pub fn open(
        workspace_root: impl AsRef<std::path::Path>,
        records_root: impl AsRef<std::path::Path>,
        book: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            vfs: VfsHost::open(workspace_root)?,
            records: SoilRecordsHost::open(records_root, book)?,
            time: SystemTimeHost,
            http: None,
        })
    }

    /// Grant `soil:http` for the named origins.
    ///
    /// When `path_credential` is set, a request may put `__CREDENTIAL__` in one
    /// URL path segment and supply the guest handle `default`; the host
    /// substitutes the secret before the request leaves the process.
    pub fn with_http(
        mut self,
        origins: impl IntoIterator<Item = String>,
        path_credential: Option<String>,
    ) -> anyhow::Result<Self> {
        let policy = HttpPolicy::new(origins, 16 * 1024 * 1024, Duration::from_secs(30))?;
        let mut http = HttpHost::new(policy);
        if let Some(secret) = path_credential {
            http = http.with_credentials(Arc::new(PathCredential { secret }));
        }
        self.http = Some(http);
        Ok(self)
    }

    fn call_http(&mut self, function: &str, args: &[Value]) -> std::result::Result<Value, String> {
        if function != "fetch" {
            return Err(format!("unknown soil:http function `{function}`"));
        }
        let Some(http) = self.http.as_mut() else {
            return Err("soil:http is not granted to this host".into());
        };
        let request = args
            .first()
            .ok_or_else(|| "soil:http/fetch needs a request".to_string())?;
        let request = http_request(request)?;
        Ok(match http.perform(request) {
            Ok(response) => ok(http_response(response)),
            Err(error) => err(http_error(error)),
        })
    }

    fn call_vfs(&mut self, function: &str, args: &[Value]) -> std::result::Result<Value, String> {
        match function {
            "stat" => {
                let path = text(args.first()).unwrap_or_default();
                match VfsHostTrait::stat(&mut self.vfs, path) {
                    Ok(entry) => Ok(ok(vfs_entry(entry))),
                    Err(error) => Ok(vfs_error(error)),
                }
            }
            "ls" => {
                let parent = text(args.first()).unwrap_or_default();
                let offset = args.get(1).and_then(Value::as_f64).unwrap_or(0.0) as u64;
                let limit = args.get(2).and_then(Value::as_f64).unwrap_or(100.0) as u32;
                match VfsHostTrait::ls(&mut self.vfs, parent, offset, limit) {
                    Ok(page) => Ok(ok(vfs_page(page))),
                    Err(error) => Ok(vfs_error(error)),
                }
            }
            "read" => {
                let path = text(args.first()).unwrap_or_default();
                let offset = args.get(1).and_then(Value::as_f64).unwrap_or(0.0) as u64;
                let length = args.get(2).and_then(Value::as_f64).unwrap_or(0.0) as u32;
                match VfsHostTrait::read(&mut self.vfs, path, offset, length) {
                    Ok(chunk) => Ok(ok(vfs_chunk(chunk))),
                    Err(error) => Ok(vfs_error(error)),
                }
            }
            other => Err(format!("unknown soil:vfs function `{other}`")),
        }
    }
}

impl Host for NativeHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        match interface {
            "soil:time" => self.time.granted(interface, function),
            "soil:records" => self.records.granted(interface, function),
            "soil:vfs" => matches!(function, "ls" | "stat" | "read"),
            "soil:http" => self.http.is_some() && function == "fetch",
            _ => false,
        }
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        match interface {
            "soil:time" => self.time.call(interface, function, args),
            "soil:records" => self.records.call(interface, function, args),
            "soil:vfs" => self.call_vfs(function, args),
            "soil:http" => self.call_http(function, args),
            other => Err(format!("unknown interface `{other}`")),
        }
    }
}

fn vfs_entry(entry: vfs::Entry) -> Value {
    Value::List(vec![
        pair("path", Value::String(entry.path)),
        pair("name", Value::String(entry.name)),
        pair(
            "kind",
            Value::Keyword(
                match entry.kind {
                    vfs::Kind::File => "file",
                    vfs::Kind::Directory => "directory",
                }
                .into(),
            ),
        ),
        pair("size", Value::Int(entry.size as i64)),
        pair("content-id", Value::String(entry.content_id)),
        pair("media-type", Value::String(entry.media_type)),
        pair("version", Value::Int(entry.version as i64)),
        pair(
            "hydration",
            Value::Keyword(
                match entry.hydration {
                    vfs::Hydration::Present => "present",
                    vfs::Hydration::Placeholder => "placeholder",
                }
                .into(),
            ),
        ),
        pair("pinned", Value::Bool(entry.pinned)),
        pair("updated-at-ms", Value::Int(entry.updated_at_ms as i64)),
        pair("mode", Value::Int(entry.mode as i64)),
        pair("child-count", Value::Int(entry.child_count as i64)),
    ])
}

fn vfs_page(page: vfs::Page) -> Value {
    Value::List(vec![
        pair(
            "entries",
            Value::List(page.entries.into_iter().map(vfs_entry).collect()),
        ),
        pair(
            "next",
            page.next
                .map(|next| Value::Int(next as i64))
                .unwrap_or(Value::Nil),
        ),
    ])
}

fn vfs_chunk(chunk: vfs::Chunk) -> Value {
    Value::List(vec![
        pair("offset", Value::Int(chunk.offset as i64)),
        pair(
            "data",
            Value::List(
                chunk
                    .data
                    .into_iter()
                    .map(|byte| Value::Int(byte as i64))
                    .collect(),
            ),
        ),
        pair("eof", Value::Bool(chunk.eof)),
        pair("content-id", Value::String(chunk.content_id)),
    ])
}

fn vfs_error(error: vfs::Error) -> Value {
    let code = match error {
        vfs::Error::Denied => "denied",
        vfs::Error::NotFound => "not-found",
        vfs::Error::NotDirectory => "not-directory",
        vfs::Error::IsDirectory => "is-directory",
        vfs::Error::NotHydrated => "not-hydrated",
        vfs::Error::OverBudget => "over-budget",
        vfs::Error::Invalid => "invalid",
        vfs::Error::Io => "io",
        vfs::Error::Stale => "stale",
    };
    err(code)
}

/// One path credential, exposed under the guest handle `default`.
///
/// The secret stays in the host process: the guest supplies a handle and a URL
/// containing `__CREDENTIAL__`, and `soil:http` substitutes the value.
struct PathCredential {
    secret: String,
}

impl CredentialProvider for PathCredential {
    fn resolve(&self, _handle: &str) -> anyhow::Result<Option<(String, String)>> {
        Ok(None)
    }

    fn resolve_path(&self, handle: &str) -> anyhow::Result<Option<String>> {
        Ok((handle == "default").then(|| self.secret.clone()))
    }
}

fn http_request(value: &Value) -> std::result::Result<HttpRequest, String> {
    let method = match record_text(value, "method").as_deref() {
        Some("get") => HttpMethod::Get,
        Some("head") => HttpMethod::Head,
        Some("post") => HttpMethod::Post,
        Some("put") => HttpMethod::Put,
        Some("patch") => HttpMethod::Patch,
        Some("delete") => HttpMethod::Delete,
        Some(other) => return Err(format!("soil:http: unknown method `{other}`")),
        None => return Err("soil:http: request has no method".into()),
    };
    let url = record_text(value, "url").ok_or("soil:http: request has no url")?;
    let headers = record_field(value, "headers")
        .as_list()
        .unwrap_or_default()
        .iter()
        .map(|header| (record_text(header, "name"), record_text(header, "value")))
        .map(|(name, value)| match (name, value) {
            (Some(name), Some(value)) => Ok((name, value)),
            _ => Err("soil:http: header needs a name and value".to_string()),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let body = match record_field(value, "body") {
        Value::Nil => None,
        bytes => Some(value_bytes(&bytes)?),
    };
    let credential = record_text(value, "credential");
    let max_bytes = record_u32(value, "max-bytes").unwrap_or(1024 * 1024);
    let timeout_ms = record_u32(value, "timeout-ms").unwrap_or(10_000);
    Ok(HttpRequest {
        url,
        method,
        headers,
        body,
        credential,
        max_bytes,
        timeout_ms,
    })
}

fn http_response(response: soil::http_host::Response) -> Value {
    Value::List(vec![
        pair("status", Value::Int(i64::from(response.status))),
        pair("final-url", Value::String(response.final_url)),
        pair(
            "headers",
            Value::List(
                response
                    .headers
                    .into_iter()
                    .map(|(name, value)| {
                        Value::List(vec![
                            pair("name", Value::String(name)),
                            pair("value", Value::String(value)),
                        ])
                    })
                    .collect(),
            ),
        ),
        pair(
            "body",
            Value::List(
                response
                    .body
                    .into_iter()
                    .map(|byte| Value::Int(i64::from(byte)))
                    .collect(),
            ),
        ),
    ])
}

fn http_error(error: HttpError) -> &'static str {
    match error {
        HttpError::Denied => "denied",
        HttpError::Invalid => "invalid",
        HttpError::OverBudget => "over-budget",
        HttpError::Timeout => "timeout",
        HttpError::Unavailable => "unavailable",
        HttpError::Tls => "tls",
        HttpError::Http => "http",
        HttpError::Io => "io",
        HttpError::Unsupported => "unsupported",
    }
}

fn record_field<'a>(record: &'a Value, name: &str) -> &'a Value {
    let Some(items) = record.as_list() else {
        return &Value::Nil;
    };
    for item in items {
        let Some(pair) = item.as_list() else { continue };
        let Some(field) = pair
            .first()
            .and_then(Value::as_keyword)
            .or_else(|| pair.first().and_then(Value::as_symbol))
        else {
            continue;
        };
        if field == name {
            return pair.get(1).unwrap_or(&Value::Nil);
        }
    }
    &Value::Nil
}

fn record_text(record: &Value, name: &str) -> Option<String> {
    text(Some(record_field(record, name)))
}

fn record_u32(record: &Value, name: &str) -> Option<u32> {
    let value = record_field(record, name).as_f64()?;
    (value >= 0.0 && value <= f64::from(u32::MAX)).then_some(value as u32)
}

fn value_bytes(value: &Value) -> std::result::Result<Vec<u8>, String> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                _ => Err("soil:http: body must be a list of bytes".to_string()),
            })
            .collect(),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => Err("soil:http: body must be a list of bytes or a string".into()),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.map(|value| match value {
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => text.clone(),
        other => other.to_string(),
    })
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
