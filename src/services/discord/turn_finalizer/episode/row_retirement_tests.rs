//! Drifted-row retirement at the claim: every proof axis, the CAS outcomes it reports, the
//! compensation and cleanup-less callers, and turns admitted around the retirement.
//! Drifted rows here are synthetic; no production writer of that drift is known.

use super::row_retirement::{RuntimeWitness, retire_drifted_row, test_hooks};
use super::*;
use crate::services::discord::inflight::{
    self, GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
};
use serenity::model::id::{MessageId, UserId};
use std::sync::atomic::Ordering;

const TMUX: &str = "adk-codex-retire";

#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `work` on this thread with INFO logs captured.
async fn logged<T>(work: impl std::future::Future<Output = T>) -> (T, String) {
    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let guard = tracing::subscriber::set_default(subscriber);
    let out = work.await;
    drop(guard);
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    (out, logs)
}

fn lines<'a>(logs: &'a str, marker: &str) -> Vec<&'a str> {
    logs.lines().filter(|line| line.contains(marker)).collect()
}

async fn busy(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    nonce: Option<&str>,
) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(
        nonce.map(str::to_owned),
    ));
    shared
        .mailbox(channel)
        .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(123))
        .await;
    shared.restart.global_active.fetch_add(1, Ordering::Relaxed);
    token
}

/// The episode's row with its finalizer id drifted from the mailbox key 123.
fn drifted_row(channel: ChannelId, nonce: Option<&str>) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel.get(),
        None,
        7,
        124,
        124,
        "prompt".to_string(),
        None,
        Some(TMUX.to_string()),
        None,
        None,
        0,
    );
    row.turn_nonce = nonce.map(str::to_owned);
    save_inflight_state(&row).expect("seed row");
    load_inflight_state(&ProviderKind::Codex, channel.get()).expect("seeded row")
}

fn row_path(channel: ChannelId) -> std::path::PathBuf {
    inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Codex,
        channel.get(),
    )
}

fn row_bytes(channel: ChannelId) -> Option<Vec<u8>> {
    std::fs::read(row_path(channel)).ok()
}

fn key(shared: &SharedData, channel: ChannelId, nonce: Option<&str>) -> TurnKey {
    TurnKey::new(channel, 123, shared.restart.current_generation).with_episode_nonce(nonce)
}

fn probed() -> Option<RuntimeWitness<'static>> {
    Some(RuntimeWitness {
        probed_tmux: Some(TMUX),
        claim_snapshot: None,
    })
}

async fn claim(
    shared: &Arc<SharedData>,
    key: TurnKey,
    witness: Option<RuntimeWitness<'_>>,
) -> Option<CapturedFinish> {
    claim_episode(shared, &ProviderKind::Codex, key, true, None, witness)
        .await
        .expect("claim")
}

