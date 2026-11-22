// SPDX-License-Identifier: MIT OR Apache-2.0
//! In-process `soil:records` host over `fungos-records`.
//!
//! This adapter is deliberately domain-free. It maps one book to the WIT
//! collection shape and scopes entity ids by collection. Devices, signals,
//! presence, and schemas live in `.soil`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::sexp::Value;
use anyhow::{Context, Result};
use fungos_records::{Record, Store};

use crate::Host;

pub struct SoilRecordsHost {
    store: Store,
    book: String,
}

impl SoilRecordsHost {
    pub fn open(root: impl AsRef<Path>, book: impl Into<String>) -> Result<Self> {
        let root = root.as_ref();
        let store = Store::open(root, &root.join("device_id"))
            .with_context(|| format!("open records at {}", root.display()))?;
        Ok(Self {
            store,
            book: book.into(),
        })
    }

    pub fn book(&self) -> &str {
        &self.book
    }

    fn scoped(collection: &str, id: &str) -> String {
        if collection.is_empty() {
            id.to_string()
        } else {
            format!("{collection}:{id}")
        }
    }

    fn unscoped(collection: &str, entity_id: &str) -> String {
        if collection.is_empty() {
            entity_id.to_string()
        } else {
            entity_id
                .strip_prefix(&format!("{collection}:"))
                .unwrap_or(entity_id)
                .to_string()
        }
    }

    fn entry_value(collection: &str, record: Record) -> Value {
        let body = serde_json::to_vec(&record.fields).unwrap_or_default();
        Value::List(vec![
            pair(
                "key",
                Value::List(vec![
                    pair("collection", Value::String(collection.to_string())),
                    pair(
                        "id",
                        Value::String(Self::unscoped(collection, &record.entity_id)),
                    ),
                ]),
            ),
            pair("schema", Value::String(record.schema_id)),
            pair("revision", Value::Int(record.updated_hlc as i64)),
            pair("updated-at-ms", Value::Int(record.updated_hlc as i64)),
            pair("origin", Value::String(record.origin)),
            pair("deleted", Value::Bool(record.deleted)),
            pair(
                "body",
                Value::List(body.iter().map(|byte| Value::Int(*byte as i64)).collect()),
            ),
        ])
    }

    fn scan_page(
        &self,
        collection: &str,
        cursor: Option<String>,
        limit: usize,
    ) -> std::result::Result<Value, String> {
        let cursor = cursor.map(|cursor| Self::scoped(collection, &cursor));
        let records = self
            .store
            .scan_records(&self.book, cursor.as_deref(), limit.saturating_add(1))
            .map_err(|error| error.to_string())?;
        let prefix = format!("{collection}:");
        let mut matching = records
            .into_iter()
            .filter(|record| collection.is_empty() || record.entity_id.starts_with(&prefix));
        let mut entries = Vec::with_capacity(limit);
        for _ in 0..limit {
            let Some(record) = matching.next() else { break };
            entries.push(Self::entry_value(collection, record));
        }
        // The cursor is exclusive on the host. The next page starts at the
        // first matching record we did not return.
        let next = matching
            .next()
            .map(|record| Self::unscoped(collection, &record.entity_id));
        Ok(page_value(entries, next))
    }

    fn scan_query_page(
        &self,
        collection: &str,
        field: &str,
        value: &str,
        cursor: Option<String>,
        limit: usize,
    ) -> std::result::Result<Value, String> {
        let mut cursor = cursor.map(|cursor| Self::scoped(collection, &cursor));
        let prefix = format!("{collection}:");
        let limit = limit.max(1);
        let mut entries = Vec::with_capacity(limit);
        let mut next = None;
        loop {
            let page = self
                .store
                .scan_records(&self.book, cursor.as_deref(), 512)
                .map_err(|error| error.to_string())?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for record in page {
                let entity = record.entity_id.clone();
                if collection.is_empty() || entity.starts_with(&prefix) {
                    if record
                        .fields
                        .get(field)
                        .and_then(|value| value.as_str())
                        == Some(value)
                    {
                        entries.push(Self::entry_value(collection, record));
                        if entries.len() == limit {
                            next = Some(Self::unscoped(collection, &entity));
                            break;
                        }
                    }
                }
                cursor = Some(entity);
            }
            if entries.len() == limit || page_len < 512 {
                break;
            }
        }
        Ok(page_value(entries, next))
    }

