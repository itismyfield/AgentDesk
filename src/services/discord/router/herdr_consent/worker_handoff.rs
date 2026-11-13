use crate::db::intake_outbox::{consent_handoff, consent_projection::project};
use crate::domain::intake_consent::HoldInput;
use crate::intake_consent_authority::BeforeEffects;
use crate::services::session_host::policy::{
    consent::{Consent, ConsentStore, IntentRef},
    durable,
};
use sqlx::PgPool;

pub(super) async fn hold(
    pool: &PgPool,
    proof: BeforeEffects,
    store: &ConsentStore,
    expected: &Consent,
    intent: &IntentRef,
) -> Result<(), String> {
    let current = store
        .load(&expected.key)?
        .ok_or("consent custody missing")?;
    if current != *expected || !current.unresolved_intents().contains(intent) {
        return Err(
            "before-effects consent custody does not identify this exact worker payload".into(),
        );
    }
    let input = HoldInput::checked(&project(proof.row()), intent, durable::key(&current.key)?)?;
    consent_handoff::hold_before_effects(pool, proof, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handoff_observes_exact_unresolved_consent_before_db_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (offset, mode) in ["missing", "different", "settled", "source"]
            .into_iter()
            .enumerate()
        {
            let channel = (891270 + offset).to_string();
            let (row, root, store, mut expected, mut intent) =
                consent_handoff::tests::setup(&pool, &channel).await;
            let before =
                crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row.id).await;
            let store = if mode == "missing" {
                ConsentStore::new(Some(root.path().join("empty")))
            } else {
                store
            };
            match mode {
                "different" => expected.expires += 1,
                "settled" => expected = store.acknowledge_discard(&expected, &intent).unwrap(),
                "source" => intent.source_message += 1,
                _ => {}
            }
            assert!(
                hold(
                    &pool,
                    BeforeEffects::for_test(&row),
                    &store,
                    &expected,
                    &intent
                )
                .await
                .is_err(),
                "{mode}"
            );
            assert_eq!(
                crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row.id).await,
                before,
                "{mode}"
            );
        }
        let first = consent_handoff::tests::setup(&pool, "891280").await;
        let second = consent_handoff::tests::setup(&pool, "891281").await;
        let input = HoldInput::checked(
            &project(&first.0),
            &first.4,
            durable::key(&first.3.key).unwrap(),
        )
        .unwrap();
        let before =
            crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, second.0.id).await;
        assert!(
            consent_handoff::hold_before_effects(&pool, BeforeEffects::for_test(&second.0), input)
                .await
                .is_err()
        );
        assert_eq!(
            crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, second.0.id).await,
            before
        );
        pool.close().await;
        db.drop().await;
    }
}
