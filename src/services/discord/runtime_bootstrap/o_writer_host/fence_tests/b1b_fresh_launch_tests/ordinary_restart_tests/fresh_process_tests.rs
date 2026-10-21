//! A managed writer waits for the binding that an independent relay restores after boot.

use super::*;
use crate::services::tui_o::store::OEra;

#[derive(Clone, Copy)]
pub(super) enum BindingBoot {
    Existing,
    Rehydrate,
    Missing,
}

pub(super) fn remove_binding(mode: BindingBoot) -> Option<dedupe::TuiRuntimeBinding> {
    if matches!(mode, BindingBoot::Existing) {
        return None;
    }
    let binding = dedupe::runtime_binding_for_tmux_session(CANARY_TMUX)
        .expect("capture the actual current incarnation before removing its memory binding");
    assert!(dedupe::clear_tmux_runtime_binding(CANARY_TMUX));
    assert!(dedupe::runtime_binding_for_tmux_session(CANARY_TMUX).is_none());
    assert!(!dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    Some(binding)
}

pub(super) fn rehydrate(
    mode: BindingBoot,
    binding: Option<dedupe::TuiRuntimeBinding>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !matches!(mode, BindingBoot::Rehydrate) {
        return None;
    }
    Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let restored = dedupe::reconcile_rehydrated_tmux_runtime_binding_with_authority(
            "codex",
            CANARY_TMUX,
            CANARY_CHANNEL,
            || {},
            |_, existing| {
                assert!(
                    existing.is_none(),
                    "the writer must not synthesize a runtime binding"
                );
                Some((binding.unwrap(), true))
            },
        );
        assert!(
            restored.is_some(),
            "real rehydration publication must succeed"
        );
        assert!(dedupe::codex_verified_channel_delivery_allowed(
            CANARY_CHANNEL
        ));
        eprintln!("B1B_FRESH_PROCESS binding_rehydrated=1 delay_ms=200");
    }))
}

pub(super) async fn wait_retry_post(
    store: &OStore,
    era: &OEra,
    output: &Discord,
    serial: u64,
    prior: &[String],
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    loop {
        let channel = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
        if channel
            .ledger()
            .piece(serial)
            .is_some_and(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
        {
            assert_eq!(channel.ledger().next_serial(), serial + 1);
            assert!(
                crate::services::tui_o::writer::local_retry::pending(channel.ledger()).is_none()
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the saved NotFound retry must POST once after its original 30s due time"
        );
        drop(channel);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut expected = prior.to_vec();
    expected.push(PAYLOAD.to_owned());
    assert_eq!(
        output.texts(),
        expected,
        "the restored retry POSTs exactly once"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_fresh_process_approval_rehydrates_and_posts_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_fresh_process_approval_rehydrates_and_posts_once_pg"
    )) {
        restart_with_binding(Pending::Approval, false, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_fresh_process_prepared_rehydrates_and_settles_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_fresh_process_prepared_rehydrates_and_settles_once_pg"
    )) {
        restart_with_binding(Pending::Prepared, false, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_fresh_process_not_found_rehydrates_and_retries_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_fresh_process_not_found_rehydrates_and_retries_once_pg"
    )) {
        restart_with_binding(Pending::LocalRetry, false, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scoped_fresh_process_approval_rehydrates_and_posts_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_fresh_process_approval_rehydrates_and_posts_once_pg"
    )) {
        restart_with_binding(Pending::Approval, true, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scoped_fresh_process_prepared_rehydrates_and_settles_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_fresh_process_prepared_rehydrates_and_settles_once_pg"
    )) {
        restart_with_binding(Pending::Prepared, true, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scoped_fresh_process_not_found_rehydrates_and_retries_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_fresh_process_not_found_rehydrates_and_retries_once_pg"
    )) {
        restart_with_binding(Pending::LocalRetry, true, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scoped_fresh_process_idle_history_rehydrates_without_durable_work_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_fresh_process_idle_history_rehydrates_without_durable_work_pg"
    )) {
        restart_with_binding(Pending::Idle, true, None, BindingBoot::Rehydrate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scoped_fresh_process_missing_binding_stays_unpublished_and_alarms_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_fresh_process_missing_binding_stays_unpublished_and_alarms_once_pg"
    )) {
        restart_with_binding(Pending::Idle, true, None, BindingBoot::Missing).await;
    }
}
