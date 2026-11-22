// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Compile the generated dispatch against mock WIT bindings.
//!
//! This does not test a real component boundary; it catches drift between the
//! generator's Rust names and the shape `wit-bindgen` exposes.

#[allow(dead_code, clippy::module_inception)]
mod wit {
    pub mod soil {
        pub mod records {
            pub mod records {
                pub struct Key {
                    pub collection: String,
                    pub id: String,
                }
                pub struct Entry {
                    pub key: Key,
                    pub schema: String,
                    pub revision: u64,
                    pub updated_at_ms: u64,
                    pub origin: String,
                    pub deleted: bool,
                    pub body: Vec<u8>,
                }
                pub struct Page {
                    pub entries: Vec<Entry>,
                    pub next: Option<String>,
                }
                pub struct PackedPage {
                    pub entries: Vec<u8>,
                    pub next: Option<String>,
                }
                pub enum FilterOp {
                    Eq,
                    Prefix,
                    Contains,
                    Exists,
                    Lt,
                    Lte,
                    Gt,
                    Gte,
                }
                pub struct Filter {
                    pub field: String,
                    pub op: FilterOp,
                    pub value: Option<String>,
                }
                pub struct QuerySpec {
                    pub collection: String,
                    pub prefix: Option<String>,
                    pub filters: Vec<Filter>,
                    pub fields: Vec<String>,
                    pub cursor: Option<String>,
                    pub limit: u32,
                }
                pub struct AggregateSpec {
                    pub collection: String,
                    pub prefix: Option<String>,
                    pub filters: Vec<Filter>,
                    pub group_by: Vec<String>,
                    pub bucket_field: Option<String>,
                    pub bucket_seconds: u32,
                }
                pub struct AggregateGroup {
                    pub values: Vec<String>,
                    pub count: u64,
                }
                pub struct ApplyResult {
                    pub applied: u32,
                    pub skipped: u32,
                }
                pub struct Version {
                    pub revision: u64,
                    pub origin: String,
                }
                pub struct CasResult {
                    pub applied: bool,
                    pub entry: Entry,
                }
                pub enum Error {
                    Denied,
                    NotFound,
                    OverBudget,
                    Invalid,
                    Stale,
                    Io,
                    Unsupported,
                }

                pub fn get(_: Key) -> Result<Entry, Error> {
                    Err(Error::Unsupported)
                }
                pub fn scan(_: String, _: Option<String>, _: u32) -> Result<Page, Error> {
                    Err(Error::Unsupported)
                }
                pub fn query(_: QuerySpec) -> Result<Page, Error> {
                    Err(Error::Unsupported)
                }
                pub fn aggregate(
                    _: String,
                    _: AggregateSpec,
                ) -> Result<Vec<AggregateGroup>, Error> {
                    Err(Error::Unsupported)
                }
                pub fn scan_packed(
                    _: String,
                    _: Option<String>,
                    _: u32,
                ) -> Result<PackedPage, Error> {
                    Err(Error::Unsupported)
                }
                pub fn upsert(_: Entry) -> Result<Entry, Error> {
                    Err(Error::Unsupported)
                }
                pub fn compare_and_swap(
                    _: Key,
                    _: Option<Version>,
                    _: Entry,
                ) -> Result<CasResult, Error> {
                    Err(Error::Unsupported)
                }
                pub fn delete(_: Key) -> Result<(), Error> {
                    Err(Error::Unsupported)
                }
                pub fn changes(_: Option<String>, _: u32) -> Result<Page, Error> {
                    Err(Error::Unsupported)
                }
                pub fn apply(_: Vec<Entry>) -> Result<ApplyResult, Error> {
                    Err(Error::Unsupported)
                }
            }
        }

        pub mod time {
            pub mod time {
                pub enum Error {
                    Denied,
                    NotReady,
                }
                pub struct DateTime {
                    pub unix_ms: u64,
                    pub offset_minutes: i32,
                    pub year: u32,
                    pub month: u8,
                    pub day: u8,
                    pub hour: u8,
                    pub minute: u8,
                    pub second: u8,
                    pub weekday: u8,
                    pub zone: Option<String>,
                }
                pub fn monotonic_ms() -> Result<u64, Error> {
                    Err(Error::NotReady)
                }
                pub fn wall_clock() -> Result<DateTime, Error> {
                    Err(Error::NotReady)
                }
                pub fn tick(_: u32) -> Result<Option<u64>, Error> {
                    Err(Error::NotReady)
                }
            }
        }
    }
}

include!("../bindings/soil-dispatch.rs");

#[test]
fn parses_a_query_spec_and_encodes_a_typed_error() {
    let spec = Value::List(vec![
        Value::List(vec![
            Value::Keyword("collection".into()),
            Value::String("books".into()),
        ]),
        Value::List(vec![Value::Keyword("prefix".into()), Value::Nil]),
        Value::List(vec![
            Value::Keyword("filters".into()),
            Value::List(vec![Value::List(vec![
                Value::List(vec![
                    Value::Keyword("field".into()),
                    Value::String("status".into()),
                ]),
                Value::List(vec![
                    Value::Keyword("op".into()),
                    Value::Symbol("eq".into()),
                ]),
                Value::List(vec![
                    Value::Keyword("value".into()),
                    Value::String("open".into()),
                ]),
            ])]),
        ]),
        Value::List(vec![Value::Keyword("fields".into()), Value::Nil]),
        Value::List(vec![Value::Keyword("cursor".into()), Value::Nil]),
        Value::List(vec![Value::Keyword("limit".into()), Value::Int(10)]),
    ]);
    assert_eq!(
        call("soil:records", "query", &[spec]).unwrap(),
        Value::List(vec![
            Value::Symbol("err".into()),
            Value::Symbol("unsupported".into()),
        ])
    );
}

#[test]
fn dispatches_zero_argument_time_calls() {
    assert_eq!(
        call("soil:time", "monotonic-ms", &[]).unwrap(),
        Value::List(vec![
            Value::Symbol("err".into()),
            Value::Symbol("not-ready".into()),
        ])
    );
    assert!(granted("soil:records", "aggregate"));
    assert!(!granted("soil:records", "scan-query"));
}
