use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::server::outbox_worker::{drain_message_outbox_once, origin_fixture::Discord};
use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
use crate::services::cluster::stream_relay::{
    GenerationSourceIdentity, RelaySink, RelaySinkOutcome, SourceEpoch, SourceFileIdentity,
    SourceStamp, SourceWitness, StreamFrame,
};
use crate::services::discord::{
    bot_role::UtilityBotRole, inflight, outbound::delivery_record, session_relay_sink,
    shared_state::test_rest,
};
use crate::services::tmux_common::session_temp_path;
use serde_json::json;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const CHANNEL: u64 = 72_280_700;
const FIRST_NONCE: &str = "11111111111111111111111111111111";
const NEXT_NONCE: &str = "22222222222222222222222222222222";
const FIRST_ANSWER: &str = "first wrapper answer";
const NEXT_ANSWER: &str = "later autonomous answer";

struct Fixture {
    mock: Discord,
    sink: session_relay_sink::SessionBoundDiscordRelaySink,
    binding: MatchedChannel,
    source: SourceStamp,
    first: String,
    next: String,
    pool: Arc<sqlx::PgPool>,
    db: DispatchPostgresTestDb,
    _rest: test_rest::Guard,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    async fn new(label: &str, next_answer: &str) -> Self {
        let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let db = DispatchPostgresTestDb::create("adk_wrapper_idle", label).await;
        let pool = Arc::new(db.connect_and_migrate().await);
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            pool.as_ref().clone(),
        ));
        let mock = Discord::new().await;
        let registry = mock.registry().await;
        let rest = test_rest::install(
            registry
                .utility_bot_http_clone(UtilityBotRole::Notify)
                .await
                .expect("mock outbox bot"),
        );
        let provider = ProviderKind::Claude;
        registry
            .register(provider.as_str().into(), shared.clone())
            .await;
        let sink = session_relay_sink::SessionBoundDiscordRelaySink::new(registry.clone());
        let session = provider.build_tmux_session_name(&CHANNEL.to_string());
        let output = expected_rollout_path_for(&session);
        let generation_marker = session_temp_path(&session, "generation");
        fs::create_dir_all(Path::new(&output).parent().unwrap()).unwrap();
        let prelude = terminal_jsonl("");
        let first = format!("{prelude}{}", terminal_jsonl(FIRST_ANSWER));
        let next = terminal_jsonl(next_answer);
        fs::write(&output, format!("{first}{next}")).unwrap();
        fs::write(&generation_marker, b"wrapper fixture generation").unwrap();
        let generation = delivery_record::current_generation_mtime_ns(&session);
        assert_ne!(generation, 0);
        let marker_metadata = fs::metadata(&generation_marker).unwrap();
        let file = SourceFileIdentity::from_open_file(&fs::File::open(&output).unwrap());
        let SourceFileIdentity::Unix { dev, ino } = file else {
            panic!("wrapper fixture needs a real source identity");
        };
        let source = SourceStamp {
            epoch: SourceEpoch::default(),
            file,
            witness: SourceWitness {
                generation: Some(GenerationSourceIdentity::Unix {
                    mtime_ns: generation,
                    dev: marker_metadata.dev(),
                    ino: marker_metadata.ino(),
                }),
                spawn_nonce_hash: None,
            },
        };
        let first_end = first.len() as u64;
        let next_end = first_end + next.len() as u64;
        fs::write(
            session_temp_path(&session, "headless-terminal"),
            json!({
                "generation": generation, "dev": dev, "ino": ino,
                "reads": [
                    {"start": prelude.len(), "end": first_end, "nonce": FIRST_NONCE},
                    {"start": first_end, "end": next_end, "nonce": NEXT_NONCE},
                ],
            })
            .to_string(),
        )
        .unwrap();

        let mut row = test_inflight_state();
        row.channel_id = CHANNEL;
        row.tmux_session_name = Some(session.clone());
        row.output_path = Some(output.clone());
        row.turn_nonce = Some(FIRST_NONCE.to_owned());
        row.delivery_bot = Some("notify".into());
        row.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper);
        row.runtime_kind_unknown_on_disk = false;
        inflight::save_inflight_state(&row).expect("durable first turn");
        let outbox_id = enqueued_id(
            enqueue_headless_turn_row_for_test(&shared, &row, None, FIRST_ANSWER).await,
        );
        assert!(mock.posts().is_empty(), "enqueue is not a Discord POST");
        assert_eq!(
            drain_message_outbox_once(&pool, &registry, "wrapper-fixture").await,
            1
        );
        let sent: (String, String) =
            sqlx::query_as("SELECT status, producer_turn_nonce FROM message_outbox WHERE id = $1")
                .bind(outbox_id)
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
        assert_eq!((sent.0.as_str(), sent.1.as_str()), ("sent", FIRST_NONCE));
        assert_eq!(
            mock.posts().len(),
            1,
            "the real worker posted the first answer"
        );
        assert!(inflight::clear_inflight_state(&provider, CHANNEL));
        assert!(inflight::load_inflight_state_read_only(&provider, CHANNEL).is_none());
        let binding = MatchedChannel {
            channel_id: CHANNEL.to_string(),
            agent_id: "wrapper-idle-fixture".into(),
            provider,
            expected_session_name: session,
            expected_rollout_path: output,
        };
        Self {
            mock,
            sink,
            binding,
            source,
            first,
            next,
            pool,
            db,
            _rest: rest,
            _root: root,
        }
    }

    fn frame(&self, start: u64, payload: String, sequence: u64) -> StreamFrame {
        let Some(GenerationSourceIdentity::Unix { mtime_ns, .. }) = self.source.witness.generation
        else {
            panic!("wrapper fixture needs a real generation");
        };
        let range = Some((start, start + payload.len() as u64));
        StreamFrame {
            session_name: self.binding.expected_session_name.clone(),
            binding: self.binding.clone(),
            payload,
            sequence,
            terminal_consumed_end: None,
            turn_user_msg_id: 0,
            turn_started_at: String::new(),
            turn_start_offset: None,
            relay_range: range,
            relay_generation_mtime_ns: Some(mtime_ns),
            relay_source_stamp: Some(self.source),
            source_span: range,
            read_source: None,
        }
    }

    async fn relay(&self, start: u64, payload: String, sequence: u64) {
        let frame = self.frame(start, payload, sequence);
        assert_eq!(
            self.sink.deliver(&frame).await.unwrap(),
            RelaySinkOutcome::TerminalDelivered
        );
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn terminal_jsonl(answer: &str) -> String {
    let assistant = json!({"type": "assistant", "message": {"content": [
        {"type": "text", "text": answer},
    ]}});
    let result = json!({"type": "result", "subtype": "success", "result": answer});
    format!("{assistant}\n{result}\n")
}

#[tokio::test(flavor = "current_thread")]
async fn sent_wrapper_headless_range_does_not_repeat_but_equal_later_turn_posts_pg() {
    let _policy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new("exact wrapper replay with equal later answer", FIRST_ANSWER).await;

    fixture.relay(0, fixture.first.clone(), 1).await;
    assert_eq!(
        fixture.mock.posts().len(),
        1,
        "rowless replay must not post a second copy"
    );

    fixture
        .relay(fixture.first.len() as u64, fixture.next.clone(), 2)
        .await;
    let posts = fixture.mock.posts();
    assert_eq!(
        posts.len(),
        2,
        "the distinct turn with the same answer must post"
    );
    assert!(
        posts[1].1["content"]
            .as_str()
            .unwrap()
            .contains(FIRST_ANSWER)
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn sent_wrapper_headless_prefix_does_not_consume_coalesced_new_tail_pg() {
    let _policy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = Fixture::new("wrapper replay and new output in one frame", NEXT_ANSWER).await;

    fixture
        .relay(0, format!("{}{}", fixture.first, fixture.next), 1)
        .await;
    let posts = fixture.mock.posts();
    assert_eq!(
        posts.len(),
        2,
        "only the first outbox answer and the new idle tail may post"
    );
    let tail = posts[1].1["content"].as_str().unwrap();
    assert!(
        tail.contains(NEXT_ANSWER) && !tail.contains(FIRST_ANSWER),
        "posted: {tail}"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn sent_wrapper_proof_cannot_consume_replaced_source_with_same_generation_pg() {
    let _policy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut fixture = Fixture::new(
        "wrapper source rotation without marker change",
        FIRST_ANSWER,
    )
    .await;
    let stale = fixture.frame(0, fixture.first.clone(), 1);
    let path = &fixture.binding.expected_rollout_path;
    let replacement = format!("{path}.replacement");
    fs::write(&replacement, &fixture.first).unwrap();
    fs::rename(replacement, path).unwrap();
    let replacement_file = SourceFileIdentity::from_open_file(&fs::File::open(path).unwrap());
    assert_ne!(fixture.source.file, replacement_file);

    assert_eq!(
        fixture.sink.deliver(&stale).await.unwrap(),
        RelaySinkOutcome::TerminalNotDelivered
    );
    assert_eq!(fixture.mock.posts().len(), 1);
    fixture.source.file = replacement_file;
    fixture.relay(0, fixture.first.clone(), 2).await;
    assert_eq!(fixture.mock.posts().len(), 2);
    fixture.close().await;
}