    /// Scan a type/prefix, returning only the selected top-level body fields.
    ///
    /// This is the read-side pushdown for reports: the host still sees the
    /// record, but the guest does not receive or decode a full issue body just
    /// to count a status. SQL stays inside the records backend; the Spore sees
    /// a typed projection, not a table.
    fn scan_fields_page(
        &self,
        collection: &str,
        prefix: &str,
        fields: &[String],
        cursor: Option<String>,
        limit: usize,
    ) -> std::result::Result<Value, String> {
        let prefix = if collection.is_empty() {
            prefix.to_string()
        } else {
            format!("{collection}:{prefix}")
        };
        // A type prefix is one contiguous entity-id range. Start the host scan at
        // the prefix instead of walking every record before it. The cursor is
        // exclusive, and every real id is lexically greater than the bare prefix.
        let mut cursor = cursor
            .map(|cursor| Self::scoped(collection, &cursor))
            .or_else(|| (!prefix.is_empty()).then(|| prefix.clone()));
        let limit = limit.max(1);
        let mut entries = Vec::with_capacity(limit);
        let mut next = None;
        loop {
            let page = self
                .store
                .scan_records(&self.book, cursor.as_deref(), 512)
                .map_err(|error| error.to_string())?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for record in page {
                let entity = record.entity_id.clone();
                if entity.starts_with(&prefix) {
                    entries.push(Self::entry_value_projected(collection, record, fields));
                    if entries.len() == limit {
                        next = Some(Self::unscoped(collection, &entity));
                        break;
                    }
                }
                cursor = Some(entity);
            }
            if entries.len() == limit || page_len < 512 {
                break;
            }
        }
        Ok(page_value(entries, next))
    }

    fn entry_value_projected(collection: &str, record: Record, fields: &[String]) -> Value {
        let mut record = record;
        if !fields.is_empty()
            && let Some(object) = record.fields.as_object()
        {
            let mut projected = serde_json::Map::new();
            for field in fields {
                if let Some(value) = object.get(field) {
                    projected.insert(field.clone(), value.clone());
                }
            }
            record.fields = serde_json::Value::Object(projected);
        }
        Self::entry_value(collection, record)
    }

    /// One filtered, projected page.
    ///
    /// `scan-query` and `scan-fields` were two partial spellings of this. The
    /// spec is a record, not a SQL string: collection, prefix, typed filters,
    /// selected body fields, cursor, limit. The host owns the translation to
    /// SQLite/JSON.
    fn query_page(&self, spec: &Value) -> std::result::Result<Value, String> {
        let collection = field_value(spec, "collection").as_str().unwrap_or("").to_string();
        let prefix = field_value(spec, "prefix").as_str().unwrap_or("").to_string();
        let filters = field_value(spec, "filters");
        let filters = filters.as_list().unwrap_or(&[]).to_vec();
        let fields = field_value(spec, "fields")
            .as_list()
            .unwrap_or(&[])
            .iter()
            .map(value_text)
            .collect::<Vec<_>>();
        let cursor = match field_value(spec, "cursor") {
            Value::Nil => None,
            value => Some(value_text(&value)),
        };
        let limit = field_value(spec, "limit").as_f64().unwrap_or(50.0) as usize;
        self.query_page_inner(&collection, &prefix, &filters, &fields, cursor, limit)
    }