/// The release retires the drifted row only when a non-empty nonce, provider/channel and runtime
/// all name it; any refused axis keeps the row byte-identical and names that axis once.
#[tokio::test]
async fn drift_retirement_requires_every_proof_axis() {
    super::super::tests::with_isolated_runtime_root(|| async {
        enum Witness {
            Probed(&'static str),
            Unwitnessed,
            Snapshot { started_at: Option<&'static str> },
        }
        let cases = [
            (Some("episode-a"), Witness::Probed(TMUX), None),
            (
                Some("episode-a"),
                Witness::Snapshot { started_at: None },
                None,
            ),
            (Some(""), Witness::Probed(TMUX), Some("nonce_absent")),
            (None, Witness::Probed(TMUX), Some("nonce_absent")),
            (
                Some("episode-b"),
                Witness::Probed(TMUX),
                Some("nonce_mismatch"),
            ),
            (
                Some("episode-a"),
                Witness::Probed("adk-codex-other"),
                Some("runtime_identity_mismatch"),
            ),
            (
                Some("episode-a"),
                Witness::Unwitnessed,
                Some("runtime_identity_mismatch"),
            ),
            (
                Some("episode-a"),
                Witness::Snapshot {
                    started_at: Some("2026-01-01T00:00:00Z"),
                },
                Some("runtime_identity_mismatch"),
            ),
        ];
        for (index, (row_nonce, witness, refusal)) in cases.into_iter().enumerate() {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(648_460 + index as u64);
            // A legacy episode carries no nonce on its mailbox, key or row.
            let episode = row_nonce.map(|_| "episode-a");
            busy(&shared, channel, episode).await;
            let row = drifted_row(channel, row_nonce);
            let before = row_bytes(channel);
            let mut snapshot = SyntheticClaimSnapshot::from_row(&row);
            let witness = match witness {
                Witness::Probed(name) => RuntimeWitness {
                    probed_tmux: Some(name),
                    claim_snapshot: None,
                },
                Witness::Unwitnessed => RuntimeWitness::default(),
                Witness::Snapshot { started_at } => {
                    if let Some(started_at) = started_at {
                        snapshot.started_at = started_at.to_string();
                    }
                    RuntimeWitness {
                        probed_tmux: None,
                        claim_snapshot: Some(&snapshot),
                    }
                }
            };
            let key = key(&shared, channel, episode);
            let (captured, logs) = logged(claim(&shared, key, Some(witness))).await;
            assert!(
                captured.unwrap().finish.removed_token.is_some(),
                "case {index}"
            );
            let declined = lines(&logs, "episode_row_retirement_declined");
            match refusal {
                None => {
                    assert!(declined.is_empty(), "case {index}: {declined:?}");
                    assert_eq!(lines(&logs, "episode_row_retired").len(), 1, "case {index}");
                    assert_eq!(row_bytes(channel), None, "case {index}: row retired");
                }
                Some(reason) => {
                    assert_eq!(declined.len(), 1, "case {index}: {logs}");
                    let expected = format!("gate=\"proof\" reason=\"{reason}\"");
                    assert!(
                        declined[0].contains(&expected),
                        "case {index}: {}",
                        declined[0]
                    );
                    assert!(
                        declined[0].contains("mailbox_released=true"),
                        "case {index}"
                    );
                    assert_eq!(row_bytes(channel), before, "case {index}: row bytes");
                }
            }
        }
    })
    .await;
}

/// A row naming another provider or channel than the claim is not this episode's.
#[tokio::test]
async fn drift_retirement_refuses_a_row_of_another_provider_or_channel() {
    super::super::tests::with_isolated_runtime_root(|| async {
        for (index, foreign_channel) in [false, true].into_iter().enumerate() {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(648_470 + index as u64);
            busy(&shared, channel, Some("episode-a")).await;
            let mut row = drifted_row(channel, Some("episode-a"));
            if foreign_channel {
                row.channel_id += 1;
            } else {
                row.provider = ProviderKind::Claude.as_str().to_string();
            }
            std::fs::write(row_path(channel), serde_json::to_vec(&row).unwrap()).unwrap();
            let before = row_bytes(channel);
            let key = key(&shared, channel, Some("episode-a"));
            let (_, logs) = logged(claim(&shared, key, probed())).await;
            let declined = lines(&logs, "episode_row_retirement_declined");
            assert_eq!(declined.len(), 1, "case {index}: {logs}");
            assert!(declined[0].contains("reason=\"provider_channel_mismatch\""));
            assert_eq!(row_bytes(channel), before, "case {index}");
        }
    })
    .await;
}

/// Only the exact token the claim observed and then removed proves the release.
#[tokio::test]
async fn drift_retirement_requires_the_released_token_to_be_the_observed_actor() {
    super::super::tests::with_isolated_runtime_root(|| async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(648_480);
        let observed_token = busy(&shared, channel, Some("episode-a")).await;
        let observed = shared.mailbox(channel).snapshot().await;
        let row = drifted_row(channel, Some("episode-a"));
        let before = row_bytes(channel);
        let replacement = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "episode-a".to_string(),
        )));
        let finish = |removed: Option<Arc<CancelToken>>| {
            crate::services::turn_orchestrator::FinishTurnResult {
                removed_token: removed,
                has_pending: false,
                mailbox_online: true,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            }
        };
        let key = key(&shared, channel, Some("episode-a"));
        for (removed, reason) in [
            (None, "mailbox_not_released"),
            (Some(replacement), "actor_changed"),
        ] {
            let (_, logs) = logged(async {
                retire_drifted_row(
                    &ProviderKind::Codex,
                    key,
                    Some(&observed),
                    &row,
                    &finish(removed),
                    probed().unwrap(),
                )
            })
            .await;
            let declined = lines(&logs, "episode_row_retirement_declined");
            assert_eq!(declined.len(), 1, "{reason}: {logs}");
            assert!(
                declined[0].contains(&format!("reason=\"{reason}\"")),
                "{}",
                declined[0]
            );
            assert_eq!(row_bytes(channel), before, "{reason}");
        }
        retire_drifted_row(
            &ProviderKind::Codex,
            key,
            Some(&observed),
            &row,
            &finish(Some(observed_token)),
            probed().unwrap(),
        );
        assert_eq!(
            row_bytes(channel),
            None,
            "the observed actor's release retires it"
        );
    })
    .await;
}

