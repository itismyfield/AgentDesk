//! A TUI channel row bound to a headless SDK transcript no longer blocks the pane's next turn.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

const SDK_RECORD: &str =
    r#"{"type":"system","subtype":"init","session_id":"s","entrypoint":"sdk-cli"}"#;
const CLI_RECORD: &str =
    r#"{"type":"system","subtype":"init","session_id":"s","entrypoint":"cli"}"#;
const HEADLESS_OUTPUT: &str = "headless lane output";
const SDK_PROMPT: &str = r#"{"type":"user","entrypoint":"sdk-cli","sessionId":"s","message":{"role":"user","content":"lane prompt"}}"#;
const SDK_END_TURN: &str = r#"{"type":"assistant","entrypoint":"sdk-cli","sessionId":"s","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"headless lane output"}]}}"#;
const SDK_STOP_HOOKS: &str = r#"{"type":"system","subtype":"stop_hook_summary","entrypoint":"sdk-cli","sessionId":"s","hookCount":1}"#;
const SDK_LAST_PROMPT: &str =
    r#"{"type":"last-prompt","lastPrompt":"lane prompt","sessionId":"s"}"#;
const SDK_COST_STATE: &str = r#"{"type":"cost-state","sessionId":"s","totalCostUSD":0.1}"#;

/// The operational shape: a synthetic session-bound row with relayed output, preserved across
/// a drain restart and never committed.
fn misbound_row(
    channel_id: u64,
    user_msg_id: u64,
    tmux: &str,
    output_path: &std::path::Path,
    owner: crate::services::discord::inflight::RelayOwnerKind,
) -> crate::services::discord::inflight::InflightTurnState {
    let provider = crate::services::provider::ProviderKind::Claude;
    let mut state = stale_foreign_state(provider, channel_id, user_msg_id, tmux, output_path);
    state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    state.injected_prompt_message_id = Some(user_msg_id);
    state.set_relay_owner_kind(owner);
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
    body_sends: Vec<String>,
}

/// Fails closed and records every message-body POST or edit to `channel` at the long-send and
/// replace transports; deletes and other channels pass through.
fn record_body_sends(
    channel: poise::serenity_prelude::ChannelId,
) -> (
    Arc<std::sync::Mutex<Vec<String>>>,
    crate::services::discord::formatting::rollback_transport_test_hook::Guard,
    crate::services::discord::formatting::chunk_transport_test_hook::Guard,
) {
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (posts, edits) = (sent.clone(), sent.clone());
    let send_hook = crate::services::discord::formatting::rollback_transport_test_hook::install(
        Box::new(move |seen, content, _reference, _nonce, _enforce| {
            (seen == channel).then(|| {
                posts.lock().unwrap().push(content.to_string());
                Err("body POST attempted".to_string())
            })
        }),
        Box::new(|_, _| None),
    );
    let edit_hook = crate::services::discord::formatting::chunk_transport_test_hook::install(
        Box::new(move |seen, _message, content| {
            (seen == channel).then(|| {
                edits.lock().unwrap().push(content.to_string());
                Err("body edit attempted".to_string())
            })
        }),
    );
    (sent, send_hook, edit_hook)
}

/// Runs the pending-start worker for a new TUI-direct prompt behind a session-bound row whose
/// transcript holds `records` (relayed to EOF unless `lagging`), via production demotion.
fn new_tui_turn_behind_misbound_row(records: &[&str], lagging: bool, channel_id: u64) -> Outcome {
    let owner = crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay;
    new_tui_turn_behind_row(records, lagging, owner, channel_id)
}

