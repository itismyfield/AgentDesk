#[test]
fn cold_codex_herdr_pending_lookup_leaves_source_and_listing_unlocked_pg() {
    super::run_case(super::Case::PendingLookup);
}
