//! A text `!clear` overlapping a forced HTTP cancel of the channel's Herdr execution: the clear
//! discards the waiting input it finds once, and the cancel records only what its purge removed.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use futures::FutureExt;
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use serenity::{ChannelId, MessageId, UserId};

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::admin_host_guard::tests::Recorder;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::{probe_before_postlude, root_replaced, test_turn};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, SharedData, inflight, queue_persistence_context};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, load_channel_pending_queue_for_tests,
};

const CHANNEL: u64 = 6_772_301;
const AUTH: &str = "c2-clear-overlap";
const DISPATCH: &str = "dispatch-c2";

/// Who goes first and where the first one is held while the other runs. The rig's pane vanishes
/// at its first close and reads as a shell after a clear's recovery, so each side has its child.
const HTTP_FIRST: [&str; 2] = ["http-first-before-postlude", "http-first-after-capture"];
const CLEAR_FIRST: [&str; 1] = ["clear-first"];

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

fn input(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// The rig's bound execution on a throwaway PG, a runtime registered for the HTTP route, and the
/// text dispatcher over a recording Discord.
struct Overlap {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    registry: Arc<HealthRegistry>,
    text: Arc<Text>,
    gateway: tokio::task::JoinHandle<()>,
    _voice: tempfile::TempDir,
    _switches: herdr::LiveSwitches,
}

/// The text dispatcher's side: a recording Discord and the context and data it runs on.
struct Text {
    api: Recorder,
    ctx: serenity::Context,
    data: Data,
}

impl Overlap {
    /// With a running dispatched turn unless `idle`; an idle row's pane reads as a restored shell,
    /// so the clear's recovery retires it under the transition.
    async fn bind(rig: &herdr::P11TerminationRig, idle: bool) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
        let provider = ProviderKind::Claude;
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel(), "c2")
            .await;
        let key = crate::services::discord::adk_session::build_adk_session_key;
        let key = key(&shared, channel(), &provider, None).await.unwrap();
        let bound = herdr::HostedState::Bound;
        let switches = rig.bind(&pool, &shared.token_hash, &key, bound).await;
        let root = crate::config::runtime_root().unwrap().display().to_string();
        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,runtime_root}', to_jsonb($2::text)), '{source_ref,runtime_root}', to_jsonb($2::text)) WHERE session_key = $1")
            .bind(&key).bind(root).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
            .bind(DISPATCH)
            .execute(&pool)
            .await
            .unwrap();
        let dispatch = (!idle).then_some(DISPATCH);
        sqlx::query("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2, last_heartbeat = NOW() WHERE session_key = $1")
            .bind(&key).bind(dispatch).execute(&pool).await.unwrap();
        if idle {
            rig.restore_shell();
        } else {
            test_turn::start(&shared, channel(), 51, "turn-a").await;
            let mut row = inflight::load_inflight_state(&provider, CHANNEL).unwrap();
            row.dispatch_id = Some(DISPATCH.into());
            inflight::save_inflight_state(&row).unwrap();
        }
        let api = Recorder::start().await;
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let rest = format!("http://127.0.0.1:{}", api.port);
        let ctx = discord_mock::serenity_context(rest, gateway_url).await;
        let voice = tempfile::tempdir().unwrap();
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.audio.recordings_dir = voice.path().to_path_buf();
        voice_config.keep_recordings = true;
        let data = Data {
            shared: shared.clone(),
            token: "c2-clear-overlap".into(),
            provider,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        Self {
            db,
            pool,
            shared,
            registry,
            text: Arc::new(Text { api, ctx, data }),
            gateway,
            _voice: voice,
            _switches: switches,
        }
    }

    async fn enqueue(shared: &Arc<SharedData>, id: u64, text: &str) {
        let context = queue_persistence_context(shared, &ProviderKind::Claude, channel());
        let mailbox = shared.mailbox(channel());
        let queued = mailbox.enqueue(input(id, text), context).await;
        assert!(queued.enqueued, "{text} queued");
    }

    fn app(&self) -> axum::Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let (pool, registry) = (Some(self.pool.clone()), Some(self.registry.clone()));
        crate::server::routes::cancel_route_test_app(config, pool, registry, herdr::NODE)
    }

    /// The forced cancel the HTTP route takes; the router is built per request.
    fn cancel(&self) -> impl std::future::Future<Output = (StatusCode, Value)> + use<> {
        let app = self.app();
        async move {
            let bearer = format!("Bearer {AUTH}");
            let headers = [
                ("authorization", bearer.as_str()),
                ("content-type", "application/json"),
            ];
            let uri = format!("/turns/{CHANNEL}/cancel");
            let body = json!({"force": true}).to_string();
            crate::server::routes::cancel_route_test_post(&app, &uri, &headers, &body).await
        }
    }

    async fn dead_lettered(&self) -> BTreeSet<u64> {
        let dead: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT message_id FROM relay_dead_letter WHERE channel_id = $1 ORDER BY message_id",
        )
        .bind(CHANNEL.to_string())
        .fetch_all(&self.pool)
        .await
        .unwrap();
        dead.into_iter()
            .flatten()
            .map(|id| id.parse().unwrap())
            .collect()
    }

    async fn finish(self) {
        self.gateway.abort();
        self.pool.close().await;
        self.db.drop().await;
    }
}