/// A same-episode save after the claim read the row is the episode's own residue, not a
/// successor: it stays, and the WARN names the version change with the release it followed.
#[tokio::test]
async fn a_same_episode_save_after_the_read_keeps_the_row_and_names_its_version() {
    super::super::tests::with_isolated_runtime_root(|| async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(648_490);
        busy(&shared, channel, Some("episode-a")).await;
        let row = drifted_row(channel, Some("episode-a"));
        test_hooks::set_before_cas(
            channel.get(),
            Box::new(move || {
                let mut progressed = load_inflight_state(&ProviderKind::Codex, channel.get())
                    .expect("row still present");
                progressed.last_offset += 1;
                save_inflight_state(&progressed).expect("progress save");
            }),
        );
        let key = key(&shared, channel, Some("episode-a"));
        let (captured, logs) = logged(claim(&shared, key, probed())).await;
        assert!(captured.unwrap().finish.removed_token.is_some());
        let progressed = load_inflight_state(&ProviderKind::Codex, channel.get()).expect("kept");
        assert_eq!(progressed.save_generation, row.save_generation + 1);
        assert_eq!(progressed.last_offset, row.last_offset + 1);
        let declined = lines(&logs, "episode_row_retirement_declined");
        assert_eq!(declined.len(), 1, "{logs}");
        let observed = format!("observed_save_generation={}", row.save_generation + 1);
        for fragment in [
            "gate=\"cas\"",
            "reason=\"snapshot_version_changed\"",
            "mailbox_released=true",
            observed.as_str(),
        ] {
            assert!(
                declined[0].contains(fragment),
                "{fragment}: {}",
                declined[0]
            );
        }
    })
    .await;
}

/// An unreadable row is never retired and is named as such; the shared snapshot clear keeps
/// reporting it as `Missing`, and a truly absent row stays a silent `Missing`.
#[tokio::test]
async fn an_unreadable_row_is_named_and_kept_while_the_shared_clear_keeps_its_answer() {
    super::super::tests::with_isolated_runtime_root(|| async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(648_500);
        busy(&shared, channel, Some("episode-a")).await;
        let row = drifted_row(channel, Some("episode-a"));
        let path = row_path(channel);
        let damaged = path.clone();
        test_hooks::set_before_cas(
            channel.get(),
            Box::new(move || std::fs::write(&damaged, b"{not json").unwrap()),
        );
        let key = key(&shared, channel, Some("episode-a"));
        let (_, logs) = logged(claim(&shared, key, probed())).await;
        let declined = lines(&logs, "episode_row_retirement_declined");
        assert_eq!(declined.len(), 1, "{logs}");
        assert!(
            declined[0].contains("reason=\"row_parse_failed\""),
            "{}",
            declined[0]
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");

        let (outcome, logs) = logged(async {
            inflight::clear_inflight_state_for_snapshot(&ProviderKind::Codex, &row)
        })
        .await;
        assert_eq!(outcome, inflight::GuardedClearOutcome::Missing);
        assert_eq!(
            lines(&logs, "reason=\"row_parse_failed\"").len(),
            1,
            "{logs}"
        );
        std::fs::remove_file(&path).unwrap();
        let (outcome, logs) = logged(async {
            inflight::clear_inflight_state_for_snapshot(&ProviderKind::Codex, &row)
        })
        .await;
        assert_eq!(outcome, inflight::GuardedClearOutcome::Missing);
        assert!(lines(&logs, "WARN").is_empty(), "{logs}");
    })
    .await;
}

/// Marks the episode's ledger entry finalized without releasing anything: its recovery actor
/// is already gone, so the first submission only settles the ledger.
async fn finalize_ledger_only(shared: &Arc<SharedData>, key: TurnKey, row: &InflightTurnState) {
    let dead = Arc::new(CancelToken::new());
    let mut snapshot = SyntheticClaimSnapshot::from_row(row);
    snapshot.recovery_actor = Some(Arc::downgrade(&dead));
    drop(dead);
    let first = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            key,
            ProviderKind::Codex,
            TerminalEvent::Complete,
            FinalizeContext::watcher(),
            Some(snapshot),
            shared.clone(),
        )
        .await;
    assert!(matches!(first, FinalizeOutcome::AlreadyFinalized));
}