fn new_tui_turn_behind_row(
    records: &[&str],
    lagging: bool,
    owner: crate::services::discord::inflight::RelayOwnerKind,
    channel_id: u64,
) -> Outcome {
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
        let transcript: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(&headless, transcript).expect("write transcript");
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
        let mut row = misbound_row(channel_id, stale_msg, &tmux, &headless, owner);
        row.turn_nonce = stale_token.turn_nonce().map(str::to_owned);
        if lagging {
            row.last_offset = records[0].len() as u64 + 1;
        }
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
        let (body_sends, _send_hook, _edit_hook) = record_body_sends(channel);
        let worker = run_worker(shared.clone(), rec, view, claim, abort_cleanup, reclaim);
        tokio::spawn(worker).await.unwrap();
        let body_sends = body_sends.lock().unwrap().clone();
        Outcome {
            claims: claims.load(Ordering::SeqCst),
            aborts: aborts.load(Ordering::SeqCst),
            stale_cancelled: stale_token.cancelled.load(Ordering::Relaxed),
            row: crate::services::discord::inflight::load_inflight_state(&provider, channel_id),
            body_sends,
        }
    });
    reset_present_for_tests();
    outcome
}

#[test]
fn a_restart_preserved_row_on_a_headless_transcript_yields_to_the_next_tui_turn() {
    let outcome = new_tui_turn_behind_misbound_row(&[SDK_RECORD], false, 6_332_010);

    assert_eq!(
        (outcome.claims, outcome.aborts),
        (1, 0),
        "the new turn claims"
    );
    assert!(
        outcome.stale_cancelled,
        "the misbound row's mailbox turn is released"
    );
    assert_eq!(
        outcome.body_sends,
        Vec::<String>::new(),
        "reclaiming posts or edits no message body"
    );
    let row = outcome.row.expect("the new turn's row");
    assert_eq!(
        row.user_msg_id, 6_332_210,
        "the headless row and its output are gone"
    );
}

#[test]
fn a_headless_row_whose_sdk_turn_ended_yields_to_the_next_tui_turn() {
    let ended: &[&str] = &[SDK_PROMPT, SDK_END_TURN];
    let after_hooks: &[&str] = &[
        SDK_PROMPT,
        SDK_END_TURN,
        SDK_STOP_HOOKS,
        SDK_LAST_PROMPT,
        SDK_COST_STATE,
    ];
    let cases = [
        (ended, false),
        (ended, true),
        (after_hooks, false),
        (after_hooks, true),
    ];
    let outcomes: Vec<_> = cases
        .into_iter()
        .enumerate()
        .map(|(case, (records, lagging))| {
            let outcome =
                new_tui_turn_behind_misbound_row(records, lagging, 6_332_020 + case as u64);
            (outcome.claims, outcome.aborts, outcome.body_sends)
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![(1, 0, Vec::<String>::new()); 4],
        "each finished SDK shape, relayed to EOF or lagging, yields without a body send"
    );
}

#[test]
fn a_session_bound_row_on_a_tui_transcript_still_blocks_the_next_turn() {
    let outcome = new_tui_turn_behind_misbound_row(&[CLI_RECORD], false, 6_332_011);

    assert_eq!(
        (outcome.claims, outcome.aborts),
        (0, 1),
        "the live turn is not overwritten"
    );
    let row = outcome.row.expect("the live row survives");
    assert_eq!(row.user_msg_id, 6_332_111);
    assert_eq!(row.full_response, HEADLESS_OUTPUT);
}

#[test]
fn a_busy_row_on_a_transcript_not_marked_sdk_still_needs_a_ready_pane() {
    let tui = [SDK_PROMPT, SDK_END_TURN].map(|record| record.replace("sdk-cli", "cli"));
    let unmarked =
        [SDK_PROMPT, SDK_END_TURN].map(|record| record.replace(r#""entrypoint":"sdk-cli","#, ""));
    let owner = crate::services::discord::inflight::RelayOwnerKind::Watcher;
    let outcomes: Vec<_> = [tui, unmarked]
        .iter()
        .enumerate()
        .map(|(case, records)| {
            let records = records.each_ref().map(String::as_str);
            let outcome = new_tui_turn_behind_row(&records, false, owner, 6_332_030 + case as u64);
            (outcome.claims, outcome.aborts)
        })
        .collect();

    assert_eq!(outcomes, vec![(0, 1); 2], "the busy turn keeps its row");
}
