use serde_json::{Value, json};

use super::reader::LedgerDocument;

#[test]
fn document_rewrite_preserves_unknown_fields_and_rejects_unknown_obligation_fields() {
    let value: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/delivery_obligation/ledger.json"
    )))
    .unwrap();
    let mut document: LedgerDocument = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&document).unwrap(), value);
    let extensions = document.extensions().clone();
    document.obligation_ledger.as_mut().unwrap().publication.rev += 1;
    let encoded = serde_json::to_string(&document).unwrap();
    let reread: LedgerDocument = serde_json::from_str(&encoded).unwrap();
    assert_eq!(reread.extensions(), &extensions);
    assert_eq!(
        reread.extensions()["future_section"],
        value["future_section"]
    );
    assert_eq!(reread.extensions()["future_scalar"], value["future_scalar"]);
    let mut unknown = value;
    unknown["obligation_ledger"]["future_obligation_flag"] = json!(true);
    assert!(serde_json::from_value::<LedgerDocument>(unknown).is_err());
}