/// An `AlreadyFinalized` duplicate's own proven release retires the drifted row whatever its
/// context, given runtime evidence; without evidence the row stays with that reason named.
#[tokio::test]
async fn an_already_finalized_compensation_retires_with_its_own_release() {
    super::super::tests::with_isolated_runtime_root(|| async {
        for (index, submitter) in ["stale_busy", "watcher_snapshot", "bridge"]
            .into_iter()
            .enumerate()
        {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(648_510 + index as u64);
            let token = busy(&shared, channel, Some("episode-a")).await;
            let row = drifted_row(channel, Some("episode-a"));
            let key = key(&shared, channel, Some("episode-a"));
            finalize_ledger_only(&shared, key, &row).await;
            assert!(!token.cancelled.load(Ordering::Acquire), "{submitter}");
            let before = row_bytes(channel);
            let fin = &shared.turn_finalizer;
            let nonce = Some("episode-a".to_string());
            let (outcome, logs) = logged(async {
                match submitter {
                    "stale_busy" => {
                        fin.submit_stale_busy_cancel(
                            key,
                            ProviderKind::Codex,
                            nonce,
                            TMUX.to_string(),
                            shared.clone(),
                        )
                        .await
                    }
                    "watcher_snapshot" => {
                        fin.submit_terminal_with_claim_snapshot(
                            key,
                            ProviderKind::Codex,
                            TerminalEvent::Complete,
                            FinalizeContext::watcher(),
                            Some(SyntheticClaimSnapshot::from_row(&row)),
                            shared.clone(),
                        )
                        .await
                    }
                    _ => {
                        let ctx = FinalizeContext::bridge();
                        fin.submit_terminal_with_episode_nonce(
                            key,
                            ProviderKind::Codex,
                            TerminalEvent::Complete,
                            ctx,
                            nonce,
                            shared.clone(),
                        )
                        .await
                    }
                }
            })
            .await;
            assert!(
                matches!(outcome, FinalizeOutcome::AlreadyFinalized),
                "{submitter}"
            );
            assert!(
                token.cancelled.load(Ordering::Acquire),
                "{submitter}: released"
            );
            let declined = lines(&logs, "episode_row_retirement_declined");
            if submitter == "bridge" {
                assert_eq!(declined.len(), 1, "{logs}");
                assert!(declined[0].contains("reason=\"runtime_identity_mismatch\""));
                assert_eq!(row_bytes(channel), before, "{submitter}");
            } else {
                assert!(declined.is_empty(), "{submitter}: {declined:?}");
                assert_eq!(row_bytes(channel), None, "{submitter}: retired");
            }
        }
    })
    .await;
}

/// A finalize that does not own row cleanup releases the episode but never retires its row.
#[tokio::test]
async fn a_cleanup_less_finalize_releases_without_retiring_the_drifted_row() {
    super::super::tests::with_isolated_runtime_root(|| async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(648_520);
        let token = busy(&shared, channel, Some("episode-a")).await;
        drifted_row(channel, Some("episode-a"));
        let before = row_bytes(channel);
        let key = key(&shared, channel, Some("episode-a"));
        let (outcome, logs) = logged(shared.turn_finalizer.submit_terminal_with_episode_nonce(
            key,
            ProviderKind::Codex,
            TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some("episode-a".to_string()),
            shared.clone(),
        ))
        .await;
        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: Some(_),
                ..
            }
        ));
        assert!(token.cancelled.load(Ordering::Acquire));
        assert!(lines(&logs, "episode_row_retire").is_empty(), "{logs}");
        assert_eq!(row_bytes(channel), before);
    })
    .await;
}

/// The pre-panel release keeps its same-id cleanup contract: a drifted row stays.
#[cfg(unix)]
#[tokio::test]
async fn the_pre_panel_release_never_retires_a_drifted_row() {
    super::super::tests::with_isolated_runtime_root(|| async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = ChannelId::new(648_530);
        let token = busy(&shared, channel, Some("episode-a")).await;
        let row = drifted_row(channel, Some("episode-a"));
        assert_eq!(row.turn_source, inflight::TurnSource::Managed);
        let before = row_bytes(channel);
        let key = key(&shared, channel, Some("episode-a"));
        let (released, logs) = logged(
            super::super::cleanup::terminal_release::release_restored_watcher_active_turn_before_panel_edit(
                &shared,
                &ProviderKind::Codex,
                Some(key),
            ),
        )
        .await;
        assert!(released);
        assert!(token.cancelled.load(Ordering::Acquire));
        assert!(lines(&logs, "episode_row_retire").is_empty(), "{logs}");
        assert_eq!(row_bytes(channel), before);
    })
    .await;
}

