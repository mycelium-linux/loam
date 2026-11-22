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
            "query" => Ok(ok(
                self.query_page(args.first().ok_or("query needs a spec")?)?
            )),
            "scan-query" => {
                let collection = text(args.first()).unwrap_or_default();
                let field = text(args.get(1)).unwrap_or_default();
                let value = text(args.get(2)).unwrap_or_default();
                let spec = list(vec![
                    pair("collection", Value::String(collection)),
                    pair("prefix", Value::Nil),
                    pair(
                        "filters",
                        list(vec![list(vec![
                            pair("field", Value::String(field)),
                            pair("op", Value::String("eq".into())),
                            pair("value", Value::String(value)),
                        ])]),
                    ),
                    pair("fields", Value::Nil),
                    pair("cursor", args.get(3).cloned().unwrap_or(Value::Nil)),
                    pair("limit", args.get(4).cloned().unwrap_or(Value::Int(50))),
                ]);
                Ok(ok(self.query_page(&spec)?))
            }
            "scan-fields" => {
                let collection = text(args.first()).unwrap_or_default();
                let prefix = text(args.get(1)).unwrap_or_default();
                let fields = args.get(2).and_then(Value::as_list).unwrap_or(&[]).to_vec();
                let spec = list(vec![
                    pair("collection", Value::String(collection)),
                    pair("prefix", Value::String(prefix)),
                    pair("filters", Value::Nil),
                    pair("fields", list(fields)),
                    pair("cursor", args.get(3).cloned().unwrap_or(Value::Nil)),
                    pair("limit", args.get(4).cloned().unwrap_or(Value::Int(50))),
                ]);
                Ok(ok(self.query_page(&spec)?))
            }
            "aggregate" => {
                let collection = text(args.first()).unwrap_or_default();
                let spec = args.get(1).cloned().unwrap_or(Value::Nil);
                Ok(ok(self.aggregate_page(&collection, &spec)?))
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

    fn query_page(&mut self, spec: &Value) -> std::result::Result<Value, String> {
        let collection = owned_text(field_value(spec, "collection")).unwrap_or_default();
        let prefix = owned_text(field_value(spec, "prefix")).unwrap_or_default();
        let filters = field_value(spec, "filters");
        let filters = filters
            .as_list()
            .unwrap_or(&[])
            .iter()
            .map(|filter| {
                serde_json::json!({
                    "field": owned_text(field_value(filter, "field")).unwrap_or_default(),
                    "op": owned_text(field_value(filter, "op")).unwrap_or_default(),
                    "value": owned_text(field_value(filter, "value")).unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        let fields = field_value(spec, "fields")
            .as_list()
            .unwrap_or(&[])
            .iter()
            .filter_map(|value| text(Some(value)).filter(|text| !text.is_empty()))
            .collect::<Vec<_>>();
        let mut cursor = records_option(&field_value(spec, "cursor"));
        let limit = field_value(spec, "limit").as_f64().unwrap_or(50.0) as usize;
        let limit = limit.max(1);
        let mut entries = Vec::with_capacity(limit);
        let mut next = None;
        loop {
            let page = records::Host::scan(&mut self.records, collection.clone(), cursor, 512)
                .map_err(|error| format!("{error:?}"))?;
            let page_len = page.entries.len();
            for entry in page.entries {
                let id = entry.key.id.clone();
                if !id.starts_with(&prefix) || !records_filters(&entry.body, &filters) {
                    cursor = Some(id);
                    continue;
                }
                entries.push(records_entry(project_entry(entry, &fields)));
                if entries.len() == limit {
                    next = Some(id);
                    break;
                }
                cursor = Some(id);
            }
            if entries.len() == limit || page_len < 512 {
                break;
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(records_page_values(entries, next))
    }

    fn aggregate_page(
        &mut self,
        collection: &str,
        spec: &Value,
    ) -> std::result::Result<Value, String> {
        let aggregate = aggregate_spec(spec)?;
        let prefix = aggregate
            .get("prefix")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let group_by = aggregate
            .get("group_by")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let filters = aggregate
            .get("filters")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let bucket = aggregate
            .get("bucket")
            .and_then(serde_json::Value::as_object);
        let bucket_field = bucket
            .and_then(|bucket| bucket.get("field"))
            .and_then(serde_json::Value::as_str);
        let bucket_seconds = bucket
            .and_then(|bucket| bucket.get("seconds"))
            .and_then(serde_json::Value::as_i64)
            .filter(|seconds| *seconds > 0);
        let mut groups: std::collections::BTreeMap<Vec<String>, u64> =
            std::collections::BTreeMap::new();
        let mut cursor = None;
        loop {
            let page = records::Host::scan(
                &mut self.records,
                collection.to_string(),
                cursor.clone(),
                512,
            )
            .map_err(|error| format!("{error:?}"))?;
            let page_len = page.entries.len();
            for entry in page.entries {
                let id = entry.key.id.clone();
                cursor = Some(id.clone());
                if !id.starts_with(prefix) || !records_filters(&entry.body, &filters) {
                    continue;
                }
                let fields: serde_json::Value =
                    serde_json::from_slice(&entry.body).map_err(|error| error.to_string())?;
                let mut key = group_by
                    .iter()
                    .map(|field| json_text(json_path(&fields, field)))
                    .collect::<Vec<_>>();
                if let (Some(field), Some(seconds)) = (bucket_field, bucket_seconds)
                    && let Some(value) = json_path(&fields, field).and_then(|value| value.as_i64())
                {
                    key.push((value.div_euclid(seconds) * seconds).to_string());
                }
                *groups.entry(key).or_default() += 1;
            }
            if page_len < 512 || cursor.is_none() {
                break;
            }
        }
        Ok(Value::List(
            groups
                .into_iter()
                .map(|(values, count)| {
                    list(vec![
                        pair(
                            "values",
                            list(values.into_iter().map(Value::String).collect()),
                        ),
                        pair("count", Value::Int(count as i64)),
                    ])
                })
                .collect(),
        ))
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
            "soil:time" => {
                function == "monotonic-ms" || function == "wall-clock" || function == "tick"
            }
            "soil:records" => matches!(
                function,
                "get"
                    | "scan"
                    | "scan-query"
                    | "scan-fields"
                    | "query"
                    | "aggregate"
                    | "upsert"
                    | "delete"
                    | "compare-and-swap"
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

fn project_entry(entry: records::Entry, fields: &[String]) -> records::Entry {
    if fields.is_empty() {
        return entry;
    }
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&entry.body) else {
        return entry;
    };
    let Some(object) = body.as_object() else {
        return entry;
    };
    let mut projected = serde_json::Map::new();
    for field in fields {
        if let Some(value) = object.get(field) {
            projected.insert(field.clone(), value.clone());
        }
    }
    records::Entry {
        body: serde_json::to_vec(&serde_json::Value::Object(projected)).unwrap_or_default(),
        ..entry
    }
}

fn records_filters(body: &[u8], filters: &[serde_json::Value]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let Ok(fields) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    filters.iter().all(|filter| {
        let field = filter
            .get("field")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let op = filter
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("eq");
        let wanted = filter
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let actual = json_path(&fields, field);
        match op {
            "exists" => actual.is_some(),
            "prefix" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.starts_with(wanted)),
            "contains" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains(wanted)),
            _ => actual.is_some_and(|actual| {
                actual.as_str() == Some(wanted) || actual.to_string() == wanted
            }),
        }
    })
}

fn aggregate_spec(spec: &Value) -> std::result::Result<serde_json::Value, String> {
    if let Value::String(text) = spec {
        return serde_json::from_str(text).map_err(|error| error.to_string());
    }
    let filters = field_value(spec, "filters");
    let filters = filters
        .as_list()
        .unwrap_or(&[])
        .iter()
        .map(|filter| {
            serde_json::json!({
                "field": owned_text(field_value(filter, "field")).unwrap_or_default(),
                "op": owned_text(field_value(filter, "op")).unwrap_or_default(),
                "value": owned_text(field_value(filter, "value")).unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let group_by = field_value(spec, "group_by");
    let group_by = group_by
        .as_list()
        .unwrap_or(&[])
        .iter()
        .map(|value| text(Some(value)).unwrap_or_default())
        .collect::<Vec<_>>();
    let bucket_field = owned_text(field_value(spec, "bucket_field")).unwrap_or_default();
    let bucket_seconds = field_value(spec, "bucket_seconds").as_f64().unwrap_or(0.0) as i64;
    let mut out = serde_json::json!({"prefix": owned_text(field_value(spec, "prefix")).unwrap_or_default(), "filters": filters, "group_by": group_by});
    if !bucket_field.is_empty() && bucket_seconds > 0 {
        out["bucket"] = serde_json::json!({"field": bucket_field, "seconds": bucket_seconds});
    }
    Ok(out)
}

fn json_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(value, |value, part| value.get(part))
}

fn json_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
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