impl Text {
    /// `!clear` through the text dispatcher: whether it was consumed and the replies it posted.
    async fn text_clear(&self) -> (bool, Vec<String>) {
        let mut message = serenity::Message::default();
        message.id = MessageId::new(CHANNEL + 2);
        message.channel_id = channel();
        message.author.id = UserId::new(1);
        message.author.name = "clear-test".into();
        message.content = "!clear".into();
        self.api.take();
        let handled = Box::pin(super::super::handle_text_command_with_uploads(
            &self.ctx,
            &message,
            &self.data,
            channel(),
            "!clear",
            &[],
            &mut None,
        ))
        .await;
        let posted = format!("POST /api/v10/channels/{CHANNEL}/messages ");
        let replies = self.api.take().into_iter().filter_map(|call| {
            let body: Value = serde_json::from_str(call.strip_prefix(&posted)?).ok()?;
            Some(body["content"].as_str()?.to_owned())
        });
        (handled.expect("the clear ran"), replies.collect())
    }
}

fn ids(queue: &[Intervention]) -> Vec<u64> {
    queue.iter().map(|item| item.message_id.get()).collect()
}

/// The count the clear's notice reports, `None` when it reports none.
fn discarded_count(replies: &[String]) -> Option<usize> {
    let reply = replies
        .iter()
        .find(|reply| reply.contains("대기 메시지 "))?;
    let count = reply.split("대기 메시지 ").nth(1)?.split('건').next()?;
    count.parse().ok()
}

fn http_receipt(status: StatusCode, reply: &Value) -> BTreeSet<u64> {
    let claimed = match status {
        StatusCode::OK => &reply["queue_dead_lettered_message_ids"],
        _ => &reply["context"]["queue_dead_lettered_message_ids"],
    };
    let claimed = claimed.as_array().cloned().unwrap_or_default();
    let id = |id: &Value| id.as_str().and_then(|id| id.parse().ok()).or(id.as_u64());
    claimed.iter().filter_map(id).collect()
}

// A text `!clear` overlapping a forced HTTP cancel: the clear discards what it finds once, the
// cancel records only what its purge removed, and no waiting input is lost or discarded twice.
#[test]
fn force_root_replaced_text_clear_overlap_pg() {
    let name = "force_root_replaced_text_clear_overlap_pg";
    overlap(&format!("{}::{name}", module_path!()), &HTTP_FIRST);
}

// The overlap above with the clear first, in its own child.
#[test]
fn force_root_replaced_text_clear_first_overlap_pg() {
    let name = "force_root_replaced_text_clear_first_overlap_pg";
    overlap(&format!("{}::{name}", module_path!()), &CLEAR_FIRST);
}

