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
            let bytes = std::fs::read(&run.source).unwrap();
            std::fs::remove_file(&run.source).unwrap();
            std::fs::write(&run.source, bytes).unwrap();
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

/// A successor launched between the reads and the retire: the CAS refuses, so the successor's
/// row, execution and hold stay and nothing is shown.
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
                    sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{execution_nonce}', '\"successor-execution\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                    inflight::save_inflight_state(&next).unwrap();
                })
            });
            run.pass().await;
            let durable = run.durable().expect("the successor's row");
            assert_eq!(durable.turn_nonce.as_deref(), Some("successor"));
            let hosted: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT hosted_execution->>'execution_nonce', hosted_execution->>'state' FROM sessions WHERE session_key = $1",
            )
            .bind(&run.key)
            .fetch_one(&run.pool)
            .await
            .unwrap();
            assert_eq!(hosted.0.as_deref(), Some("successor-execution"));
            assert_ne!(hosted.1.as_deref(), Some("retired"), "{hosted:?}");
            assert!(herdr::held());
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

/// An O-owned channel shows only the body-free marker, never the recovered body, and still ends.
#[test]
fn an_o_owned_channel_shows_only_the_marker_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_channel_shows_only_the_marker_pg"
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
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            assert!(
                !run.texts().iter().any(|t| t.contains(ANSWER)),
                "{:?}",
                run.texts()
            );
            run.close().await;
        },
    );
}

const TAIL: u64 = 1_479_671_399_000_112;

/// The previous turn's tail stamped in the same second as the input mixes the read: the notice
/// falls back to the interrupt message instead of another turn's text.
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
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            assert!(
                !run.texts()
                    .iter()
                    .any(|t| t.contains(ANSWER) || t.contains("previous turn"))
            );
            run.close().await;
        },
    );
}
