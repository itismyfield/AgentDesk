use super::*;
use serde_json::json;

#[test]
fn a_start_origin_lands_on_the_metadata_path_the_execution_reads() {
    let placed = start_metadata_with_origin(Some(json!({"routine_id": "r"})), Some(" G-1 "))
        .unwrap()
        .unwrap();
    assert_eq!(placed["routine_id"], "r", "existing metadata is kept");
    assert_eq!(metadata_origin_id(Some(&placed)).as_deref(), Some("G-1"));
    let fresh = start_metadata_with_origin(None, Some("G-2")).unwrap();
    assert_eq!(metadata_origin_id(fresh.as_ref()).as_deref(), Some("G-2"));
}

#[test]
fn a_blank_origin_leaves_the_metadata_untouched() {
    let metadata = Some(json!({"silent": true}));
    for blank in [None, Some(""), Some("   ")] {
        assert_eq!(
            start_metadata_with_origin(metadata.clone(), blank).unwrap(),
            metadata
        );
    }
    assert_eq!(start_metadata_with_origin(None, None).unwrap(), None);
}

#[test]
fn an_origin_the_contract_cannot_carry_is_refused() {
    let long = "x".repeat(MAX_ORIGIN_ID_LEN + 1);
    let edge = "x".repeat(MAX_ORIGIN_ID_LEN);
    assert_eq!(
        start_metadata_with_origin(None, Some(&long)),
        Err("origin_id_too_long")
    );
    assert!(start_metadata_with_origin(None, Some(&edge)).is_ok());
    let other = json!({"human_input": {"origin_id": "G-other"}});
    assert_eq!(
        start_metadata_with_origin(Some(other.clone()), Some("G-1")),
        Err("origin_id_conflict")
    );
    assert!(start_metadata_with_origin(Some(other), Some("G-other")).is_ok());
    for shape in [json!("text"), json!({"human_input": 3})] {
        assert_eq!(
            start_metadata_with_origin(Some(shape), Some("G-1")),
            Err("metadata_not_object")
        );
    }
}

#[test]
fn only_a_carried_origin_names_one() {
    for metadata in [
        None,
        Some(json!({})),
        Some(json!({"human_input": {"origin_id": "  "}})),
        Some(json!({"human_input": {"origin_id": 7}})),
        Some(json!({"origin_id": "top-level is not the contract"})),
    ] {
        assert_eq!(metadata_origin_id(metadata.as_ref()), None, "{metadata:?}");
    }
}

/// The contract's bound is 256 bytes on both the start body and the metadata it carries; the
/// literals pin it independently of the constant.
#[test]
fn an_origin_of_256_bytes_is_carried_and_257_is_refused() {
    let edge = "x".repeat(256);
    let over = "x".repeat(257);
    let placed = start_metadata_with_origin(None, Some(&edge)).unwrap();
    assert_eq!(metadata_origin_id(placed.as_ref()), Some(edge.clone()));
    assert_eq!(
        start_metadata_with_origin(None, Some(&over)),
        Err("origin_id_too_long")
    );
    let carried = json!({"human_input": {"origin_id": over}});
    assert_eq!(metadata_origin_id(Some(&carried)), None);
}