fn overlap(path: &str, orders: &[&str]) {
    if !herdr::in_child(path, CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut rows = 0;
        for &order in orders {
            // Each order starts from its own runtime root, so no earlier row's files carry over.
            let _root = crate::config::TestRuntimeRootGuard::new();
            let idle = order == "clear-first";
            let close = |(method, _): &(String, String)| method.ends_with(".close");
            let closed_before = rig.writes().iter().filter(|w| close(w)).count();
            let node = Overlap::bind(&rig, idle).await;
            Overlap::enqueue(&node.shared, 61, "waiting before both").await;
            let cleared = Arc::new(Mutex::new(None));
            let late = order != "clear-first";
            let (status, reply) = if idle {
                // The clear holds the transition at its recovery's swap while the cancel arrives.
                let (request, answered) = (node.cancel(), Arc::new(Mutex::new(None)));
                let probe_answer = answered.clone();
                root_replaced::BEFORE_CAS.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        Box::pin(async move {
                            *probe_answer.lock().unwrap() = Some(request.await);
                        })
                    }))
                });
                *cleared.lock().unwrap() = Some(node.text.text_clear().await);
                let answer = answered.lock().unwrap().take();
                answer.unwrap_or_else(|| panic!("{order}: the cancel ran inside the clear"))
            } else {
                // The cancel holds its exclusion while the clear runs and later input arrives.
                let (text, shared) = (node.text.clone(), node.shared.clone());
                let probe_cleared = cleared.clone();
                let hold = move || {
                    async move {
                        *probe_cleared.lock().unwrap() = Some(text.text_clear().await);
                        Overlap::enqueue(&shared, 62, "after the clear").await;
                    }
                    .boxed()
                };
                match order {
                    "http-first-before-postlude" => {
                        probe_before_postlude(Box::new(move || Box::pin(hold())))
                    }
                    _ => crate::services::queue::set_pre_purge_probe_for_test(
                        channel(),
                        Box::new(hold),
                    ),
                }
                node.cancel().await
            };
            let (handled, replies) = cleared
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| panic!("{order}: the clear ran while the other was held"));
            assert!(handled, "{order}: the command is consumed");
            let notices = replies
                .iter()
                .filter(|reply| reply.contains("대기 메시지 "));
            assert_eq!(notices.count(), 1, "{order}: one clear notice: {replies:?}");

            let waiting: BTreeSet<u64> = [61].into_iter().chain(late.then_some(62)).collect();
            let receipt = http_receipt(status, &reply);
            let recorded = node.dead_lettered().await;
            let queue = ids(&node
                .shared
                .mailbox(channel())
                .snapshot()
                .await
                .intervention_queue);
            let (filed, _) = load_channel_pending_queue_for_tests(
                &ProviderKind::Claude,
                &node.shared.token_hash,
                channel(),
            );
            assert_eq!(
                ids(&filed),
                queue,
                "{order}: the file holds the actor's queue"
            );
            let kept: BTreeSet<u64> = queue.iter().copied().collect();
            let by_clear: BTreeSet<u64> = &(&waiting - &kept) - &receipt;
            assert_eq!(
                recorded, receipt,
                "{order}: the cancel records what it claims: {reply}"
            );
            assert!(
                receipt.is_disjoint(&by_clear),
                "{order}: nothing discarded twice"
            );
            assert_eq!(
                by_clear,
                BTreeSet::from([61]),
                "{order}: the clear took 61: {reply}"
            );
            assert_eq!(discarded_count(&replies), Some(1), "{order}: {replies:?}");
            let closes = rig.writes().iter().filter(|w| close(w)).count() - closed_before;
            // The rig's pane vanishes at its first close, so a later order may find it gone.
            let most = usize::from(order != "clear-first");
            assert!(closes <= most, "{order}: nothing closed twice: {closes}");
            let expected = match order {
                "http-first-before-postlude" => (StatusCode::OK, vec![62], vec![]),
                "http-first-after-capture" => (StatusCode::INTERNAL_SERVER_ERROR, vec![], vec![62]),
                _ => (StatusCode::CONFLICT, vec![], vec![]),
            };
            let receipt: Vec<u64> = receipt.into_iter().collect();
            assert_eq!(
                (status, receipt, queue),
                expected,
                "{order}: {reply} {replies:?}"
            );
            let detail = reply["context"]["termination"]["detail"]
                .as_str()
                .unwrap_or_default();
            if order == "clear-first" {
                assert_eq!(detail, "transition busy", "{order}: {reply}");
                let row = herdr::row(&node.pool, &herdr_key(&node).await).await;
                assert_eq!(
                    row.map(|row| row.0),
                    Some(herdr::HostedState::Retired),
                    "{order}"
                );
            }
            node.finish().await;
            rows += 1;
        }
        assert_eq!(rows, orders.len());
    });
}

async fn herdr_key(node: &Overlap) -> String {
    let key = crate::services::discord::adk_session::build_adk_session_key;
    key(&node.shared, channel(), &ProviderKind::Claude, None)
        .await
        .unwrap()
}
