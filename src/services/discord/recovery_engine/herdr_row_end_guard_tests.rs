//! What keeps a restarted row: reads short of a clear idle or end, a live actor, routing, races
//! with a successor, and what an O-owned channel or a mixed read shows.
use super::*;

const NEGATIVE: u64 = 1_479_671_399_000_105;

/// A long busy turn, a stale ready screen over an open turn, an unreadable endpoint or a replaced
/// source keeps the row, its execution and its hold, and shows nothing.
#[test]
fn unresolved_reads_keep_the_row_as_production_does_pg() {
    let path = concat!(
        module_path!(),
        "::unresolved_reads_keep_the_row_as_production_does_pg"
    );
    in_child(
        path,
        NEGATIVE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: NEGATIVE,
                    routed: true,
                },
            )
            .await;
            let saved = run.row.clone();
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            run.pass().await;
            assert_kept(&run, &saved, "long busy turn").await;
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert_kept(&run, &saved, "stale ready screen, turn still open").await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.garbled = true);
            run.pass().await;
            assert_kept(&run, &saved, "unreadable endpoint").await;
            // The same records under a new file: not the source the execution logged.
            run.relay.set(|pane| {
                pane.garbled = false;
                pane.screen = Some(READY.into());
            });
            replace_source(&run.source);
            run.pass().await;
            assert_kept(&run, &saved, "replaced source").await;
            run.close().await;
        },
    );
}

const LIVE: u64 = 1_479_671_399_000_106;

/// A clear idle with a live turn on the channel's mailbox, or that turn's nonce indexed, is
/// another turn's: the row stays.
#[test]
fn a_live_actor_keeps_an_idle_row_pg() {
    let path = concat!(module_path!(), "::a_live_actor_keeps_an_idle_row_pg");
    in_child(
        path,
        LIVE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: LIVE,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let token = Arc::new(
                crate::services::provider::CancelToken::from_persisted_turn_nonce(Some(
                    "live-turn".into(),
                )),
            );
            run.shared
                .mailbox(run.channel)
                .restore_active_turn(
                    token,
                    serenity::model::id::UserId::new(7),
                    serenity::model::id::MessageId::new(61),
                )
                .await;
            // A row from before generations were stamped, as the live turn's own row looks.
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &run.provider,
                run.channel.get(),
            );
            let mut json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            json["born_generation"] = 0.into();
            std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            let saved = run.durable().unwrap();
            run.pass().await;
            assert_kept(&run, &saved, "live mailbox turn").await;
            run.close().await;
        },
    );
}

const UNROUTED: u64 = 1_479_671_399_000_107;

/// A channel routing no longer admits keeps even an ended execution's row: nothing is retired.
#[test]
fn an_unrouted_channel_keeps_an_ended_row_pg() {
    let path = concat!(
        module_path!(),
        "::an_unrouted_channel_keeps_an_ended_row_pg"
    );
    in_child(
        path,
        UNROUTED,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: UNROUTED,
                    routed: false,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            run.pass().await;
            assert_kept(&run, &run.row, "unrouted").await;
            run.close().await;
        },
    );
}

const RETIRE_RACE: u64 = 1_479_671_399_000_108;

/// Another retire and a successor row between the reads and this retire: the CAS refuses, so the
/// successor's row and the hold stay and nothing is shown.
#[test]
fn a_successor_before_the_retire_keeps_everything_pg() {
    let path = concat!(
        module_path!(),
        "::a_successor_before_the_retire_keeps_everything_pg"
    );
    in_child(
        path,
        RETIRE_RACE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: RETIRE_RACE,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let mut successor = run.row.clone();
            successor.turn_nonce = Some("successor".into());
            successor.born_generation = CURRENT;
            let (pool, key, next) = (run.pool.clone(), run.key.clone(), successor.clone());
            on("before_retire", move || {
                let (pool, key, next) = (pool.clone(), key.clone(), next.clone());
                Box::pin(async move {
                    let lookup = HostedLookupKey::SessionKey(&key);
                    let HostedLookup::Found(found) =
                        hosted::load_hosted_execution_pg(&pool, lookup).await
                    else {
                        panic!("the bound execution");
                    };
                    let HostedRecord::Known(record) = found.record.clone() else {
                        panic!("a known record");
                    };
                    let nonce = record.execution_nonce.clone();
                    let written = hosted::retire_pg(&pool, &found, &record.owner, &nonce).await;
                    assert!(matches!(written, Ok(hosted::HostedCasOutcome::Written)));
                    inflight::save_inflight_state(&next).unwrap();
                })
            });
            run.pass().await;
            let durable = run.durable().expect("the successor's row");
            assert_eq!(durable.turn_nonce.as_deref(), Some("successor"));
            let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
            assert_eq!(
                run.hosted().await,
                Some(retired),
                "only the other retire wrote"
            );
            assert!(herdr::held(), "this pass released nothing");
            assert_eq!(run.texts(), Vec::<String>::new());
            run.close().await;
        },
    );
}

const CLEAR_RACE: u64 = 1_479_671_399_000_109;

/// A successor row written after the notice: the clear sees another turn and keeps it.
#[test]
fn a_successor_before_the_clear_keeps_its_row_pg() {
    let path = concat!(
        module_path!(),
        "::a_successor_before_the_clear_keeps_its_row_pg"
    );
    in_child(
        path,
        CLEAR_RACE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: CLEAR_RACE,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let mut successor = run.row.clone();
            successor.turn_nonce = Some("successor".into());
            successor.born_generation = CURRENT;
            on("before_clear", move || {
                let next = successor.clone();
                Box::pin(async move { inflight::save_inflight_state(&next).unwrap() })
            });
            run.pass().await;
            let durable = run.durable().expect("the successor's row");
            assert_eq!(durable.turn_nonce.as_deref(), Some("successor"));
            assert_eq!(run.texts().iter().filter(|t| t.contains(ANSWER)).count(), 1);
            run.close().await;
        },
    );
}