    fn query_page_inner(
        &self,
        collection: &str,
        prefix: &str,
        filters: &[Value],
        fields: &[String],
        cursor: Option<String>,
        limit: usize,
    ) -> std::result::Result<Value, String> {
        let prefix = if collection.is_empty() {
            prefix.to_string()
        } else {
            format!("{collection}:{prefix}")
        };
        let mut cursor = cursor
            .map(|cursor| Self::scoped(collection, &cursor))
            .or_else(|| (!prefix.is_empty()).then(|| prefix.clone()));
        let limit = limit.max(1);
        let mut entries = Vec::with_capacity(limit);
        let mut next = None;
        loop {
            let page = self
                .store
                .scan_records(&self.book, cursor.as_deref(), 512)
                .map_err(|error| error.to_string())?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for record in page {
                let entity = record.entity_id.clone();
                cursor = Some(entity.clone());
                if !entity.starts_with(&prefix)
                    || !value_filters_match(&record.fields, filters)
                {
                    continue;
                }
                entries.push(Self::entry_value_projected(collection, record, fields));
                if entries.len() == limit {
                    next = Some(Self::unscoped(collection, &entity));
                    break;
                }
            }
            if entries.len() == limit || page_len < 512 {
                break;
            }
        }
        Ok(page_value(entries, next))
    }

    /// Count records grouped by body fields and/or a numeric time bucket.
    ///
    /// This is the host-side half of report pushdown: the guest sends a small
    /// JSON spec and receives one row per group. SQL stays in the records
    /// backend; Soil never sees a table or a query language.
    fn aggregate(
        &self,
        collection: &str,
        spec: &Value,
    ) -> std::result::Result<Value, String> {
        let spec = match spec {
            Value::String(text) => serde_json::from_str(text)
                .map_err(|error| format!("aggregate spec: {error}"))?,
            other => aggregate_spec_json(other)?,
        };
        let prefix = spec
            .get("prefix")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let group_by = spec
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
        let filters = spec
            .get("filters")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let bucket = spec.get("bucket").and_then(serde_json::Value::as_object);
        let bucket_field = bucket
            .and_then(|bucket| bucket.get("field"))
            .and_then(serde_json::Value::as_str);
        let bucket_seconds = bucket
            .and_then(|bucket| bucket.get("seconds"))
            .and_then(serde_json::Value::as_i64)
            .filter(|seconds| *seconds > 0);

        let prefix = if collection.is_empty() {
            prefix.to_string()
        } else {
            format!("{collection}:{prefix}")
        };
        let mut groups: BTreeMap<Vec<String>, u64> = BTreeMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .store
                .scan_records(&self.book, cursor.as_deref(), 512)
                .map_err(|error| error.to_string())?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for record in page {
                let entity = record.entity_id.clone();
                cursor = Some(entity.clone());
                if !entity.starts_with(&prefix) || !filters_match(&record.fields, &filters) {
                    continue;
                }
                let mut key = group_by
                    .iter()
                    .map(|field| json_text(json_path(&record.fields, field)))
                    .collect::<Vec<_>>();
                if let (Some(field), Some(seconds)) = (bucket_field, bucket_seconds)
                    && let Some(value) = json_path(&record.fields, field).and_then(|value| value.as_i64())
                {
                    key.push((value.div_euclid(seconds) * seconds).to_string());
                }
                *groups.entry(key).or_default() += 1;
            }
            if page_len < 512 {
                break;
            }
        }
        let rows = groups
            .into_iter()
            .map(|(values, count)| {
                Value::List(vec![
                    pair(
                        "values",
                        Value::List(values.into_iter().map(Value::String).collect()),
                    ),
                    pair("count", Value::Int(count as i64)),
                ])
            })
            .collect();
        Ok(ok(Value::List(rows)))
    }

    fn get_entry(&self, collection: &str, id: &str) -> std::result::Result<Option<Value>, String> {
        self.store
            .get_optional(&self.book, &Self::scoped(collection, id))
            .map(|record| record.map(|record| Self::entry_value(collection, record)))
            .map_err(|error| error.to_string())
    }

    /// Conditional write over the same revision/origin pair the WIT exposes.
    ///
    /// A records book is single-writer, so this is a lock-free check on the
    /// materialized revision: compare the caller's expected version, then put
    /// or tombstone. A conflict returns the current entry, not an error, so the
    /// guest can decide whether to retry or report who holds the claim.
    fn compare_and_swap_page(
        &mut self,
        key: &Value,
        expected: Option<&Value>,
        entry: &Value,
    ) -> std::result::Result<Value, String> {
        let (collection, id) = parse_key(key)?;
        let scoped = Self::scoped(&collection, &id);
        let current = self
            .store
            .get_optional(&self.book, &scoped)
            .map_err(|error| error.to_string())?;
        let current_entry = current
            .as_ref()
            .map(|record| Self::entry_value(&collection, record.clone()));
        let matches = match (expected, &current) {
            (None, None) => true,
            (None, Some(record)) => record.deleted,
            (Some(_), None) => return Ok(err("not-found")),
            (Some(version), Some(record)) => {
                field_value(version, "revision").as_f64().map(|n| n as u64)
                    == Some(record.updated_hlc)
                    && field_value(version, "origin").as_str() == Some(record.origin.as_str())
            }
        };
        if !matches {
            return Ok(ok(Value::List(vec![
                pair("applied", Value::Bool(false)),
                pair("entry", current_entry.unwrap_or(Value::Nil)),
            ])));
        }

        let deleted = truthy(&field_value(entry, "deleted"));
        let schema = field_value(entry, "schema")
            .as_str()
            .map(str::to_string)
            .or_else(|| current.as_ref().map(|record| record.schema_id.clone()))
            .unwrap_or_else(|| "soil.record.v1".to_string());
        if deleted {
            self.store
                .delete_record(&self.book, &scoped, &schema, 1)
                .map_err(|error| error.to_string())?;
        } else {
            let fields = body_json(&field_value(entry, "body"))?;
            self.store
                .put_record(&self.book, &scoped, &schema, 1, fields)
                .map_err(|error| error.to_string())?;
        }
        let written = self
            .store
            .get_optional(&self.book, &scoped)
            .map_err(|error| error.to_string())?
            .map(|record| Self::entry_value(&collection, record))
            .unwrap_or(Value::Nil);
        Ok(ok(Value::List(vec![
            pair("applied", Value::Bool(true)),
            pair("entry", written),
        ])))
    }
}

