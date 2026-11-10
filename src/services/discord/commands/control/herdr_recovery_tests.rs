//! The Herdr recovery clear's discard settles the inputs it removes, as the plain clear does.
use super::*;
use crate::services::turn_orchestrator::{QueuePersistenceContext, v2_queue};

/// An origin the recovery discard removes gets its cancelled receipt and is owed no longer.
#[tokio::test(flavor = "current_thread")]
async fn the_recovery_discard_settles_each_removed_origin_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_340_503));
    let persistence = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    let mailbox = shared.mailbox(channel);
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let id = 8_000_000_000_000_340_531_u64;
    let original = serde_json::from_value(serde_json::json!({"author_id":1,"message_id":id,
        "source_message_ids":[id],"channel_id":channel.get(),"queued_generation":5,"text":"x"}))
    .unwrap();
    let context =
        serde_json::json!({"origin":"headless","metadata":{"human_input":{"origin_id":"G-h"}}});
    let admitted = mailbox.v2_admit_ticket_with_context(original, "t-h".into(), context);
    admitted.await.unwrap();

    let mut report = ClearReport::default();
    discard_waiting(
        &shared,
        &provider,
        channel,
        None,
        &BTreeSet::new(),
        &mut report,
    )
    .await;
    let sql = "SELECT turn_id, terminal_outcome FROM turns WHERE channel_id = $1";
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sql)
        .bind(channel.get().to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
    let turn = format!("discord:{channel}:{id}");
    assert_eq!(rows, [(turn, Some("cancelled".to_string()))], "{report:?}");
    let owed = mailbox.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    assert!(owed.await.unwrap().is_empty());
    pool.close().await;
    db.drop().await;
}

