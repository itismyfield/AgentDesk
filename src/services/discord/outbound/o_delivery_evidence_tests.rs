use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::tui_o::{
    ownership::OwnershipGate,
    shadow::{
        CaptureOutcome, CaptureSource, SourceId, binding_reader::source_id_for,
        capture::SourceCapture,
    },
    store::{InitSource, Initialized, OStore, StoreConfig},
    writer::{
        AlarmSink, DeliveryLease, PostOutcome, SeenMessage, WriterAlarm,
        deliver::ChannelWriter,
        pieces::{Derived, UnitDeriver},
    },
};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub(in crate::services::discord) struct Port {
    pub messages: Mutex<Vec<SeenMessage>>,
    pub reads: AtomicUsize,
    first: u64,
    pub uncertain: AtomicUsize,
    pub on_get: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl DiscordPort for Port {
    fn bot_id(&self) -> u64 {
        42
    }
    fn post(
        &self,
        _: u64,
        content: String,
    ) -> impl std::future::Future<Output = PostOutcome> + Send + 'static {
        let mut messages = self.messages.lock().unwrap();
        let message = SeenMessage {
            id: self.first + ((messages.len() as u64) << 22) + messages.len() as u64,
            author_id: 42,
            content,
        };
        messages.push(message.clone());
        let uncertain = self.uncertain.load(Ordering::SeqCst);
        async move {
            if uncertain == 1 || (uncertain == 2 && message.id % 2 == 1) {
                PostOutcome::Uncertain("timeout".into())
            } else {
                PostOutcome::Created(message)
            }
        }
    }
    async fn history_after(&self, _: u64, _: u64) -> Result<Vec<SeenMessage>, String> {
        if self.uncertain.load(Ordering::SeqCst) != 0 {
            Err("initial history unavailable".into())
        } else {
            Ok(self.messages.lock().unwrap().clone())
        }
    }
    async fn message(&self, _: u64, id: u64) -> Result<Option<SeenMessage>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned())
    }
}
pub(in crate::services::discord) struct Lease;
impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        Some(())
    }
}
pub(in crate::services::discord) struct Alarms;
impl AlarmSink for Alarms {
    fn raise(&self, _: u64, alarm: WriterAlarm) {
        if !matches!(alarm, WriterAlarm::Unresolved { .. }) {
            panic!("writer alarm: {alarm:?}");
        }
    }
}

pub(in crate::services::discord) struct Fixture {
    pub root: std::path::PathBuf,
    pub source: SourceId,
    pub row: InflightTurnState,
    pub body: String,
    pub port: Arc<Port>,
    pub writer: ChannelWriter<Port, Lease, Alarms>,
    pub get_calls: Arc<AtomicUsize>,
    pub sends: Arc<AtomicUsize>,
}

pub(in crate::services::discord) fn snowflake(age_secs: i64) -> u64 {
    ((chrono::Utc::now().timestamp_millis() - age_secs * 1000 - 1_420_070_400_000) as u64) << 22
}

impl Fixture {
    pub async fn new(channel: u64, provider: ProviderKind, bodies: &[&str]) -> Self {
        Self::new_mode(channel, provider, bodies, false).await
    }
    pub async fn new_mode(
        channel: u64,
        provider: ProviderKind,
        bodies: &[&str],
        uncertain: bool,
    ) -> Self {
        Self::new_with_mode(channel, provider, bodies, u8::from(uncertain)).await
    }
    pub async fn new_with_mode(
        channel: u64,
        provider: ProviderKind,
        bodies: &[&str],
        uncertain: u8,
    ) -> Self {
        Self::new_with_terminal(channel, provider, bodies, uncertain, None).await
    }
    async fn new_with_terminal(
        channel: u64,
        provider: ProviderKind,
        bodies: &[&str],
        uncertain: u8,
        ending: Option<serde_json::Value>,
    ) -> Self {
        Self::new_scoped(channel, provider, bodies, uncertain, ending, None).await
    }