impl Host for SoilRecordsHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        interface == "soil:records"
            && matches!(
                function,
                "scan"
                    | "scan-query"
                    | "scan-fields"
                    | "aggregate"
                    | "query"
                    | "get"
                    | "upsert"
                    | "delete"
                    | "changes"
                    | "scan-packed"
                    | "cas"
                    | "compare-and-swap"
            )
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        if interface != "soil:records" {
            return Err(format!("unknown interface `{interface}`"));
        }

        match function {
            "scan-query" => {
                let collection = args
                    .first()
                    .map(value_text)
                    .ok_or_else(|| "scan-query needs a collection".to_string())?;
                let field = args
                    .get(1)
                    .map(value_text)
                    .ok_or_else(|| "scan-query needs a field".to_string())?;
                let value = args
                    .get(2)
                    .map(value_text)
                    .ok_or_else(|| "scan-query needs a value".to_string())?;
                let cursor = args.get(3).and_then(|value| match value {
                    Value::Nil => None,
                    value => Some(value_text(value)),
                });
                let limit = args.get(4).and_then(Value::as_f64).unwrap_or(50.0) as usize;
                Ok(ok(self.scan_query_page(&collection, &field, &value, cursor, limit)?))
            }
            "query" => {
                let spec = args
                    .first()
                    .ok_or_else(|| "query needs a spec".to_string())?;
                Ok(ok(self.query_page(spec)?))
            }
            "aggregate" => {
                let collection = args.first().map(value_text).unwrap_or_default();
                let spec = args.get(1).cloned().unwrap_or(Value::Nil);
                self.aggregate(&collection, &spec)
            }
            "scan-fields" => {
                let collection = args.first().map(value_text).unwrap_or_default();
                let prefix = args.get(1).map(value_text).unwrap_or_default();
                let fields = args
                    .get(2)
                    .and_then(Value::as_list)
                    .map(|values| values.iter().map(value_text).collect::<Vec<_>>())
                    .unwrap_or_default();
                let cursor = args.get(3).and_then(|value| match value {
                    Value::Nil => None,
                    value => Some(value_text(value)),
                });
                let limit = args.get(4).and_then(Value::as_f64).unwrap_or(50.0) as usize;
                Ok(ok(self.scan_fields_page(&collection, &prefix, &fields, cursor, limit)?))
            }
            "scan" | "changes" => {
                let collection = args
                    .first()
                    .map(value_text)
                    .ok_or_else(|| "scan needs a collection".to_string())?;
                let cursor = args.get(1).and_then(|value| match value {
                    Value::Nil => None,
                    value => Some(value_text(value)),
                });
                let limit = args.get(2).and_then(Value::as_f64).unwrap_or(50.0) as usize;
                Ok(ok(self.scan_page(&collection, cursor, limit)?))
            }
            "get" => {
                let key = args.first().ok_or_else(|| "get needs a key".to_string())?;
                let (collection, id) = parse_key(key)?;
                Ok(match self.get_entry(&collection, &id)? {
                    Some(entry) => ok(entry),
                    None => err("not-found"),
                })
            }
            "upsert" => {
                let entry = args
                    .first()
                    .cloned()
                    .ok_or_else(|| "upsert needs an entry".to_string())?;
                let key = field_value(&entry, "key");
                let (collection, id) = parse_key(&key)?;
                let schema = field_value(&entry, "schema")
                    .as_str()
                    .unwrap_or("soil.record.v1")
                    .to_string();
                let fields = body_json(&field_value(&entry, "body"))?;
                self.store
                    .put_record(
                        &self.book,
                        &Self::scoped(&collection, &id),
                        &schema,
                        1,
                        fields,
                    )
                    .map_err(|error| error.to_string())?;
                Ok(match self.get_entry(&collection, &id)? {
                    Some(entry) => ok(entry),
                    None => err("io"),
                })
            }
            "delete" => {
                let key = args
                    .first()
                    .ok_or_else(|| "delete needs a key".to_string())?;
                let (collection, id) = parse_key(key)?;
                let Some(existing) = self.get_entry(&collection, &id)? else {
                    return Ok(err("not-found"));
                };
                let schema = field_value(&existing, "schema")
                    .as_str()
                    .unwrap_or("soil.record.v1")
                    .to_string();
                self.store
                    .delete_record(&self.book, &Self::scoped(&collection, &id), &schema, 1)
                    .map_err(|error| error.to_string())?;
                Ok(ok(Value::Nil))
            }
            "compare-and-swap" | "cas" => {
                let key = args
                    .first()
                    .ok_or_else(|| "compare-and-swap needs a key".to_string())?;
                let expected = args.get(1).filter(|value| !matches!(value, Value::Nil));
                let entry = args
                    .get(2)
                    .ok_or_else(|| "compare-and-swap needs an entry".to_string())?;
                self.compare_and_swap_page(key, expected, entry)
            }
            "scan-packed" => Ok(err("unsupported")),
            other => Err(format!("unknown soil:records function `{other}`")),
        }
    }
}

