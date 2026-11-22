//! Namespaced `soil:records` host.
//!
//! One Spore can read several books without seeing host paths. Each namespace is
//! a named grant:
//!
//! ```text
//! crm       -> team/fpl/crm        read/write
//! contacts  -> personal/avery/contacts read
//! messages  -> personal/avery/mail read
//! ```
//!
//! The Spore names only `crm`, `contacts`, `messages`. The host maps those names
//! to books and enforces read/write per namespace. Roles/Mycelium determine
//! which namespace grants are linked.

use std::{collections::BTreeMap, sync::Mutex};

use anyhow::Context;
use fungos_records::{Record, Store};

use crate::records_host::soil::records::records;

/// Records one `scan` call may return.
///
/// Reading a 6,000-record book crossed this boundary 12 times at 512 and twice
/// at 4096, and wall-clock did not move: the cost is copying every body into the
/// component, not the call. So this is a memory knob more than a speed one, and
/// it is set high enough that a whole book is two or three crossings without
/// holding an unbounded page at once. The real lever is not fetching bodies a
/// query does not need — see the records-backend follow-ups.
const MAX_PAGE: u32 = 4096;

/// How many store batches one `scan` may walk while looking for its own
/// collection. Bounded so a large book cannot make one call unbounded.
const MAX_SCAN_BATCHES: usize = 32;

/// One named book grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespace {
    pub name: String,
    pub book: String,
    pub collection: String,
    pub read: bool,
    pub write: bool,
}

impl Namespace {
    pub fn read(
        name: impl Into<String>,
        book: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            book: book.into(),
            collection: collection.into(),
            read: true,
            write: false,
        }
    }

    pub fn write(
        name: impl Into<String>,
        book: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            book: book.into(),
            collection: collection.into(),
            read: true,
            write: true,
        }
    }
}

pub struct NamespacedRecordsHost {
    store: Mutex<Store>,
    namespaces: BTreeMap<String, Namespace>,
    /// Native REPL mode: a collection name is a scope inside one book.
    dynamic_book: Option<String>,
}

impl NamespacedRecordsHost {
    pub fn open(
        root: impl AsRef<std::path::Path>,
        namespaces: Vec<Namespace>,
    ) -> anyhow::Result<Self> {
        let root = root.as_ref();
        let store = Store::open(root, &root.join("device_id")).context("open fungos-records")?;
        let mut map = BTreeMap::new();
        for namespace in namespaces {
            if namespace.name.is_empty() || namespace.book.is_empty() {
                anyhow::bail!("a namespace needs a name and a book");
            }
            if map.insert(namespace.name.clone(), namespace).is_some() {
                anyhow::bail!("duplicate namespace");
            }
        }
        Ok(Self {
            store: Mutex::new(store),
            namespaces: map,
            dynamic_book: None,
        })
    }

    /// Open one book where every collection name the caller asks for is a
    /// namespace inside that book. This is for the in-process REPL, not for a
    /// wasm grant: a Spore still gets an explicit namespace list.
    pub fn open_dynamic(
        root: impl AsRef<std::path::Path>,
        book: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let root = root.as_ref();
        let store = Store::open(root, &root.join("device_id")).context("open fungos-records")?;
        Ok(Self {
            store: Mutex::new(store),
            namespaces: BTreeMap::new(),
            dynamic_book: Some(book.into()),
        })
    }

    pub fn namespaces(&self) -> impl Iterator<Item = &Namespace> {
        self.namespaces.values()
    }

    fn namespace(&mut self, name: &str, write: bool) -> Result<Namespace, records::Error> {
        if !self.namespaces.contains_key(name)
            && let Some(book) = &self.dynamic_book
        {
            self.namespaces.insert(
                name.to_string(),
                Namespace {
                    name: name.to_string(),
                    book: book.clone(),
                    collection: name.to_string(),
                    read: true,
                    write: true,
                },
            );
        }
        let namespace = self.namespaces.get(name).ok_or(records::Error::Denied)?;
        if !namespace.read || (write && !namespace.write) {
            return Err(records::Error::Denied);
        }
        Ok(namespace.clone())
    }

    /// Store ids are scoped by collection: a book's primary key is
    /// `(book, entity id)`, so without this an inbox decision `m1` would
    /// overwrite the raw message `m1`, and a brief keyed by wearer would
    /// overwrite that wearer's vitals view.
    fn scoped(namespace: &Namespace, id: &str) -> String {
        // A whole-book namespace keeps the store's own ids: the book is the
        // collection, so there is nothing to disambiguate. That is the case for
        // a book written by the records CLI, whose ids are `contact-<hash>`
        // rather than `<collection>:<id>`.
        if namespace.collection.is_empty() {
            return id.to_string();
        }
        format!("{}:{id}", namespace.collection)
    }

