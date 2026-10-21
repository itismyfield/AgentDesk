//! Actual provider progress survives writer restarts on both sides of fresh proof publication.

use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use dedupe::binding_context::{BindingContext, SpawnNonceMarker};

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_restart_after_fresh_context_before_proof_preserves_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_restart_after_fresh_context_before_proof_preserves_fifo_pg"
    )) {
        scenario(true, BeforeInput::RestartAfterContext).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_restart_immediately_after_fresh_proof_preserves_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_restart_immediately_after_fresh_proof_preserves_fifo_pg"
    )) {
        scenario(true, BeforeInput::RestartAfterProof).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_restart_after_first_post_settles_without_duplicate_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_restart_after_first_post_settles_without_duplicate_pg"
    )) {
        scenario(true, BeforeInput::RestartAfterFirstPost).await;
    }
}

pub(super) async fn restart(
    fixture: &CanaryFixture,
    output: &Discord,
    readiness: &Arc<host::Readiness>,
    handles: &mut Vec<host::ManagedWriterHandle>,
    context: &BindingContext,
    after_post: bool,
) {
    let store_root = fixture.root.path().join("o_store");
    let era_file = store_root.join("o_era");
    let init_file = store_root.join(CANARY_CHANNEL.to_string()).join("init");
    let metadata = [fs::read(&era_file).unwrap(), fs::read(&init_file).unwrap()];
    let nonce_path = crate::services::tmux_common::session_temp_path(CANARY_TMUX, "spawn_nonce");
    let nonce_bytes = fs::read(&nonce_path).unwrap();
    let archive = PathBuf::from(&context.owner_runtime_root)
        .join("runtime/binding_evidence/codex")
        .join(format!("{}.json", context.execution_nonce));
    let archived_context = fs::read(&archive).unwrap();
    assert_eq!(
        serde_json::from_slice::<BindingContext>(&archived_context).unwrap(),
        *context,
        "the archive retains launch context before any source proof exists"
    );
    let ownership = dedupe::binding_events::codex::read_ownership(context).unwrap();
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(context.execution_nonce.clone())
    );
    if after_post {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
            .unwrap()
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        eventually(
            || {
                let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
                channel.ledger().unresolved().is_none()
                    && channel.ledger().piece(0).is_some_and(|piece| {
                        matches!(
                            piece.outcome,
                            Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                                _
                            ))
                        )
                    })
                    && channel.ledger().piece(1).is_some_and(|piece| {
                        piece.payload == "-# ✅ 완료"
                            && matches!(
                                piece.outcome,
                                Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                                    _
                                ))
                            )
                    })
            },
            "the actual first body and summary ledger to settle before restart",
        )
        .await;
        assert_eq!(
            store
                .open_channel(&era, CANARY_CHANNEL)
                .unwrap()
                .unwrap()
                .ledger()
                .next_serial(),
            2
        );
        assert_eq!(
            output
                .texts()
                .iter()
                .filter(|text| text.contains(FIRST_REPLY))
                .count(),
            1
        );
        assert!(
            output
                .texts()
                .iter()
                .all(|text| !text.contains(SECOND_REPLY))
        );
    }
    let previous_generation = handles[0].generation();
    let previous_actor = readiness.submission(CANARY_CHANNEL).unwrap();
    let stopped = handles.pop().unwrap().stop_and_join();
    tokio::time::timeout(Duration::from_secs(20), stopped)
        .await
        .expect("the actual O actor must stop at the selected provider phase")
        .unwrap();
    assert!(!readiness.is_hosted(CANARY_CHANNEL));
    let prepared = parts(
        &fixture.shared,
        &ProviderKind::Codex,
        None,
        Some(Arc::clone(&output.http)),
    );
    *handles = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || prepared,
    );
    assert_eq!(handles.len(), 1);
    assert_ne!(handles[0].generation(), previous_generation);
    eventually(
        || readiness.submission(CANARY_CHANNEL).is_some(),
        "the restarted actual O actor",
    )
    .await;
    assert!(!previous_actor.same_run(&readiness.submission(CANARY_CHANNEL).unwrap()));
    assert_eq!(fs::read(era_file).unwrap(), metadata[0]);
    assert_eq!(fs::read(init_file).unwrap(), metadata[1]);
    assert_eq!(fs::read(nonce_path).unwrap(), nonce_bytes);
    assert_eq!(fs::read(archive).unwrap(), archived_context);
    assert_eq!(
        dedupe::binding_events::codex::read_ownership(context).unwrap(),
        ownership,
        "writer restart must preserve the independently recorded hook ownership proof"
    );
    assert_eq!(
        dedupe::binding_context::execution_context("codex", &context.execution_nonce).unwrap(),
        *context
    );
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(context.execution_nonce.clone())
    );
}

pub(super) fn assert_proof(context: &BindingContext, expected: bool) {
    let ownership = dedupe::binding_events::codex::read_ownership(context).unwrap();
    assert!(!ownership.conflicted && ownership.pending.is_empty());
    assert_eq!(ownership.verified.is_some(), expected);
    if let Some(proof) = ownership.verified {
        assert_eq!(proof.source.session_id, NEW_NATIVE_ID);
        assert_eq!(
            proof.source.path,
            context
                .provider_root
                .as_ref()
                .unwrap()
                .join(format!("rollout-{NEW_NATIVE_ID}.jsonl")),
        );
        assert!(dedupe::binding_events::codex::source_file_matches(
            &proof.source
        ));
    }
}