fn aggregate_spec_json(spec: &Value) -> std::result::Result<serde_json::Value, String> {
    let mut object = serde_json::Map::new();
    object.insert(
        "prefix".into(),
        serde_json::Value::String(field_value(spec, "prefix").as_str().unwrap_or("").to_string()),
    );
    let filters = field_value(spec, "filters");
    let filters = filters
        .as_list()
        .unwrap_or(&[])
        .iter()
        .map(|filter| {
            let mut row = serde_json::Map::new();
            row.insert(
                "field".into(),
                serde_json::Value::String(field_value(filter, "field").as_str().unwrap_or("").to_string()),
            );
            row.insert(
                "op".into(),
                serde_json::Value::String(field_value(filter, "op").as_str().unwrap_or("eq").to_string()),
            );
            row.insert(
                "value".into(),
                serde_json::Value::String(field_value(filter, "value").as_str().unwrap_or("").to_string()),
            );
            serde_json::Value::Object(row)
        })
        .collect();
    object.insert("filters".into(), serde_json::Value::Array(filters));
    let group_by = field_value(spec, "group_by");
    object.insert(
        "group_by".into(),
        serde_json::Value::Array(
            group_by
                .as_list()
                .unwrap_or(&[])
                .iter()
                .map(|value| serde_json::Value::String(value_text(value)))
                .collect(),
        ),
    );
    let bucket_field = field_value(spec, "bucket_field");
    let bucket_seconds = field_value(spec, "bucket_seconds").as_f64().unwrap_or(0.0) as i64;
    if let Some(field) = bucket_field.as_str().filter(|field| !field.is_empty())
        && bucket_seconds > 0
    {
        object.insert(
            "bucket".into(),
            serde_json::json!({"field": field, "seconds": bucket_seconds}),
        );
    }
    Ok(serde_json::Value::Object(object))
}