    pub async fn injected_with_prefix(channel: u64, bodies: &[&str], prefix: usize) -> Self {
        Self::new_scoped(channel, ProviderKind::Claude, bodies, 0, None, Some(prefix)).await
    }

    async fn new_scoped(
        channel: u64,
        provider: ProviderKind,
        bodies: &[&str],
        uncertain: u8,
        ending: Option<serde_json::Value>,
        prefix: Option<usize>,
    ) -> Self {
        let root = crate::config::runtime_root().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("native-{channel}.jsonl"));
        let mut values = Vec::new();
        match provider {
            ProviderKind::Claude => values.push(if prefix.is_some() {
                json!({"type":"user","uuid":"input","sessionId":"native-session","isMeta":true,"message":{"content":"<task-notification>background task completed</task-notification>"}})
            } else {
                json!({"type":"user","uuid":"input","sessionId":"native-session","message":{"content":"input"}})
            }),
            ProviderKind::Codex => { values.push(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}));
                values.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"input"}]}})); },
            _ => panic!("native fixture provider"),
        }
        let mut body = String::new();
        for (index, text) in bodies.iter().enumerate() {
            if !body.is_empty()
                && crate::services::discord::semantic_boundaries::semantic_chunk_separator_needed(
                    &body, text,
                )
            {
                body.push_str("\n\n");
            }
            body.push_str(text);
            values.push(match provider {
                ProviderKind::Claude => json!({"type":"assistant","uuid":format!("a-{index}"),"apiBlockIndex":0,"message":{"id":format!("body-{index}"),"content":[{"type":"text","text":text}]}}),
                _=>json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":format!("body-{index}"),"content":[{"type":"output_text","text":text}]}}),
            });
        }
        values.push(ending.unwrap_or_else(|| match provider {
            ProviderKind::Claude => json!({"type":"system","subtype":"stop_hook_summary"}),
            _ => json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}),
        }));
        let mut raw = vec![b' '; prefix.unwrap_or(0)];
        if let Some(last) = raw.last_mut() {
            *last = b'\n';
        }
        let delivery_start = raw.len() as u64;
        let initial_hash = manifest::hash(&raw);
        raw.extend(values.iter().flat_map(|v| format!("{v}\n").into_bytes()));
        std::fs::write(&path, &raw).unwrap();
        let source = source_id_for("native-session", &path).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        store
            .init_channel(&Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: source.clone(),
                    delivery_start,
                    prefix_hash: initial_hash.clone(),
                }],
                initial_anchor: snowflake(500),
                build_digest: "fixture".into(),
                at: chrono::Utc::now(),
            })
            .unwrap();
        store
            .begin_era(&[channel], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![InitSource {
                        source_id: source.clone(),
                        delivery_start,
                        prefix_hash: initial_hash.clone(),
                    }],
                    initial_anchor: snowflake(500),
                    build_digest: "fixture".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let mut channel_store = store.open_channel(&era, channel).unwrap().unwrap();
        let mut capture = SourceCapture::open(source.clone(), delivery_start).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1024 * 1024) else {
            panic!("capture")
        };
        channel_store
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        let mut deriver = UnitDeriver::new(channel, manifest::provider(&provider).unwrap());
        let work: Vec<Derived> = batch
            .records
            .iter()
            .flat_map(|record| deriver.derive(record))
            .collect();
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let port = Arc::new(Port {
            messages: Mutex::new(Vec::new()),
            reads: AtomicUsize::new(0),
            first: snowflake(390),
            uncertain: AtomicUsize::new(usize::from(uncertain)),
            on_get: Mutex::new(None),
        });
        let mut writer = ChannelWriter::new(channel_store, gate, port.clone(), Lease, Alarms);
        for piece in work {
            assert_eq!(
                writer.deliver(&piece).await,
                crate::services::tui_o::writer::deliver::Step::Done
            );
        }
        let kind = if provider == ProviderKind::Claude {
            RuntimeHandoffKind::ClaudeTui
        } else {
            RuntimeHandoffKind::CodexTui
        };
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            7,
            snowflake(400),
            snowflake(395),
            "input".into(),
            Some("native-session".into()),
            Some(format!("test-{channel}")),
            Some(path.display().to_string()),
            None,
            0,
        );
        row.runtime_kind = Some(kind);
        row.turn_start_offset = Some(delivery_start);
        row.last_offset = raw.len() as u64;
        row.full_response = body.clone();
        row.turn_nonce = Some(format!("nonce-{channel}"));
        row.tui_terminal_source_file_identity = Some((source.dev, source.ino));
        Self {
            root,
            source,
            row,
            body,
            port,
            writer,
            get_calls: Arc::new(AtomicUsize::new(0)),
            sends: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn reopen_after_post_crash(&mut self) {
        let path = self
            .root
            .join("o_store")
            .join(self.row.channel_id.to_string())
            .join("ledger.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let cut = bytes[..bytes.len() - 1]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .unwrap()
            + 1;
        std::fs::write(&path, &bytes[..cut]).unwrap();
        let store = OStore::existing(&self.root).unwrap();
        let channel = store
            .open_channel(&store.read_era().unwrap().unwrap(), self.row.channel_id)
            .unwrap()
            .unwrap();
        assert!(channel.ledger().unresolved().is_some());
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        self.writer = ChannelWriter::new(channel, gate, self.port.clone(), Lease, Alarms);
    }
    fn plan(&self) -> Manifest {
        manifest::build(
            &self.root,
            manifest::Query {
                provider: &self.row.provider_kind().unwrap(),
                channel: self.row.channel_id,
                path: &self.source.path,
                file: (self.source.dev, self.source.ino),
                range: (0, self.row.last_offset),
            },
            Instant::now() + std::time::Duration::from_secs(10),
        )
        .unwrap()
    }
    pub async fn http(
        &self,
    ) -> (
        Arc<serenity::Http>,
        crate::services::discord::shared_state::test_rest::Guard,
    ) {
        use axum::{
            Json, Router,
            http::{Method, StatusCode, Uri},
            response::IntoResponse,
        };
        let port = self.port.clone();
        let channel = self.row.channel_id;
        let calls = self.get_calls.clone();
        let sends = self.sends.clone();
        let app=Router::new().fallback(move|method:Method,uri:Uri|{
            let port=port.clone();let calls=calls.clone();let sends=sends.clone();async move {
                if method==Method::POST||method==Method::PATCH{sends.fetch_add(1,Ordering::SeqCst);}
                if uri.path().ends_with("/users/@me") { return Json(json!({"id":"42","username":"bot","discriminator":"0001","avatar":null,"bot":true})).into_response(); }
                if method==Method::DELETE{return StatusCode::NO_CONTENT.into_response();}
                if !uri.path().contains("/messages") {return Json(json!({"id":channel.to_string(),"type":0,"guild_id":"1","name":"adk-cc","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response();}
                let id=uri.path().rsplit('/').next().unwrap().parse::<u64>().unwrap_or(snowflake(10));
                let msg=port.messages.lock().unwrap().iter().find(|m|m.id==id).cloned();
                if method==Method::GET{if let Some(hook)=port.on_get.lock().unwrap().take(){hook();} calls.fetch_add(1,Ordering::SeqCst);if msg.is_none(){return StatusCode::NOT_FOUND.into_response();}}
                let content=msg.map(|m|m.content).unwrap_or_default();
                Json(json!({"id":id.to_string(),"channel_id":channel.to_string(),"content":content,"author":{"id":"42","username":"bot","discriminator":"0001","avatar":null,"bot":true},"timestamp":"2026-10-27T00:00:00Z","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0})).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("fixture-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let guard = crate::services::discord::shared_state::test_rest::install(http.clone());
        (http, guard)
    }
}

#[tokio::test]
async fn i7441b_local_native_receipts_require_whole_episode_and_exact_payload() {
    for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let f = Fixture::new(744101 + index as u64, provider, &["prefix. ", "tail"]).await;
        let plan = f.plan();
        assert!(
            episode(
                &f.root,
                None,
                &*f.port,
                &plan,
                Instant::now() + std::time::Duration::from_secs(10)
            )
            .await
            .is_some()
        );
        let proof = episode(
            &f.root,
            None,
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        let latest = f
            .port
            .messages
            .lock()
            .unwrap()
            .iter()
            .map(|m| MessageId::new(m.id).created_at().timestamp_millis() as u64)
            .max()
            .unwrap();
        assert_eq!(
            proof.posted_ms, latest,
            "the last covered piece sets the delivery time"
        );
        f.port.messages.lock().unwrap()[0].content = "transformed".into();
        assert!(
            episode(
                &f.root,
                None,
                &*f.port,
                &plan,
                Instant::now() + std::time::Duration::from_secs(10)
            )
            .await
            .is_none()
        );
        assert!(
            verify(
                None,
                &*f.port,
                &plan,
                plan.body.len() - 4,
                Instant::now() + std::time::Duration::from_secs(10),
                delivery_evidence::local(&f.root, f.row.channel_id).as_ref()
            )
            .await
            .is_some()
        );
    }
}

#[tokio::test]
async fn i7441b_source_prefix_and_deadline_budgets_fail_closed_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let mut f = Fixture::new(744103, ProviderKind::Claude, &["small body"]).await;
    manifest::SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
    let _plan = f.plan();
    assert_eq!(
        manifest::SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        f.row.last_offset as usize
    );
    let expired = manifest::build(
        &f.root,
        manifest::Query {
            provider: &ProviderKind::Claude,
            channel: f.row.channel_id,
            path: &f.source.path,
            file: (f.source.dev, f.source.ino),
            range: (0, f.row.last_offset),
        },
        Instant::now(),
    );
    assert!(expired.is_none());
    assert!(
        manifest::build_terminal(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino),
                range: (0, f.row.last_offset),
            },
            Instant::now(),
        )
        .is_none()
    );
    let raw = std::fs::read(&f.source.path).unwrap();
    let prefix = format!(
        "{}\n",
        json!({"type":"metadata","padding":"x".repeat(64*1024*1024 + 1)})
    );
    let joined = [prefix.as_bytes(), raw.as_slice()].concat();
    f.row.turn_start_offset = Some(prefix.len() as u64);
    f.row.last_offset = joined.len() as u64;
    std::fs::write(&f.source.path, &joined).unwrap();
    let cursor_path = f
        .root
        .join("o_store")
        .join(f.row.channel_id.to_string())
        .join("cursor")
        .join(format!(
            "{}.json",
            crate::services::tui_o::store::spool::source_key(&f.source)
        ));
    let mut cursor: crate::services::tui_o::store::spool::Cursor =
        serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    cursor.captured_through = joined.len() as u64;
    cursor.prefix_hash = manifest::hash(&joined);
    std::fs::write(cursor_path, serde_json::to_vec(&cursor).unwrap()).unwrap();
    manifest::SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
    assert!(
        manifest::build(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino),
                range: (prefix.len() as u64, joined.len() as u64)
            },
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .is_none()
    );
    assert_eq!(
        manifest::SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        64 * 1024 * 1024
    );
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441b_prefix",
        "bounded prefix orphan",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        RuntimeHandoffKind::ClaudeTui,
    )]);
    crate::services::discord::tmux::tmux_watcher::o_evidence_tests::assert_receipted_fixture_suppressed(&f,&pool).await;
    super::super::completed_turn_ledger::o_pending::capture(&ProviderKind::Claude, &f.row).await;
    let pending = f
        .root
        .join("runtime/discord_pending_o_deliveries/claude")
        .join(format!("{}.json", f.row.channel_id));
    let before = std::fs::read(&pending).unwrap();
    let (_http, _http_guard) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    super::super::completed_turn_ledger::o_pending::project(
        &shared,
        &ProviderKind::Claude,
        f.row.channel_id,
    )
    .await;
    assert_eq!(std::fs::read(pending).unwrap(), before);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn i7441b_pg_only_runner_receipt_is_read_without_repairing_local_ledger_pg() {
    use crate::services::tui_o::repost::{
        identity::{IDENTITY_VERSION, SPLIT_VERSION, payload_sha256},
        io::probe::{
            matcher::ObservedMessage,
            tests::{Fake, run},
        },
        o_piece_attempts::AttemptResult,
        o_piece_delivery::{self as db, AdmittedBy, NewDelivery},
        runner::Runner,
    };
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    // SQL establishes real clocks; pause only the writer's history wait.
    tokio::time::resume();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441b_receipt",
        "O PG-only evidence",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    tokio::time::pause();
    let f = Fixture::new_mode(744104, ProviderKind::Claude, &["PG-only body"], true).await;
    tokio::time::resume();
    let plan = f.plan();
    let piece = &plan.pieces[0];
    let key = PieceKey::new(piece.key.clone(), piece.index).unwrap();
    let local_path = f
        .root
        .join("o_store")
        .join(f.row.channel_id.to_string())
        .join("ledger.jsonl");
    let before = std::fs::read(&local_path).unwrap();
    let local = delivery_evidence::local(&f.root, f.row.channel_id).unwrap();
    assert!(matches!(
        local
            .latest_piece(&piece.key, piece.index)
            .unwrap()
            .1
            .outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    db::admit(
        &pool,
        &NewDelivery {
            key: key.clone(),
            payload: piece.payload.clone(),
            payload_sha256: payload_sha256(&piece.payload),
            identity_version: IDENTITY_VERSION,
            split_version: SPLIT_VERSION,
            original_anchor: snowflake(500),
            sender_id: 42,
            admitted_by: AdmittedBy::Uncertain,
            origin_serial: 0,
            origin_node: "fixture".into(),
            original: AttemptResult::Uncertain,
            prior_retries: 0,
            failure: None,
        },
    )
    .await
    .unwrap();
    let message = f.port.messages.lock().unwrap()[0].clone();
    let observed = |id, content: String| ObservedMessage {
        id,
        channel_id: f.row.channel_id,
        author_id: 42,
        content,
        rich_embeds: 0,
        footers: Vec::new(),
        nonce: None,
    };
    let proof = snowflake(380);
    let reader = Fake::with([
        observed(proof, "proof".into()),
        observed(message.id, message.content),
    ]);
    let run = run("i7441b");
    let mut runner = Runner::new(pool.clone(), 42, run);
    runner
        .tick(
            f.row.channel_id,
            &reader,
            &[proof],
            Instant::now() - std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_piece_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(std::fs::read(&local_path).unwrap(), before);
    assert!(
        episode(
            &f.root,
            Some(&pool),
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .await
        .is_some()
    );
    crate::services::discord::tmux::tmux_watcher::o_evidence_tests::assert_receipted_fixture_suppressed(&f,&pool).await;
    for variant in 0..5 {
        let mut wrong = plan.clone();
        match variant {
            0 => wrong.pieces[0].key.channel_id += 1,
            1 => {
                wrong.pieces[0].key.provider = crate::services::tui_o::shadow::ShadowProvider::Codex
            }
            2 => wrong.pieces[0].key.native_key = "different native unit".into(),
            3 => wrong.pieces[0].key.kind = crate::services::tui_o::shadow::UnitKind::ToolResult,
            _ => wrong.pieces[0].index += 1,
        }
        assert!(
            episode(
                &f.root,
                Some(&pool),
                &*f.port,
                &wrong,
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .await
            .is_none(),
            "key field {variant}"
        );
    }
    for (table, bad, good) in [
        (
            "o_piece_delivery",
            "identity_version=99",
            format!("identity_version={IDENTITY_VERSION}"),
        ),
        ("o_piece_delivery", "sender_id=43", "sender_id=42".into()),
        ("o_piece_receipts", "author_id=43", "author_id=42".into()),
        (
            "o_piece_delivery",
            "payload_sha256='0000000000000000000000000000000000000000000000000000000000000000'",
            format!("payload_sha256='{}'", payload_sha256(&piece.payload)),
        ),
        (
            "o_piece_delivery",
            "payload='wrong'",
            format!("payload='{}'", piece.payload.replace('\'', "''")),
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET {bad}")))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            episode(
                &f.root,
                Some(&pool),
                &*f.port,
                &plan,
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .await
            .is_none(),
            "{bad}"
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET {good}")))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE o_piece_delivery SET split_version=99")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        episode(
            &f.root,
            Some(&pool),
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .await
        .is_none()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn i7441b_multichunk_tail_proof_does_not_require_consumed_prefix() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let body = format!("```rust\n{}\n```", "let 한글 = 1;\n".repeat(500));
    let f = Fixture::new(744107, ProviderKind::Claude, &[&body]).await;
    let plan = f.plan();
    assert!(plan.pieces.len() > 2);
    let sent = plan.pieces[0].body_end;
    f.port.messages.lock().unwrap()[0].content = "unproven prefix".into();
    assert!(
        episode(
            &f.root,
            None,
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .await
        .is_none()
    );
    assert!(
        verify(
            None,
            &*f.port,
            &plan,
            sent,
            Instant::now() + std::time::Duration::from_secs(10),
            delivery_evidence::local(&f.root, f.row.channel_id).as_ref()
        )
        .await
        .is_some()
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn i7441b_mixed_local_and_pg_receipts_cover_all_pieces_pg() {
    use crate::services::tui_o::repost::{
        identity::{IDENTITY_VERSION, SPLIT_VERSION, payload_sha256},
        io::probe::{
            matcher::ObservedMessage,
            tests::{Fake, run},
        },
        o_piece_attempts::AttemptResult,
        o_piece_delivery::{self as db, AdmittedBy, NewDelivery},
        runner::Runner,
    };
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::time::resume();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441b_mixed",
        "mixed O receipts",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    tokio::time::pause();
    let f = Fixture::new_with_mode(
        744109,
        ProviderKind::Claude,
        &["local prefix. ", "PG tail"],
        2,
    )
    .await;
    tokio::time::resume();
    let plan = f.plan();
    let piece = &plan.pieces[1];
    let key = PieceKey::new(piece.key.clone(), piece.index).unwrap();
    assert!(matches!(
        delivery_evidence::local(&f.root, f.row.channel_id)
            .unwrap()
            .latest_piece(&piece.key, piece.index)
            .unwrap()
            .1
            .outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    db::admit(
        &pool,
        &NewDelivery {
            key,
            payload: piece.payload.clone(),
            payload_sha256: payload_sha256(&piece.payload),
            identity_version: IDENTITY_VERSION,
            split_version: SPLIT_VERSION,
            original_anchor: snowflake(500),
            sender_id: 42,
            admitted_by: AdmittedBy::Uncertain,
            origin_serial: 1,
            origin_node: "fixture".into(),
            original: AttemptResult::Uncertain,
            prior_retries: 0,
            failure: None,
        },
    )
    .await
    .unwrap();
    let proof = snowflake(380);
    let observed = |id, content: String| ObservedMessage {
        id,
        channel_id: f.row.channel_id,
        author_id: 42,
        content,
        rich_embeds: 0,
        footers: Vec::new(),
        nonce: None,
    };
    let message = f.port.messages.lock().unwrap()[1].clone();
    let reader = Fake::with([
        observed(proof, "proof".into()),
        observed(message.id, message.content),
    ]);
    let mut runner = Runner::new(pool.clone(), 42, run("mixed"));
    runner
        .tick(
            f.row.channel_id,
            &reader,
            &[proof],
            Instant::now() - std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(
        episode(
            &f.root,
            Some(&pool),
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .await
        .is_some()
    );
    crate::services::discord::tmux::tmux_watcher::o_evidence_tests::assert_receipted_fixture_suppressed(&f,&pool).await;
    sqlx::query("DELETE FROM o_piece_receipts")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        episode(
            &f.root,
            Some(&pool),
            &*f.port,
            &plan,
            Instant::now() + std::time::Duration::from_secs(10)
        )
        .await
        .is_none()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn i7441b_read_only_ledger_torn_tail_wrong_sources_and_old_versions() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = Fixture::new(744184, ProviderKind::Claude, &["same body"]).await;
    let plan = f.plan();
    let ledger = f
        .root
        .join("o_store")
        .join(f.row.channel_id.to_string())
        .join("ledger.jsonl");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&ledger)
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    let bytes = std::fs::read(&ledger).unwrap();
    let mtime = std::fs::metadata(&ledger).unwrap().modified().unwrap();
    let _ = delivery_evidence::local(&f.root, f.row.channel_id);
    assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&ledger).unwrap().modified().unwrap(),
        mtime
    );
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    assert!(
        manifest::build(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino + 1),
                range: (0, f.row.last_offset)
            },
            deadline
        )
        .is_none()
    );
    let mut old = plan.clone();
    old.version += 1;
    assert!(
        episode(&f.root, None, &*f.port, &old, deadline)
            .await
            .is_none()
    );
    let mut interrupted = plan.clone();
    interrupted.completed = false;
    assert!(
        episode(&f.root, None, &*f.port, &interrupted, deadline)
            .await
            .is_none()
    );
    let mut empty = plan.clone();
    empty.pieces.clear();
    assert!(
        verify(
            None,
            &*f.port,
            &empty,
            0,
            deadline,
            delivery_evidence::local(&f.root, f.row.channel_id).as_ref()
        )
        .await
        .is_none()
    );
    assert!(
        verify(
            None,
            &*f.port,
            &plan,
            plan.body.len(),
            deadline,
            delivery_evidence::local(&f.root, f.row.channel_id).as_ref()
        )
        .await
        .is_none()
    );
    assert!(
        manifest::build(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino),
                range: (0, f.row.last_offset)
            },
            Instant::now()
        )
        .is_none()
    );
}

#[tokio::test]
async fn i7441b_native_piece_budget_is_not_success() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let bodies = vec!["chunk"; 257];
    let f = Fixture::new(744190, ProviderKind::Claude, &bodies).await;
    assert!(
        manifest::build(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino),
                range: (0, f.row.last_offset)
            },
            Instant::now() + std::time::Duration::from_secs(2)
        )
        .is_none()
    );
}

#[tokio::test]
async fn i7441b_aborted_or_changed_native_episode_cannot_complete_an_input() {
    for (index, kind) in ["turn_aborted", "task_started"].into_iter().enumerate() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let f = Fixture::new_with_terminal(
            744191 + index as u64,
            ProviderKind::Codex,
            &["partial"],
            0,
            Some(json!({"type":"event_msg","payload":{"type":kind,"turn_id":"successor"}})),
        )
        .await;
        assert!(
            manifest::build(
                &f.root,
                manifest::Query {
                    provider: &ProviderKind::Codex,
                    channel: f.row.channel_id,
                    path: &f.source.path,
                    file: (f.source.dev, f.source.ino),
                    range: (0, f.row.last_offset)
                },
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .is_none()
        );
    }
}

#[tokio::test]
async fn i7441b_prefix_suffix_budget_boundary_is_exact() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = Fixture::new(744193, ProviderKind::Claude, &["body"]).await;
    let raw = std::fs::read(&f.source.path).unwrap();
    let e_start = raw[..raw.len() - 1]
        .iter()
        .rposition(|b| *b == b'\n')
        .unwrap()
        + 1;
    let cursor_path = f
        .root
        .join("o_store")
        .join(f.row.channel_id.to_string())
        .join("cursor")
        .join(format!(
            "{}.json",
            crate::services::tui_o::store::spool::source_key(&f.source)
        ));
    let mut cursor: crate::services::tui_o::store::spool::Cursor =
        serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    for extra in [-1_i64, 1] {
        let padding_len = (64_i64 * 1024 * 1024 + extra) as usize - e_start;
        let mut bytes = vec![b' '; padding_len];
        bytes[padding_len - 1] = b'\n';
        bytes.extend_from_slice(&raw);
        std::fs::write(&f.source.path, &bytes).unwrap();
        cursor.captured_through = (padding_len + e_start) as u64;
        cursor.prefix_hash = manifest::hash(&bytes[..cursor.captured_through as usize]);
        std::fs::write(&cursor_path, serde_json::to_vec(&cursor).unwrap()).unwrap();
        manifest::SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let plan = manifest::build(
            &f.root,
            manifest::Query {
                provider: &ProviderKind::Claude,
                channel: f.row.channel_id,
                path: &f.source.path,
                file: (f.source.dev, f.source.ino),
                range: (padding_len as u64, bytes.len() as u64),
            },
            Instant::now() + std::time::Duration::from_secs(10),
        );
        assert_eq!(
            plan.is_some(),
            extra == -1,
            "prefix cap is independent of the requested episode range"
        );
        if let Some(plan) = &plan {
            assert!(
                super::episode(
                    &f.root,
                    None,
                    &*f.port,
                    plan,
                    Instant::now() + std::time::Duration::from_secs(10)
                )
                .await
                .is_some(),
                "bounded prefix must reach whole-episode coverage"
            );
        }
        assert_eq!(
            manifest::SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            if extra == -1 {
                bytes.len()
            } else {
                64 * 1024 * 1024
            }
        );
        manifest::SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(
            manifest::build(
                &f.root,
                manifest::Query {
                    provider: &ProviderKind::Claude,
                    channel: f.row.channel_id,
                    path: &f.source.path,
                    file: (f.source.dev, f.source.ino),
                    range: (0, bytes.len() as u64),
                },
                Instant::now() + std::time::Duration::from_secs(10)
            )
            .is_none()
        );
        assert_eq!(
            manifest::SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}

#[tokio::test]
async fn i7441b_local_snapshot_reuses_mtime_and_refreshes_changed_ledger() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = Fixture::new(744203, ProviderKind::Claude, &["body"]).await;
    let first = super::local_snapshot(f.root.clone(), f.row.channel_id, None)
        .await
        .unwrap();
    let first_state = first.1.clone().unwrap();
    let cached = super::local_snapshot(f.root.clone(), f.row.channel_id, Some(first))
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &first_state,
        cached.1.as_ref().unwrap()
    ));
    let ledger = f
        .root
        .join("o_store")
        .join(f.row.channel_id.to_string())
        .join("ledger.jsonl");
    let original = std::fs::read(&ledger).unwrap();
    std::fs::remove_file(&ledger).unwrap();
    let missing = super::local_snapshot(f.root.clone(), f.row.channel_id, Some(cached))
        .await
        .unwrap();
    assert!(missing.1.is_none());
    std::fs::write(&ledger, original).unwrap();
    let refreshed = super::local_snapshot(f.root.clone(), f.row.channel_id, Some(missing))
        .await
        .unwrap();
    assert!(refreshed.1.is_some());
    assert!(!std::sync::Arc::ptr_eq(
        &first_state,
        refreshed.1.as_ref().unwrap()
    ));
}
