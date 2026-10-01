//! A TUI channel row bound to a headless SDK transcript no longer blocks the pane's next turn.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

const SDK_RECORD: &str =
    r#"{"type":"system","subtype":"init","session_id":"s","entrypoint":"sdk-cli"}"#;
const CLI_RECORD: &str =
    r#"{"type":"system","subtype":"init","session_id":"s","entrypoint":"cli"}"#;
const HEADLESS_OUTPUT: &str = "headless lane output";

/// The operational shape: a synthetic session-bound row with relayed output, preserved across
/// a drain restart and never committed.
fn misbound_row(
    channel_id: u64,
    user_msg_id: u64,
    tmux: &str,
    output_path: &std::path::Path,
) -> crate::services::discord::inflight::InflightTurnState {
    let provider = crate::services::provider::ProviderKind::Claude;
    let mut state = stale_foreign_state(provider, channel_id, user_msg_id, tmux, output_path);
    state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    state.injected_prompt_message_id = Some(user_msg_id);
    state.set_relay_owner_kind(
        crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay,
    );
    state.restart_mode =
        Some(crate::services::discord::restart_mode::InflightRestartMode::DrainRestart);
    state.full_response = HEADLESS_OUTPUT.to_string();
    state.current_msg_id = user_msg_id + 7;
    stamp_claude_ready_for_input_evidence(&mut state, output_path);
    state
}

struct Outcome {
    claims: u32,
    aborts: u32,
    stale_cancelled: bool,
    row: Option<crate::services::discord::inflight::InflightTurnState>,
}

/// Runs the pending-start worker for a new TUI-direct prompt behind a misbound row whose
/// transcript holds `first_record`, reclaiming through the production stale-foreign demotion.
fn new_tui_turn_behind_misbound_row(first_record: &str, channel_id: u64) -> Outcome {
    let _guard = worker_test_lock();
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    reset_present_for_tests();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("test runtime");
    let outcome = rt.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = crate::services::provider::ProviderKind::Claude;
        let channel = poise::serenity_prelude::ChannelId::new(channel_id);
        let (stale_msg, anchor) = (channel_id + 100, channel_id + 200);
        let tmux = format!("tmux-headless-row-{channel_id}");
        let headless = temp.path().join("misbound.jsonl");
        std::fs::write(&headless, format!("{first_record}\n")).expect("write transcript");
        let stale_token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                stale_token.clone(),
                poise::serenity_prelude::UserId::new(1),
                poise::serenity_prelude::MessageId::new(stale_msg),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let mut row = misbound_row(channel_id, stale_msg, &tmux, &headless);
        row.turn_nonce = stale_token.turn_nonce().map(str::to_owned);
        write_inflight_fixture(temp.path(), &provider, &row);

        let mut rec = record("claude", channel_id, anchor);
        rec.tmux_session_name = tmux.clone();
        persist(&rec).unwrap();
        let view: ViewFn = Box::new(|_shared, record| {
            Box::pin(async move {
                let provider = crate::services::provider::ProviderKind::Claude;
                let inflight = crate::services::discord::inflight::load_inflight_state(
                    &provider,
                    record.channel_id,
                );
                let own = inflight
                    .as_ref()
                    .is_some_and(|state| state.user_msg_id == record.anchor_message_id);
                let foreign_inflight_identity = inflight
                    .as_ref()
                    .filter(|_| !own)
                    .map(|state| (state.user_msg_id, state.started_at.clone()));
                Some(PriorTurnObservation {
                    view: PriorTurnView {
                        inflight_present: inflight.is_some(),
                        inflight_is_own_anchor: own,
                        mailbox_blocking_turn_present: false,
                        mailbox_turn_is_own_anchor: false,
                        runtime_binding_present: true,
                    },
                    foreign_inflight_identity,
                })
            })
        });
        let claims = Arc::new(AtomicU32::new(0));
        let claims_for_fn = claims.clone();
        let root = temp.path().to_path_buf();
        let claim: ClaimFn = Box::new(move |shared, record| {
            let (claims, root) = (claims_for_fn.clone(), root.clone());
            Box::pin(async move {
                let channel = poise::serenity_prelude::ChannelId::new(record.channel_id);
                let token = Arc::new(crate::services::provider::CancelToken::new());
                let started = crate::services::discord::mailbox_try_start_turn(
                    shared,
                    channel,
                    token,
                    poise::serenity_prelude::UserId::new(1),
                    poise::serenity_prelude::MessageId::new(record.anchor_message_id),
                )
                .await;
                if !started {
                    return false;
                }
                let provider = crate::services::provider::ProviderKind::Claude;
                let tui = root.join("tui.jsonl");
                std::fs::write(&tui, format!("{CLI_RECORD}\n")).expect("write tui transcript");
                let mut fresh = crate::services::discord::inflight::InflightTurnState::new(
                    provider.clone(),
                    record.channel_id,
                    None,
                    1,
                    record.anchor_message_id,
                    record.anchor_message_id + 1,
                    record.prompt_text.clone(),
                    None,
                    Some(record.tmux_session_name.clone()),
                    Some(tui.to_string_lossy().to_string()),
                    None,
                    0,
                );
                fresh.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
                fresh.injected_prompt_message_id = Some(record.anchor_message_id);
                write_inflight_fixture(&root, &provider, &fresh);
                claims.fetch_add(1, Ordering::SeqCst);
                true
            })
        });
        let reclaim: ReclaimOrphanFn = Box::new(|shared, record| {
            Box::pin(async move {
                if demote_stale_foreign_inflight_if_current(shared, record).await {
                    ReclaimStaleForeignOutcome::StaleForeignDemoted
                } else {
                    ReclaimStaleForeignOutcome::None
                }
            })
        });
        let (abort_cleanup, aborts, _) = recording_abort_cleanup();
        let worker = run_worker(shared.clone(), rec, view, claim, abort_cleanup, reclaim);
        tokio::spawn(worker).await.unwrap();
        Outcome {
            claims: claims.load(Ordering::SeqCst),
            aborts: aborts.load(Ordering::SeqCst),
            stale_cancelled: stale_token.cancelled.load(Ordering::Relaxed),
            row: crate::services::discord::inflight::load_inflight_state(&provider, channel_id),
        }
    });
    reset_present_for_tests();
    outcome
}

#[test]
fn a_restart_preserved_row_on_a_headless_transcript_yields_to_the_next_tui_turn() {
    let outcome = new_tui_turn_behind_misbound_row(SDK_RECORD, 6_332_010);

    assert_eq!(
        (outcome.claims, outcome.aborts),
        (1, 0),
        "the new turn claims"
    );
    assert!(
        outcome.stale_cancelled,
        "the misbound row's mailbox turn is released"
    );
    let row = outcome.row.expect("the new turn's row");
    assert_eq!(row.user_msg_id, 6_332_210, "the row is the new turn's");
    assert!(
        row.full_response.is_empty(),
        "the headless output is dropped with its row, not carried or redelivered"
    );
}

#[test]
fn a_session_bound_row_on_a_tui_transcript_still_blocks_the_next_turn() {
    let outcome = new_tui_turn_behind_misbound_row(CLI_RECORD, 6_332_011);

    assert_eq!(
        (outcome.claims, outcome.aborts),
        (0, 1),
        "the live turn is not overwritten"
    );
    let row = outcome.row.expect("the live row survives");
    assert_eq!(row.user_msg_id, 6_332_111);
    assert_eq!(row.full_response, HEADLESS_OUTPUT);
}
