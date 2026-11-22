//! Shared `query`/`aggregate` implementation.
//!
//! Both records hosts expose the same filtered, projected read and grouped
//! count. Keeping the policy here means a book-backed host and a namespaced
//! host cannot drift on filters, cursors, projection, or bucket semantics.

use serde_json::Value;

use super::records;

/// How many entries one backing `scan` may return while answering a query.
const SCAN_BATCH: u32 = 512;

/// Filtered, projected read within one collection.
pub fn query<H: records::Host>(
    host: &mut H,
    spec: records::QuerySpec,
) -> Result<records::Page, records::Error> {
    if spec.limit == 0 {
        return Err(records::Error::OverBudget);
    }
    let prefix = spec.prefix.unwrap_or_default();
    let limit = spec.limit as usize;
    let mut cursor = spec.cursor;
    let mut entries = Vec::with_capacity(limit);
    let mut next = None;
    loop {
        let page = host.scan(spec.collection.clone(), cursor.clone(), SCAN_BATCH)?;
        let page_len = page.entries.len();
        for entry in page.entries {
            let id = entry.key.id.clone();
            if !id.starts_with(&prefix) || !matches(&entry.body, &spec.filters) {
                continue;
            }
            entries.push(project(entry, &spec.fields));
            if entries.len() == limit {
                next = Some(id);
                break;
            }
        }
        if entries.len() == limit || page_len < SCAN_BATCH as usize {
            break;
        }
        match page.next {
            Some(value) => cursor = Some(value),
            None => break,
        }
    }
    Ok(records::Page { entries, next })
}

/// Group live records by body fields, optionally bucketing one numeric field.
pub fn aggregate<H: records::Host>(
    host: &mut H,
    collection: String,
    spec: records::AggregateSpec,
) -> Result<Vec<records::AggregateGroup>, records::Error> {
    let prefix = spec.prefix.unwrap_or_default();
    let mut groups: std::collections::BTreeMap<Vec<String>, u64> =
        std::collections::BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = host.scan(collection.clone(), cursor.clone(), SCAN_BATCH)?;
        let page_len = page.entries.len();
        for entry in page.entries {
            let id = entry.key.id.clone();
            if !id.starts_with(&prefix) || !matches(&entry.body, &spec.filters) {
                continue;
            }
            let Ok(fields) = serde_json::from_slice::<Value>(&entry.body) else {
                continue;
            };
            let mut key = spec
                .group_by
                .iter()
                .map(|field| json_text(json_path(&fields, field)))
                .collect::<Vec<_>>();
            if let Some(field) = spec.bucket_field.as_deref()
                && spec.bucket_seconds > 0
                && let Some(value) = json_path(&fields, field).and_then(Value::as_i64)
            {
                let seconds = i64::from(spec.bucket_seconds);
                key.push((value.div_euclid(seconds) * seconds).to_string());
            }
            *groups.entry(key).or_default() += 1;
        }
        if page_len < SCAN_BATCH as usize {
            break;
        }
        match page.next {
            Some(value) => cursor = Some(value),
            None => break,
        }
    }
    Ok(groups
        .into_iter()
        .map(|(values, count)| records::AggregateGroup { values, count })
        .collect())
}

fn matches(body: &[u8], filters: &[records::Filter]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let Ok(fields) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    filters.iter().all(|filter| {
        let actual = json_path(&fields, &filter.field);
        let wanted = filter.value.as_deref().unwrap_or("");
        match filter.op {
            records::FilterOp::Exists => actual.is_some(),
            records::FilterOp::Prefix => actual
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with(wanted)),
            records::FilterOp::Contains => actual
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains(wanted)),
            records::FilterOp::Eq => actual.is_some_and(|actual| equals(actual, wanted)),
            records::FilterOp::Lt => actual.is_some_and(|actual| compare(actual, wanted).is_lt()),
            records::FilterOp::Lte => actual.is_some_and(|actual| !compare(actual, wanted).is_gt()),
            records::FilterOp::Gt => actual.is_some_and(|actual| compare(actual, wanted).is_gt()),
            records::FilterOp::Gte => actual.is_some_and(|actual| !compare(actual, wanted).is_lt()),
        }
    })
}

fn equals(actual: &Value, wanted: &str) -> bool {
    actual.as_str() == Some(wanted) || actual == wanted
}

fn compare(actual: &Value, wanted: &str) -> std::cmp::Ordering {
    if let (Some(left), Ok(right)) = (actual.as_i64(), wanted.parse::<i64>()) {
        return left.cmp(&right);
    }
    if let (Some(left), Ok(right)) = (actual.as_f64(), wanted.parse::<f64>()) {
        return left.total_cmp(&right);
    }
    actual.as_str().unwrap_or_default().cmp(wanted)
}

fn project(entry: records::Entry, fields: &[String]) -> records::Entry {
    if fields.is_empty() {
        return entry;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&entry.body) else {
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
        body: serde_json::to_vec(&Value::Object(projected)).unwrap_or_default(),
        ..entry
    }
}

fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |value, part| value.get(part))
}

fn json_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}