fn value_filters_match(fields: &serde_json::Value, filters: &[Value]) -> bool {
    filters.iter().all(|filter| {
        let field_text = field_value(filter, "field");
        let field = field_text.as_str().unwrap_or("");
        let op_value = field_value(filter, "op");
        let op = op_value.as_str().unwrap_or("eq");
        let wanted = field_value(filter, "value");
        let actual = json_path(fields, field);
        match op {
            "exists" => actual.is_some(),
            "prefix" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.starts_with(&value_text(&wanted))),
            "contains" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains(&value_text(&wanted))),
            _ => match (actual, wanted.as_str()) {
                (Some(actual), Some(wanted)) => {
                    actual == &serde_json::Value::String(wanted.to_string())
                        || actual.to_string() == wanted
                        || actual.as_str() == Some(wanted)
                }
                (None, None) => true,
                _ => false,
            },
        }
    })
}

fn json_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(value, |current, part| current.get(part))
}

fn json_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn filters_match(fields: &serde_json::Value, filters: &[serde_json::Value]) -> bool {
    filters.iter().all(|filter| {
        let field = filter.get("field").and_then(serde_json::Value::as_str).unwrap_or("");
        let op = filter.get("op").and_then(serde_json::Value::as_str).unwrap_or("eq");
        let wanted = filter.get("value");
        let actual = json_path(fields, field);
        match op {
            "exists" => actual.is_some(),
            "prefix" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.starts_with(&json_text(wanted))),
            "contains" => actual
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains(&json_text(wanted))),
            _ => match (actual, wanted) {
                (Some(actual), Some(wanted)) => actual == wanted || json_text(Some(actual)) == json_text(Some(wanted)),
                (None, None) => true,
                _ => false,
            },
        }
    })
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Nil | Value::Bool(false))
}

fn parse_key(value: &Value) -> std::result::Result<(String, String), String> {
    let collection = field_value(value, "collection")
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "key needs a collection".to_string())?;
    let id = field_value(value, "id")
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "key needs an id".to_string())?;
    Ok((collection, id))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => text.clone(),
        other => other.to_string(),
    }
}

fn body_json(value: &Value) -> std::result::Result<serde_json::Value, String> {
    let bytes = match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                _ => Err("body must be a list of bytes".to_string()),
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
        Value::String(text) => text.as_bytes().to_vec(),
        _ => return Err("body must be a list of bytes or a string".to_string()),
    };
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid JSON body: {error}"))
}

fn field_value(record: &Value, name: &str) -> Value {
    let Some(items) = record.as_list() else {
        return Value::Nil;
    };
    for item in items {
        let Some(pair) = item.as_list() else { continue };
        let field = pair
            .first()
            .and_then(|value| value.as_keyword().or_else(|| value.as_symbol()));
        if field == Some(name) {
            return pair.get(1).cloned().unwrap_or(Value::Nil);
        }
    }
    Value::Nil
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.to_string()), value])
}

fn page_value(entries: Vec<Value>, next: Option<String>) -> Value {
    Value::List(vec![
        pair("entries", Value::List(entries)),
        pair("next", next.map(Value::String).unwrap_or(Value::Nil)),
    ])
}

fn ok(value: Value) -> Value {
    Value::List(vec![Value::Symbol("ok".to_string()), value])
}

fn err(code: &str) -> Value {
    Value::List(vec![
        Value::Symbol("err".to_string()),
        Value::Symbol(code.to_string()),
    ])
}