    /// The guest-visible id, without the collection scope.
    fn unscoped(namespace: &Namespace, entity_id: &str) -> String {
        if namespace.collection.is_empty() {
            return entity_id.to_string();
        }
        entity_id
            .strip_prefix(&format!("{}:", namespace.collection))
            .unwrap_or(entity_id)
            .to_string()
    }

    fn entry(namespace: &Namespace, record: Record) -> records::Entry {
        records::Entry {
            key: records::Key {
                collection: namespace.name.clone(),
                id: Self::unscoped(namespace, &record.entity_id),
            },
            schema: record.schema_id,
            revision: record.updated_hlc,
            updated_at_ms: record.updated_hlc,
            origin: record.origin,
            deleted: record.deleted,
            body: serde_json::to_vec(&record.fields).unwrap_or_default(),
        }
    }
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u32).to_le_bytes());
}

impl records::Host for NamespacedRecordsHost {
    fn get(&mut self, key: records::Key) -> Result<records::Entry, records::Error> {
        let namespace = self.namespace(&key.collection, false)?;
        if key.id.is_empty() {
            return Err(records::Error::Invalid);
        }
        let record = {
            let store = self.store.lock().expect("records store poisoned");
            store
                .get(&namespace.book, &Self::scoped(&namespace, &key.id))
                .map_err(|_| records::Error::NotFound)?
        };
        Ok(Self::entry(&namespace, record))
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<records::Page, records::Error> {
        let namespace = self.namespace(&collection, false)?;
        if limit == 0 || limit > MAX_PAGE {
            return Err(records::Error::OverBudget);
        }
        // Empty means the book is the collection: everything in it is visible.
        let prefix = if namespace.collection.is_empty() {
            String::new()
        } else {
            format!("{}:", namespace.collection)
        };
        let store = self.store.lock().expect("records store poisoned");
        let mut entries = Vec::new();
        let mut cursor = cursor;
        // The book may hold several collections; walk it until this one has a
        // full page or the book ends.
        for _ in 0..MAX_SCAN_BATCHES {
            let batch = store
                .scan_records(&namespace.book, cursor.as_deref(), MAX_PAGE as usize)
                .map_err(|_| records::Error::Io)?;
            let batch_len = batch.len();
            let mut last = None;
            for record in batch {
                let entity_id = record.entity_id.clone();
                last = Some(entity_id.clone());
                if !entity_id.starts_with(&prefix) {
                    continue;
                }
                entries.push(Self::entry(&namespace, record));
                if entries.len() > limit as usize {
                    entries.truncate(limit as usize);
                    return Ok(records::Page {
                        entries,
                        next: last,
                    });
                }
            }
            cursor = last;
            if batch_len < MAX_PAGE as usize || cursor.is_none() {
                return Ok(records::Page {
                    entries,
                    next: None,
                });
            }
        }
        Err(records::Error::OverBudget)
    }

    fn query(&mut self, spec: records::QuerySpec) -> Result<records::Page, records::Error> {
        crate::records_host::query(self, spec)
    }

    fn aggregate(
        &mut self,
        collection: String,
        spec: records::AggregateSpec,
    ) -> Result<Vec<records::AggregateGroup>, records::Error> {
        crate::records_host::aggregate(self, collection, spec)
    }

    fn scan_packed(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<records::PackedPage, records::Error> {
        // Reuse `scan` for namespacing, cursor and limit semantics; only the
        // boundary shape is different.
        let page = <Self as records::Host>::scan(self, collection, cursor, limit)?;
        let mut entries = Vec::new();
        for entry in page.entries {
            put_u32(&mut entries, entry.key.id.len());
            entries.extend_from_slice(entry.key.id.as_bytes());
            entries.extend_from_slice(&entry.revision.to_le_bytes());
            put_u32(&mut entries, entry.body.len());
            entries.extend_from_slice(&entry.body);
        }
        Ok(records::PackedPage {
            entries,
            next: page.next,
        })
    }

    fn upsert(&mut self, entry: records::Entry) -> Result<records::Entry, records::Error> {
        let namespace = self.namespace(&entry.key.collection, true)?;
        if entry.key.id.is_empty() || entry.schema.is_empty() {
            return Err(records::Error::Invalid);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&entry.body).map_err(|_| records::Error::Invalid)?;
        if !value.is_object() {
            return Err(records::Error::Invalid);
        }
        let mut store = self.store.lock().expect("records store poisoned");
        store
            .put_record(
                &namespace.book,
                &Self::scoped(&namespace, &entry.key.id),
                &entry.schema,
                1,
                value,
            )
            .map_err(|_| records::Error::Io)?;
        let record = store
            .get(&namespace.book, &Self::scoped(&namespace, &entry.key.id))
            .map_err(|_| records::Error::Io)?;
        drop(store);
        Ok(Self::entry(&namespace, record))
    }

    fn compare_and_swap(
        &mut self,
        key: records::Key,
        expected: Option<records::Version>,
        entry: records::Entry,
    ) -> Result<records::CasResult, records::Error> {
        let namespace = self.namespace(&key.collection, true)?;
        if key.id.is_empty() {
            return Err(records::Error::Invalid);
        }
        if entry.key.collection != key.collection || entry.key.id != key.id {
            return Err(records::Error::Invalid);
        }
        // Decode before taking the store lock so a malformed body fails without
        // holding it.
        let body = if entry.deleted {
            None
        } else {
            if entry.schema.is_empty() {
                return Err(records::Error::Invalid);
            }
            let value: serde_json::Value =
                serde_json::from_slice(&entry.body).map_err(|_| records::Error::Invalid)?;
            if !value.is_object() {
                return Err(records::Error::Invalid);
            }
            Some(value)
        };

        let mut store = self.store.lock().expect("records store poisoned");
        let scoped = Self::scoped(&namespace, &key.id);
        let current = store.get(&namespace.book, &scoped).ok();
        let current_entry = current
            .as_ref()
            .map(|record| Self::entry(&namespace, record.clone()));
        let matches = match (&expected, &current) {
            (None, None) => true,
            // A tombstone is absent for create-if-absent semantics.
            (None, Some(record)) => record.deleted,
            (Some(_), None) => return Err(records::Error::NotFound),
            (Some(version), Some(record)) => {
                record.updated_hlc == version.revision && record.origin == version.origin
            }
        };
        if !matches {
            return Ok(records::CasResult {
                applied: false,
                entry: current_entry.expect("conflict has a current entry"),
            });
        }

        // A delete keeps the schema the record already had; there is no single
        // host schema to fall back to when a namespace has none. An empty schema
        // on a live write was already rejected above, so the live branch below
        // never sees one.
        let schema = if entry.schema.is_empty() {
            current
                .as_ref()
                .map(|record| record.schema_id.clone())
                .ok_or(records::Error::Invalid)?
        } else {
            entry.schema.clone()
        };

        if entry.deleted {
            store
                .delete_record(&namespace.book, &scoped, &schema, 1)
                .map_err(|_| records::Error::Io)?;
        } else {
            store
                .put_record(
                    &namespace.book,
                    &scoped,
                    &schema,
                    1,
                    body.expect("a live write has a body"),
                )
                .map_err(|_| records::Error::Io)?;
        }

        let record = store
            .get(&namespace.book, &scoped)
            .map_err(|_| records::Error::Io)?;
        drop(store);
        Ok(records::CasResult {
            applied: true,
            entry: Self::entry(&namespace, record),
        })
    }

    fn delete(&mut self, key: records::Key) -> Result<(), records::Error> {
        let namespace = self.namespace(&key.collection, true)?;
        if key.id.is_empty() {
            return Err(records::Error::Invalid);
        }
        let mut store = self.store.lock().expect("records store poisoned");
        let scoped = Self::scoped(&namespace, &key.id);
        let schema = store
            .get(&namespace.book, &scoped)
            .map(|record| record.schema_id)
            .map_err(|_| records::Error::NotFound)?;
        store
            .delete_record(&namespace.book, &scoped, &schema, 1)
            .map_err(|_| records::Error::Io)?;
        Ok(())
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> Result<records::Page, records::Error> {
        // A global cursor across multiple books is ambiguous. A single-book
        // host keeps the ordinary changes semantics; multi-book sync is a
        // host-mediated operation, not a Spore operation.
        if self.namespaces.len() != 1 {
            return Err(records::Error::Unsupported);
        }
        if limit == 0 || limit > MAX_PAGE {
            return Err(records::Error::OverBudget);
        }
        let namespace = self.namespaces.values().next().expect("one namespace");
        let records = {
            let store = self.store.lock().expect("records store poisoned");
            store
                .scan_changes(&namespace.book, since.as_deref(), limit as usize + 1)
                .map_err(|_| records::Error::Invalid)?
        };
        let mut entries: Vec<_> = records.into_iter().collect();
        let next = if entries.len() > limit as usize {
            entries.truncate(limit as usize);
            entries
                .last()
                .map(|record| format!("{}:{}", record.updated_hlc, record.entity_id))
        } else {
            None
        };
        Ok(records::Page {
            entries: entries
                .into_iter()
                .map(|record| Self::entry(namespace, record))
                .collect(),
            next,
        })
    }

    fn apply(
        &mut self,
        entries: Vec<records::Entry>,
    ) -> Result<records::ApplyResult, records::Error> {
        let mut result = records::ApplyResult {
            applied: 0,
            skipped: 0,
        };
        for entry in entries {
            let namespace = self.namespace(&entry.key.collection, true)?;
            let mut store = self.store.lock().expect("records store poisoned");
            if entry.key.id.is_empty() || entry.schema.is_empty() {
                return Err(records::Error::Invalid);
            }
            let fields = if entry.deleted {
                serde_json::json!({})
            } else {
                let value: serde_json::Value =
                    serde_json::from_slice(&entry.body).map_err(|_| records::Error::Invalid)?;
                if !value.is_object() {
                    return Err(records::Error::Invalid);
                }
                value
            };
            let entity_id = Self::scoped(&namespace, &entry.key.id);
            let applied = store
                .apply_entry(
                    &namespace.book,
                    fungos_records::RecordEntry {
                        entity_id: &entity_id,
                        schema_id: &entry.schema,
                        schema_version: 1,
                        fields,
                        deleted: entry.deleted,
                        revision: entry.revision,
                        origin: &entry.origin,
                    },
                )
                .map_err(|_| records::Error::Io)?;
            if applied {
                result.applied += 1;
            } else {
                result.skipped += 1;
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records_host::soil::records::records::Host as _;

    fn host(root: &std::path::Path) -> NamespacedRecordsHost {
        NamespacedRecordsHost::open(
            root,
            vec![
                Namespace::write("crm", "team/test/crm", "crm"),
                Namespace::read("contacts", "personal/test/contacts", "contact"),
            ],
        )
        .unwrap()
    }

    fn entry(namespace: &str, id: &str) -> records::Entry {
        records::Entry {
            key: records::Key {
                collection: namespace.into(),
                id: id.into(),
            },
            schema: "fungos.crm/account@1".into(),
            revision: 0,
            updated_at_ms: 0,
            origin: String::new(),
            deleted: false,
            body: serde_json::to_vec(&serde_json::json!({"name": "Acme"})).unwrap(),
        }
    }

    #[test]
    fn routes_namespaces_and_enforces_write_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path());
        host.upsert(entry("crm", "a1")).unwrap();
        assert_eq!(
            host.get(records::Key {
                collection: "crm".into(),
                id: "a1".into()
            })
            .unwrap()
            .key
            .collection,
            "crm"
        );
        assert_eq!(host.scan("crm".into(), None, 10).unwrap().entries.len(), 1);
        assert_eq!(
            host.upsert(entry("contacts", "c1")).unwrap_err(),
            records::Error::Denied
        );
        assert_eq!(
            host.scan("missing".into(), None, 10).unwrap_err(),
            records::Error::Denied
        );
    }

    /// A book's primary key is `(book, entity id)`, so two namespaces sharing
    /// one book must not collide on a guest id — that is how an inbox decision
    /// `m1` would erase the raw message `m1`.
    #[test]
    fn two_namespaces_in_one_book_cannot_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = NamespacedRecordsHost::open(
            dir.path(),
            vec![
                Namespace::write("raw", "personal/test/telemetry", "raw"),
                Namespace::write("decisions", "personal/test/telemetry", "decisions"),
            ],
        )
        .unwrap();

        let mut raw = entry("raw", "m1");
        raw.body = serde_json::to_vec(&serde_json::json!({"body": "hello"})).unwrap();
        host.upsert(raw).unwrap();
        let mut decision = entry("decisions", "m1");
        decision.body = serde_json::to_vec(&serde_json::json!({"action": "publish"})).unwrap();
        host.upsert(decision).unwrap();

        let body = |entry: records::Entry| {
            serde_json::from_slice::<serde_json::Value>(&entry.body).unwrap()
        };
        assert_eq!(
            body(
                host.get(records::Key {
                    collection: "raw".into(),
                    id: "m1".into()
                })
                .unwrap()
            )["body"],
            "hello",
            "the raw message survives the decision"
        );
        assert_eq!(
            body(
                host.get(records::Key {
                    collection: "decisions".into(),
                    id: "m1".into()
                })
                .unwrap()
            )["action"],
            "publish"
        );

        // A scan sees its own collection, not the whole book.
        let raw_page = host.scan("raw".into(), None, 10).unwrap();
        assert_eq!(raw_page.entries.len(), 1);
        assert_eq!(raw_page.entries[0].key.collection, "raw");
        let decision_page = host.scan("decisions".into(), None, 10).unwrap();
        assert_eq!(decision_page.entries.len(), 1);
        assert_eq!(decision_page.entries[0].key.collection, "decisions");
    }

    #[test]
    fn multi_book_changes_are_host_mediated() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path());
        assert_eq!(
            host.changes(None, 10).unwrap_err(),
            records::Error::Unsupported
        );
    }
}