/// A Herdr clear forgets the provider selector only of an execution it saw retired, field by field.
mod retired_selector {
    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::http_force::clear_retired_selector_pg;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
    };
    use crate::services::discord::commands::SESSION_CLEARED_RESPONSE;
    use crate::services::discord::host_defer_gate::tests as gate;
    use crate::services::session_host::herdr_termination_rig as herdr;

    const SELECTOR: &str = "pre-clear-sid";
    const LIVE: u64 = 5_340_221;
    const REPLACED: u64 = 5_340_222;
    const HELD: u64 = 5_340_223;
    const EXACT: u64 = 5_340_224;

    fn runtime(rig: &herdr::P11TerminationRig) -> tokio::runtime::Runtime {
        let install = rig.thread_installer();
        install();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .on_thread_start(install)
            .build()
            .unwrap()
    }

    struct Seeded {
        db: crate::db::auto_queue::test_support::TestPostgresDb,
        pool: sqlx::PgPool,
        shared: Arc<crate::services::discord::SharedData>,
        channel: ChannelId,
        key: String,
        _switches: herdr::LiveSwitches,
    }

    /// The channel's execution in `state` under this runtime's root, its row holding `SELECTOR`.
    async fn seeded(
        rig: &herdr::P11TerminationRig,
        channel: u64,
        state: herdr::HostedState,
    ) -> Seeded {
        let (db, pool) = gate::postgres().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let channel = ChannelId::new(channel);
        gate::map_channel(&shared, channel, "p11-clear").await;
        let key = crate::services::discord::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &ProviderKind::Claude, None)
            .await
            .unwrap();
        let switches = rig.bind(&pool, &shared.token_hash, &key, state).await;
        let root = crate::config::runtime_root().unwrap().display().to_string();
        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,runtime_root}', to_jsonb($2::text)), '{source_ref,runtime_root}', to_jsonb($2::text)), claude_session_id = $3, raw_provider_session_id = $3 WHERE session_key = $1")
            .bind(&key).bind(root).bind(SELECTOR).execute(&pool).await.unwrap();
        let seeded = Seeded {
            db,
            pool,
            shared,
            channel,
            key,
            _switches: switches,
        };
        assert_eq!(seeded.selected().await.as_deref(), Some(SELECTOR));
        seeded
    }

    impl Seeded {
        /// The selector the provider-session route hands a turn whose clear flag is gone.
        async fn selected(&self) -> Option<String> {
            let load = crate::db::dispatched_sessions::load_provider_session_ids_pg;
            let ids = load(&self.pool, &self.key, None).await.unwrap().unwrap();
            let select = crate::services::dispatched_sessions::selected_provider_resume_selector_for_provider;
            select(None, &ids).map(str::to_owned)
        }

        async fn clear(&self) -> ClearReport {
            let http = Arc::new(poise::serenity_prelude::Http::new(""));
            let clear = super::super::super::clear_channel_session_state;
            let provider = ProviderKind::Claude;
            clear(&http, &self.shared, &provider, self.channel, "!clear")
                .await
                .expect("a clear always commits")
        }

        /// Takes the one-shot clear flag as an intake does, handing back whether it was set.
        async fn take_clear_flag(&self) -> bool {
            let mut core = self.shared.core.lock().await;
            let session = core
                .sessions
                .get_mut(&self.channel)
                .expect("mapped channel");
            std::mem::take(&mut session.cleared)
        }

        async fn record(
            &self,
        ) -> (
            i64,
            crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        ) {
            match load_hosted_execution_pg(&self.pool, HostedLookupKey::SessionKey(&self.key)).await
            {
                HostedLookup::Found(row) => match row.record.clone() {
                    HostedRecord::Known(record) => (row.session_id(), record),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        }

        async fn done(self) {
            self.pool.close().await;
            self.db.drop().await;
        }
    }

    fn retired() -> Option<(herdr::HostedState, String)> {
        Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()))
    }

    // The clear of a live execution terminates it; a later input that lost the clear flag to a
    // requeue still finds no selector to resume, so the cleared reply holds.
    #[test]
    fn live_clear_forgets_the_retired_selector_pg() {
        let path = concat!(
            module_path!(),
            "::live_clear_forgets_the_retired_selector_pg"
        );
        if !herdr::in_child(path, LIVE) {
            return;
        }
        let rig = herdr::P11TerminationRig::boot(LIVE);
        runtime(&rig).block_on(async {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let seeded = seeded(&rig, LIVE, herdr::HostedState::Bound).await;

            let report = seeded.clear().await;

            let cleared = format!("{SESSION_CLEARED_RESPONSE}\n대기 메시지 0건을 비웠어요.");
            assert_eq!(report.reply(), cleared, "{report:?}");
            assert_eq!(herdr::row(&seeded.pool, &seeded.key).await, retired());
            assert!(seeded.take_clear_flag().await, "the clear set its flag");
            assert_eq!(seeded.selected().await, None, "the next input starts fresh");
            seeded.done().await;
        });
    }

    // A replaced root's clear retires the execution without a close and forgets its selector.
    #[test]
    fn replaced_root_clear_forgets_the_retired_selector_pg() {
        let path = concat!(
            module_path!(),
            "::replaced_root_clear_forgets_the_retired_selector_pg"
        );
        if !herdr::in_child(path, REPLACED) {
            return;
        }
        let rig = herdr::P11TerminationRig::boot(REPLACED);
        runtime(&rig).block_on(async {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let seeded = seeded(&rig, REPLACED, herdr::HostedState::Bound).await;
            rig.restore_shell();

            let report = seeded.clear().await;

            let cleared = format!("{SESSION_CLEARED_RESPONSE}\n대기 메시지 0건을 비웠어요.");
            assert_eq!(report.reply(), cleared, "{report:?}");
            assert!(rig.writes().is_empty(), "the restored shell is not closed");
            assert_eq!(herdr::row(&seeded.pool, &seeded.key).await, retired());
            assert!(seeded.take_clear_flag().await, "the clear set its flag");
            assert_eq!(seeded.selected().await, None, "the next input starts fresh");
            seeded.done().await;
        });
    }

    // A retired row whose execution is still admitted holds the clear: the reply says so and the
    // previous session, its selector and the absent clear flag all stay.
    #[test]
    fn held_clear_keeps_the_selector_and_says_so_pg() {
        let path = concat!(
            module_path!(),
            "::held_clear_keeps_the_selector_and_says_so_pg"
        );
        if !herdr::in_child(path, HELD) {
            return;
        }
        let rig = herdr::P11TerminationRig::boot(HELD);
        runtime(&rig).block_on(async {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let seeded = seeded(&rig, HELD, herdr::HostedState::Retired).await;
            let (_, record) = seeded.record().await;
            crate::services::tui_prompt_dedupe::admit_herdr_execution(
                &record.owner.logical_key,
                &record.execution_nonce,
            );

            let reply = seeded.clear().await.reply();

            assert_eq!(
                reply,
                "대기 메시지 0건을 비웠어요.\n이전 Herdr 실행은 정리하지 못했어요. Herdr execution is admitted"
            );
            assert!(!seeded.take_clear_flag().await, "a held clear sets no flag");
            assert_eq!(seeded.selected().await.as_deref(), Some(SELECTOR));
            seeded.done().await;
        });
    }

    // Only the exact retired record clears: another nonce or owner, or a row not retired, keeps the
    // selector and the clear reports it as not landed.
    #[test]
    fn retired_selector_clear_requires_the_exact_retired_record_pg() {
        let path = concat!(
            module_path!(),
            "::retired_selector_clear_requires_the_exact_retired_record_pg"
        );
        if !herdr::in_child(path, EXACT) {
            return;
        }
        let rig = herdr::P11TerminationRig::boot(EXACT);
        runtime(&rig).block_on(async {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let seeded = seeded(&rig, EXACT, herdr::HostedState::Retired).await;
            let (id, record) = seeded.record().await;
            let mut other_nonce = record.clone();
            other_nonce.execution_nonce = "another-execution".into();
            let mut other_owner = record.clone();
            other_owner.owner.logical_key = "AgentDesk-claude-another".into();
            for other in [&other_nonce, &other_owner] {
                let cleared = clear_retired_selector_pg(&seeded.pool, id, other).await;
                assert_eq!(cleared, Ok(false), "{other:?}");
                let mut report = ClearReport::default();
                super::super::super::clear_teardown::note_retired_selector(cleared, &mut report);
                assert_eq!(report.failed, ["session_selector"]);
                assert_eq!(seeded.selected().await.as_deref(), Some(SELECTOR));
            }
            sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{state}', '\"bound\"') WHERE id = $1")
                .bind(id).execute(&seeded.pool).await.unwrap();
            let live = clear_retired_selector_pg(&seeded.pool, id, &record).await;
            assert_eq!(live, Ok(false), "a live execution keeps its selector");
            assert_eq!(seeded.selected().await.as_deref(), Some(SELECTOR));
            sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{state}', '\"retired\"') WHERE id = $1")
                .bind(id).execute(&seeded.pool).await.unwrap();

            let exact = clear_retired_selector_pg(&seeded.pool, id, &record).await;

            assert_eq!(exact, Ok(true));
            let mut report = ClearReport::default();
            super::super::super::clear_teardown::note_retired_selector(exact, &mut report);
            assert!(report.failed.is_empty());
            assert_eq!(seeded.selected().await, None);
            seeded.done().await;
        });
    }
}
