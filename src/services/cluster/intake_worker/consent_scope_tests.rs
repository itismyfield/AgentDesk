use super::*;

#[tokio::test]
async fn before_effects_authority_is_exact_consumed_and_scoped_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, _root, _store, _consent, _intent) =
        crate::db::intake_outbox::consent_handoff::tests::setup(&pool, "891213").await;
    let request = intake_request_from_row(&row).unwrap();
    assert!(
        take_before_effects(&request).is_none(),
        "before_effects_authority_outside_scope_contract"
    );
    scope_before_effects(&row, async {
        let proof = take_before_effects(&request);
        assert!(
            proof.is_some(),
            "before_effects_authority_issued_before_effects_contract"
        );
        assert_eq!(proof.unwrap().row().id, row.id);
        assert!(
            take_before_effects(&request).is_none(),
            "before_effects_authority_consumed_before_mutation_contract"
        );
    })
    .await;
    let mut changed = request.clone();
    changed.user_text.push('!');
    scope_before_effects(&row, async {
        assert!(
            take_before_effects(&changed).is_none(),
            "before_effects_authority_exact_payload_contract"
        );
        assert!(
            take_before_effects(&request).is_none(),
            "before_effects_mismatch_consumes_authority_contract"
        );
    })
    .await;
    assert!(
        take_before_effects(&request).is_none(),
        "before_effects_authority_never_escapes_worker_scope_contract"
    );
    pool.close().await;
    db.drop().await;
}