/// An intake turn's start up to its row create; `None` once that create met a durable row.
async fn start_turn(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    message: u64,
) -> (Arc<CancelToken>, InflightTurnState, bool) {
    let token = Arc::new(CancelToken::new());
    let id = MessageId::new(message);
    let claimed = crate::services::discord::mailbox_try_start_turn(
        shared,
        channel,
        token.clone(),
        UserId::new(7),
        id,
    )
    .await;
    assert!(claimed, "the mailbox admits turn {message}");
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel.get(),
        None,
        7,
        message,
        0,
        "turn b".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    state.source_message_ids = vec![message];
    state.busy_followup_retry_user_msg_id = message;
    state.set_followup_requeue_context(None, false, false, Vec::new(), None, true);
    let created = inflight::save_inflight_state_create_new(&state).is_ok();
    (token, state, created)
}

/// Intake B admitted between A's release and the retirement meets A's row and is deferred
/// exactly once, then starts once after the retirement; a headless B answers Conflict with
/// nothing queued; a B admitted after the retirement is never touched by A's late duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_admitted_around_the_retirement_keep_their_input_and_row() {
    super::super::tests::with_isolated_runtime_root(|| async {
        for (index, headless) in [false, true].into_iter().enumerate() {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let provider = ProviderKind::Codex;
            let channel = ChannelId::new(648_540 + index as u64);
            busy(&shared, channel, Some("episode-a")).await;
            drifted_row(channel, Some("episode-a"));
            let deferred = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (hook_shared, hook_deferred) = (shared.clone(), deferred.clone());
            let runtime = tokio::runtime::Handle::current();
            test_hooks::set_before_cas(
                channel.get(),
                Box::new(move || {
                    tokio::task::block_in_place(|| {
                        runtime.block_on(async {
                            let (token, state, created) =
                                start_turn(&hook_shared, channel, 648_601).await;
                            assert!(!created, "A's row still holds the channel");
                            if headless {
                                crate::services::discord::mailbox_finish::unwind_unstarted_turn(
                                    &hook_shared,
                                    channel,
                                    &token,
                                )
                                .await;
                                return;
                            }
                            let reason = "foreign_row_start_deferred";
                            let queued =
                                crate::services::discord::live_bridge::defer_unstarted_turn(
                                    &hook_shared,
                                    &ProviderKind::Codex,
                                    &state,
                                    &token,
                                    true,
                                    reason,
                                )
                                .await;
                            assert!(queued);
                            hook_deferred.fetch_add(1, Ordering::Relaxed);
                        })
                    })
                }),
            );
            let key = key(&shared, channel, Some("episode-a"));
            shared
                .turn_finalizer
                .submit_stale_busy_cancel(
                    key,
                    provider.clone(),
                    Some("episode-a".to_string()),
                    TMUX.to_string(),
                    shared.clone(),
                )
                .await;
            assert_eq!(row_bytes(channel), None, "A's row retired");
            let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            assert!(snapshot.cancel_token.is_none(), "B's claim was released");
            if headless {
                assert!(
                    snapshot.intervention_queue.is_empty(),
                    "the caller keeps the input"
                );
                continue;
            }
            assert_eq!(deferred.load(Ordering::Relaxed), 1);
            let taken = crate::services::discord::idle_queue_take_next_soft_if_ready(
                &shared, &provider, channel,
            )
            .await;
            let (item, _, _lease) = taken.into_intervention().expect("B requeued");
            assert_eq!(item.message_id.get(), 648_601);
            let (b_token, mut b_row, created) = start_turn(&shared, channel, 648_601).await;
            assert!(created, "B starts once after the retirement");
            let entry = b_row.clone();
            assert_eq!(
                inflight::patch_bridge_entry_state_if_identity_unchanged(
                    &entry, &mut b_row, "test"
                ),
                GuardedSaveOutcome::Saved
            );
            let b_bytes = row_bytes(channel);
            let late = shared
                .turn_finalizer
                .submit_stale_busy_cancel(
                    key,
                    provider.clone(),
                    Some("episode-a".to_string()),
                    TMUX.to_string(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            let live = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            assert!(Arc::ptr_eq(live.cancel_token.as_ref().unwrap(), &b_token));
            assert!(!b_token.cancelled.load(Ordering::Acquire));
            assert_eq!(row_bytes(channel), b_bytes, "B's row bytes unchanged");
            assert!(live.intervention_queue.is_empty());
        }
    })
    .await;
}
