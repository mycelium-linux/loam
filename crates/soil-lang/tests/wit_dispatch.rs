// SPDX-License-Identifier: MIT OR Apache-2.0
use std::path::Path;

use soil_lang::dispatchgen::generate;

fn generated() -> soil_lang::dispatchgen::GeneratedDispatch {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    generate(
        &[root.join("soil-records.wit"), root.join("soil-time.wit")],
        "crate::wit",
    )
    .expect("generate records/time dispatch")
}

#[test]
fn emits_typed_arms_for_the_records_surface() {
    let generated = generated();
    for name in [
        "get",
        "scan",
        "query",
        "aggregate",
        "scan-packed",
        "upsert",
        "compare-and-swap",
        "delete",
        "changes",
        "apply",
    ] {
        assert!(
            generated
                .rust
                .contains(&format!("(\"soil:records\", \"{name}\") =>")),
            "missing records arm {name}"
        );
    }
    assert!(generated.functions >= 13);
    assert!(generated.types >= 20);
}

#[test]
fn emits_typed_parse_and_encode_for_canonical_fields() {
    let text = generated().rust;
    assert!(text.contains("fn parse_t10(value: &Value)"));
    assert!(text.contains("group_by: parse_t9(field(value, \"group-by\")"));
    assert!(text.contains("updated_at_ms: parse_u64(field(value, \"updated-at-ms\")"));
    assert!(text.contains("crate::wit::soil::records::records::query(&arg0)"));
    assert!(text.contains("crate::wit::soil::time::time::monotonic_ms()"));
    assert!(!text.contains("scan-query"));
    assert!(!text.contains("scan-fields"));
}

#[test]
fn emits_a_grant_table_matching_the_dispatch() {
    let text = generated().rust;
    assert!(text.contains("pub fn granted(interface: &str, function: &str) -> bool"));
    assert!(text.contains("(\"soil:records\", \"query\")"));
    assert!(text.contains("(\"soil:time\", \"tick\")"));
}