const SLOW: u64 = 1_479_671_399_000_110;

/// The pass only starts the end task: a slow end leaves the tick free for other channels.
#[test]
fn the_pass_never_waits_on_an_end_task_pg() {
    let path = concat!(module_path!(), "::the_pass_never_waits_on_an_end_task_pg");
    in_child(
        path,
        SLOW,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: SLOW,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            on("before_clear", || {
                Box::pin(tokio::time::sleep(Duration::from_secs(10)))
            });
            let started = Instant::now();
            row_end_pass(&run.shared, &run.provider).await;
            let pass = started.elapsed();
            assert!(pass < Duration::from_secs(2), "the pass took {pass:?}");
            settled_tasks().await;
            assert!(run.durable().is_none(), "the slow end still finished");
            run.close().await;
        },
    );
}

const O_OWNED: u64 = 1_479_671_399_000_111;

/// An O-owned channel whose turn closed with its terminal shows nothing at all: O relayed the turn,
/// so the row ends without the marker, the recovered body or the saved partial.
#[test]
fn an_o_owned_relayed_turn_ends_without_a_notice_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_relayed_turn_ends_without_a_notice_pg"
    );
    in_child(
        path,
        O_OWNED,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: O_OWNED,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.pass().await;
            let ended = wait(Duration::from_secs(30), || run.durable().is_none()).await;
            assert!(ended, "the row ends: {:?}", run.texts());
            let marker = interrupted_recovery_message(&run.row, "");
            let texts = run.texts();
            assert!(
                !texts.iter().any(|t| t.contains(&marker)
                    || t.contains(ANSWER)
                    || t.contains(PARTIAL)
                    || t.contains("완료 여부는 확인하지 못했습니다")),
                "{texts:?}"
            );
            assert!(!herdr::held(), "the execution's input hold is released");
            let once = wait(Duration::from_secs(20), || {
                !taken.lock().unwrap().is_empty()
            })
            .await;
            assert!(once, "the next input was taken");
            assert_eq!(
                *taken.lock().unwrap(),
                vec![false],
                "taken once, after the clear"
            );
            run.close().await;
        },
    );
}

const TAIL: u64 = 1_479_671_399_000_112;

/// The previous turn's tail stamped in the same second as the input mixes the read: the neutral
/// notice, neither the interrupt message nor another turn's text.
#[test]
fn a_same_second_previous_tail_falls_back_to_the_notice_pg() {
    let path = concat!(
        module_path!(),
        "::a_same_second_previous_tail_falls_back_to_the_notice_pg"
    );
    in_child(
        path,
        TAIL,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: TAIL,
                    routed: true,
                },
            )
            .await;
            let tail = json!({"type": "assistant", "timestamp": stamp(&run.row),
            "message": {"content": [{"type": "text", "text": "previous turn"}]}});
            let open = std::fs::read_to_string(&run.source).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&run.source)
                .unwrap();
            file.write_all(format!("{tail}\n{open}").as_bytes())
                .unwrap();
            drop(file);
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.pass().await;
            assert_ended(&run, "이전 요청의 완료 여부는 확인하지 못했습니다", &taken).await;
            let texts = run.texts();
            assert!(
                !texts.iter().any(|t| t.contains(&run.interrupted())),
                "{texts:?}"
            );
            assert!(
                !texts
                    .iter()
                    .any(|t| t.contains(ANSWER) || t.contains("previous turn"))
            );
            run.close().await;
        },
    );
}

const DRAFT: u64 = 1_479_671_399_000_113;

/// A closed Codex rollout under a ready-looking composer that holds a draft is not idle: the row,
/// its execution and its hold stay, and nothing is shown or taken.
#[test]
fn a_codex_draft_keeps_a_closed_turn_pg() {
    let path = concat!(module_path!(), "::a_codex_draft_keeps_a_closed_turn_pg");
    in_child(
        path,
        DRAFT,
        ProviderKind::Codex,
        Owner::Legacy,
        |rig| async move {
            use crate::services::codex_tui::input::{self as codex, ComposerContent};
            assert!(codex::pane_looks_ready_for_codex_prompt(CODEX_DRAFT));
            assert_ne!(
                codex::active_composer_content_in_pane(CODEX_DRAFT),
                ComposerContent::Empty
            );
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Codex,
                    channel: DRAFT,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Codex, &run.row));
            run.relay.set(|pane| pane.screen = Some(CODEX_DRAFT.into()));
            let (taken, _reset) = run.sentinel().await;
            let saved = run.row.clone();
            run.pass().await;
            assert_kept(&run, &saved, "a draft in the composer").await;
            assert!(taken.lock().unwrap().is_empty(), "no input taken");
            run.close().await;
        },
    );
}

const SURVIVOR: u64 = 1_479_671_399_000_114;

/// A pane back at its shell while the recorded provider process still lives is no end: the row,
/// the bound execution and its hold stay, and nothing is shown or taken.
#[test]
fn a_surviving_provider_keeps_an_exited_pane_row_pg() {
    let path = concat!(
        module_path!(),
        "::a_surviving_provider_keeps_an_exited_pane_row_pg"
    );
    in_child(
        path,
        SURVIVOR,
        ProviderKind::Claude,
        Owner::Alive,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: SURVIVOR,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let (taken, _reset) = run.sentinel().await;
            let saved = run.row.clone();
            run.pass().await;
            assert_kept(&run, &saved, "provider alive").await;
            assert!(taken.lock().unwrap().is_empty(), "no input taken");
            run.close().await;
        },
    );
}
