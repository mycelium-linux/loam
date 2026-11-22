// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native capability host.
//!
//! This is the in-process REPL/runner adapter. It does not implement
//! `soil:vfs`; it calls Soil's existing `VfsHost`, and delegates records and
//! time to the same native hosts the Wasm host uses.

use std::{sync::Arc, time::Duration};

#[cfg(unix)]
use soil::creds::{GeodeCredentials, HostHandle};
use soil::http_host::{
    BearerCredential, CredentialProvider, Error as HttpError, HttpHost, HttpPolicy,
    Method as HttpMethod, Request as HttpRequest,
};
use soil::records_host::{NamespacedRecordsHost, records};
use soil::vfs_host::vfs::Host as VfsHostTrait;
use soil::vfs_host::{VfsHost, vfs};

use crate::Host;
use crate::sexp::Value;

pub struct NativeHost {
    vfs: VfsHost,
    records: NamespacedRecordsHost,
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
            records: NamespacedRecordsHost::open_dynamic(records_root, book)?,
            http: None,
        })
    }

    /// Grant `soil:http` for the named origins, leasing path credentials from
    /// the Geode daemon at `socket`.
    ///
    /// The guest still names only the handle `default`. The reference, the
    /// purpose and the choice of path substitution stay host configuration, so
    /// a Soil program cannot address another Geode slot by spelling it.
    #[cfg(unix)]
    pub fn with_geode_http(
        mut self,
        origins: impl IntoIterator<Item = String>,
        socket: impl Into<std::path::PathBuf>,
        reference: impl Into<String>,
        purpose: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let purpose = purpose.into();
        let policy = HttpPolicy::new(origins, 16 * 1024 * 1024, Duration::from_secs(30))?;
        let credentials =
            GeodeCredentials::new(socket, "", purpose.clone(), Duration::from_secs(60))
                .with_host_handle(
                    "default",
                    HostHandle {
                        reference: reference.into(),
                        purpose,
                        header: None,
                    },
                );
        self.http = Some(HttpHost::new(policy).with_credentials(Arc::new(credentials)));
        Ok(self)
    }

    /// Grant `soil:http` for the named origins with one host-owned bearer token.
    ///
    /// The guest names the handle `default`; the host injects the
    /// `authorization` header. The token never enters the Soil program.
    pub fn with_bearer_http(
        mut self,
        origins: impl IntoIterator<Item = String>,
        token: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let policy = HttpPolicy::new(origins, 16 * 1024 * 1024, Duration::from_secs(30))?;
        let credentials = Arc::new(BearerCredential::new(token));
        self.http = Some(HttpHost::new(policy).with_credentials(credentials));
        Ok(self)
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

    fn call_time(&mut self, function: &str, args: &[Value]) -> std::result::Result<Value, String> {
        match function {
            "monotonic-ms" => Ok(ok(Value::Int(soil::time::monotonic_ms() as i64))),
            "wall-clock" => {
                let clock = soil::time::wall_clock().map_err(|_| "not-ready".to_string())?;
                Ok(ok(list(vec![
                    pair("unix-ms", Value::Int(clock.unix_ms as i64)),
                    pair("offset-minutes", Value::Int(clock.offset_minutes as i64)),
                    pair("year", Value::Int(clock.year as i64)),
                    pair("month", Value::Int(clock.month as i64)),
                    pair("day", Value::Int(clock.day as i64)),
                    pair("hour", Value::Int(clock.hour as i64)),
                    pair("minute", Value::Int(clock.minute as i64)),
                    pair("second", Value::Int(clock.second as i64)),
                    pair("weekday", Value::Int(clock.weekday as i64)),
                    pair("zone", clock.zone.map(Value::String).unwrap_or(Value::Nil)),
                ])))
            }
            "tick" => {
                let timeout = args.first().and_then(Value::as_f64).unwrap_or(0.0) as u32;
                if timeout > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(u64::from(
                        timeout.min(50),
                    )));
                }
                Ok(ok(Value::Int(soil::time::monotonic_ms() as i64)))
            }
            other => Err(format!("unknown soil:time function `{other}`")),
        }
    }

    fn call_records(
        &mut self,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        match function {
            "get" => {
                let key = records_key(args.first().ok_or("get needs a key")?)?;
                match records::Host::get(&mut self.records, key) {
                    Ok(entry) => Ok(ok(records_entry(entry))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "scan" => {
                let collection = text(args.first()).unwrap_or_default();
                let cursor = args.get(1).and_then(records_option);
                let limit = args.get(2).and_then(Value::as_f64).unwrap_or(50.0) as u32;
                match records::Host::scan(&mut self.records, collection, cursor, limit) {
                    Ok(page) => Ok(ok(records_page(page))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "query" => {
                let spec = records_query_spec(args.first().ok_or("query needs a spec")?)?;
                match records::Host::query(&mut self.records, spec) {
                    Ok(page) => Ok(ok(records_page(page))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "aggregate" => {
                let collection = text(args.first()).unwrap_or_default();
                let spec = records_aggregate_spec(args.get(1).ok_or("aggregate needs a spec")?)?;
                match records::Host::aggregate(&mut self.records, collection, spec) {
                    Ok(groups) => Ok(ok(list(groups.into_iter().map(records_group).collect()))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "upsert" => {
                let entry = records_parse_entry(args.first().ok_or("upsert needs an entry")?)?;
                match records::Host::upsert(&mut self.records, entry) {
                    Ok(entry) => Ok(ok(records_entry(entry))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "delete" => {
                let key = records_key(args.first().ok_or("delete needs a key")?)?;
                match records::Host::delete(&mut self.records, key) {
                    Ok(()) => Ok(ok(Value::Nil)),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            "compare-and-swap" => {
                let key = records_key(args.first().ok_or("compare-and-swap needs a key")?)?;
                let expected = args.get(1).and_then(records_version);
                let entry =
                    records_parse_entry(args.get(2).ok_or("compare-and-swap needs an entry")?)?;
                match records::Host::compare_and_swap(&mut self.records, key, expected, entry) {
                    Ok(result) => Ok(ok(list(vec![
                        pair("applied", Value::Bool(result.applied)),
                        pair("entry", records_entry(result.entry)),
                    ]))),
                    Err(error) => Ok(err(records_error(error))),
                }
            }
            other => Err(format!("unknown soil:records function `{other}`")),
        }
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
    fn read_source(&mut self, path: &str) -> std::result::Result<String, String> {
        let entry = VfsHostTrait::stat(&mut self.vfs, path.to_string())
            .map_err(|error| format!("stat {path}: {error:?}"))?;
        if entry.size > 16 * 1024 * 1024 {
            return Err(format!("source {path} is too large: {} bytes", entry.size));
        }
        let length = u32::try_from(entry.size).map_err(|_| "source is too large".to_string())?;
        let chunk = VfsHostTrait::read(&mut self.vfs, path.to_string(), 0, length)
            .map_err(|error| format!("read {path}: {error:?}"))?;
        String::from_utf8(chunk.data).map_err(|_| format!("source {path} is not UTF-8"))
    }

    fn write_source(&mut self, path: &str, source: &str) -> std::result::Result<(), String> {
        let mut writer = self
            .vfs
            .begin_writer(
                path.to_string(),
                "text/x-soil".to_string(),
                Some(source.len() as u64),
                None,
                None,
            )
            .map_err(|error| format!("begin-write {path}: {error:?}"))?;
        writer
            .write(source.as_bytes())
            .map_err(|error| format!("write {path}: {error:?}"))?;
        writer
            .finish()
            .map_err(|error| format!("finish {path}: {error:?}"))?;
        Ok(())
    }

    fn granted(&self, interface: &str, function: &str) -> bool {
        match interface {
            "soil:time" => {
                function == "monotonic-ms" || function == "wall-clock" || function == "tick"
            }
            "soil:records" => matches!(
                function,
                "get" | "scan" | "query" | "aggregate" | "upsert" | "delete" | "compare-and-swap"
            ),
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
            "soil:time" => self.call_time(function, args),
            "soil:records" => self.call_records(function, args),
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
    value.and_then(|value| match value {
        Value::Nil => None,
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => Some(text.clone()),
        other => Some(other.to_string()),
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

fn owned_text(value: Value) -> Option<String> {
    text(Some(&value))
}

fn list(values: Vec<Value>) -> Value {
    Value::List(values)
}

fn records_key(value: &Value) -> std::result::Result<records::Key, String> {
    Ok(records::Key {
        collection: owned_text(field_value(value, "collection")).unwrap_or_default(),
        id: owned_text(field_value(value, "id")).unwrap_or_default(),
    })
}

fn records_parse_entry(value: &Value) -> std::result::Result<records::Entry, String> {
    Ok(records::Entry {
        key: records_key(&field_value(value, "key"))?,
        schema: owned_text(field_value(value, "schema")).unwrap_or_default(),
        revision: field_value(value, "revision").as_f64().unwrap_or(0.0) as u64,
        updated_at_ms: field_value(value, "updated-at-ms").as_f64().unwrap_or(0.0) as u64,
        origin: owned_text(field_value(value, "origin")).unwrap_or_default(),
        deleted: !matches!(
            field_value(value, "deleted"),
            Value::Nil | Value::Bool(false)
        ),
        body: body_bytes(&field_value(value, "body"))?,
    })
}

fn records_version(value: &Value) -> Option<records::Version> {
    if matches!(value, Value::Nil) {
        return None;
    }
    Some(records::Version {
        revision: field_value(value, "revision").as_f64().unwrap_or(0.0) as u64,
        origin: owned_text(field_value(value, "origin")).unwrap_or_default(),
    })
}

fn records_entry(entry: records::Entry) -> Value {
    list(vec![
        pair(
            "key",
            list(vec![
                pair("collection", Value::String(entry.key.collection)),
                pair("id", Value::String(entry.key.id)),
            ]),
        ),
        pair("schema", Value::String(entry.schema)),
        pair("revision", Value::Int(entry.revision as i64)),
        pair("updated-at-ms", Value::Int(entry.updated_at_ms as i64)),
        pair("origin", Value::String(entry.origin)),
        pair("deleted", Value::Bool(entry.deleted)),
        pair(
            "body",
            list(
                entry
                    .body
                    .into_iter()
                    .map(|byte| Value::Int(byte as i64))
                    .collect(),
            ),
        ),
    ])
}

fn records_page(page: records::Page) -> Value {
    records_page_values(
        page.entries.into_iter().map(records_entry).collect(),
        page.next,
    )
}

fn records_page_values(entries: Vec<Value>, next: Option<String>) -> Value {
    list(vec![
        pair("entries", list(entries)),
        pair("next", next.map(Value::String).unwrap_or(Value::Nil)),
    ])
}

fn records_option(value: &Value) -> Option<String> {
    match value {
        Value::Nil => None,
        value => text(Some(value)),
    }
}

fn body_bytes(value: &Value) -> std::result::Result<Vec<u8>, String> {
    match value {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        Value::List(bytes) => bytes
            .iter()
            .map(|byte| match byte {
                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                _ => Err("body must be bytes".into()),
            })
            .collect(),
        _ => Err("body must be bytes or a string".into()),
    }
}

fn records_filter(value: &Value) -> std::result::Result<records::Filter, String> {
    let field = owned_text(field_value(value, "field")).unwrap_or_default();
    let op = match owned_text(field_value(value, "op")).as_deref() {
        Some("eq") => records::FilterOp::Eq,
        Some("prefix") => records::FilterOp::Prefix,
        Some("contains") => records::FilterOp::Contains,
        Some("exists") => records::FilterOp::Exists,
        Some("lt") => records::FilterOp::Lt,
        Some("lte") => records::FilterOp::Lte,
        Some("gt") => records::FilterOp::Gt,
        Some("gte") => records::FilterOp::Gte,
        other => return Err(format!("unknown filter op `{}`", other.unwrap_or(""))),
    };
    Ok(records::Filter {
        field,
        op,
        value: records_option(&field_value(value, "value")),
    })
}

fn records_text_list(value: Value) -> Vec<String> {
    value
        .as_list()
        .unwrap_or(&[])
        .iter()
        .filter_map(|value| text(Some(value)).filter(|text| !text.is_empty()))
        .collect()
}

fn records_query_spec(value: &Value) -> std::result::Result<records::QuerySpec, String> {
    let filters = field_value(value, "filters")
        .as_list()
        .unwrap_or(&[])
        .iter()
        .map(records_filter)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(records::QuerySpec {
        collection: owned_text(field_value(value, "collection")).unwrap_or_default(),
        prefix: records_option(&field_value(value, "prefix")),
        filters,
        fields: records_text_list(field_value(value, "fields")),
        cursor: records_option(&field_value(value, "cursor")),
        limit: field_value(value, "limit")
            .as_f64()
            .unwrap_or(50.0)
            .max(1.0) as u32,
    })
}

fn records_aggregate_spec(value: &Value) -> std::result::Result<records::AggregateSpec, String> {
    let filters = field_value(value, "filters")
        .as_list()
        .unwrap_or(&[])
        .iter()
        .map(records_filter)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(records::AggregateSpec {
        collection: owned_text(field_value(value, "collection")).unwrap_or_default(),
        prefix: records_option(&field_value(value, "prefix")),
        filters,
        group_by: records_text_list(field_value(value, "group-by")),
        bucket_field: records_option(&field_value(value, "bucket-field")),
        bucket_seconds: field_value(value, "bucket-seconds").as_f64().unwrap_or(0.0) as u32,
    })
}

fn records_group(group: records::AggregateGroup) -> Value {
    list(vec![
        pair(
            "values",
            list(group.values.into_iter().map(Value::String).collect()),
        ),
        pair("count", Value::Int(group.count as i64)),
    ])
}

fn field_value(record: &Value, name: &str) -> Value {
    let Some(items) = record.as_list() else {
        return Value::Nil;
    };
    for item in items {
        let Some(pair) = item.as_list() else { continue };
        let key = pair
            .first()
            .and_then(|value| value.as_keyword().or_else(|| value.as_symbol()));
        if key == Some(name) {
            return pair.get(1).cloned().unwrap_or(Value::Nil);
        }
    }
    Value::Nil
}

fn records_error(error: records::Error) -> &'static str {
    match error {
        records::Error::Denied => "denied",
        records::Error::NotFound => "not-found",
        records::Error::OverBudget => "over-budget",
        records::Error::Invalid => "invalid",
        records::Error::Stale => "stale",
        records::Error::Io => "io",
        records::Error::Unsupported => "unsupported",
    }
}
