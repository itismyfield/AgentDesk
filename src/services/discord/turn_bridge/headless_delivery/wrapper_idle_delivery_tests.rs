use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::server::outbox_worker::{drain_message_outbox_once, origin_fixture::Discord};
use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
use crate::services::cluster::stream_relay::{
    GenerationSourceIdentity, RelaySink, RelaySinkOutcome, SourceEpoch, SourceFileIdentity,
    SourceStamp, SourceWitness, StreamFrame,
};
use crate::services::discord::session_relay_sink::tests::idle_relay_harness::ObservedScanner;
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
    sink: Arc<session_relay_sink::SessionBoundDiscordRelaySink>,
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
        Self::with_output(
            label,
            &terminal_jsonl(""),
            &terminal_jsonl(FIRST_ANSWER),
            &terminal_jsonl(next_answer),
        )
        .await
    }

    async fn with_output(label: &str, prelude: &str, first_records: &str, next: &str) -> Self {
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
        let sink = Arc::new(session_relay_sink::SessionBoundDiscordRelaySink::new(
            registry.clone(),
        ));
        let session = provider.build_tmux_session_name(&CHANNEL.to_string());
        let output = expected_rollout_path_for(&session);
        let generation_marker = session_temp_path(&session, "generation");
        fs::create_dir_all(Path::new(&output).parent().unwrap()).unwrap();
        let first = format!("{prelude}{first_records}");
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
            next: next.to_owned(),
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

    async fn scanner(&self) -> ObservedScanner {
        let path = &self.binding.expected_rollout_path;
        fs::write(path, b"").unwrap();
        let mut scanner = ObservedScanner::start(self.binding.clone(), self.sink.clone());
        // Coordinate zero is the previous terminal end. Keep the inode from the sent proof.
        scanner.at_cursor(0).await;
        fs::write(path, format!("{}{}", self.first, self.next)).unwrap();
        scanner
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

const SENTINEL: &str =
    "{\"type\":\"ready_for_input\",\"provider\":\"claude\",\"ts\":\"2026-11-26T11:23:59Z\"}\n";
const A_END: u64 = 706_908;
const B_END: u64 = 1_312_781;

fn sized_turn(output_size: usize, tool_sizes: &[usize], answer: &str) -> String {
    let terminal = terminal_jsonl(answer);
    let rest = output_size - tool_sizes.iter().sum::<usize>() - terminal.len();
    let mut output = String::with_capacity(output_size);
    for size in tool_sizes.iter().copied().chain([rest]) {
        let start = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_fixture\",\"content\":\"";
        let end = "\"}]}}\n";
        output.push_str(start);
        output.push_str(&"x".repeat(size - start.len() - end.len()));
        output.push_str(end);
    }
    output.push_str(&terminal);
    assert_eq!(output.len(), output_size);
    output
}

#[tokio::test(flavor = "current_thread")]
async fn sent_wrapper_past_sentinel_moves_the_idle_window_to_the_equal_later_answer_pg() {
    let _policy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let first = sized_turn(706_833, &[176_000, 176_000, 176_000], FIRST_ANSWER);
    let next = sized_turn(605_873, &[166_579, 166_579], FIRST_ANSWER);
    let fixture =
        Fixture::with_output("cursor through sent wrapper proof", SENTINEL, &first, &next).await;
    assert_eq!(SENTINEL.len(), 75);
    assert_eq!(fixture.first.len() as u64, A_END);
    let all = format!("{}{}", fixture.first, fixture.next);
    assert_eq!(all.len() as u64, B_END);
    assert_eq!(all[..1_048_576].rfind('\n').unwrap() + 1, 1_040_066);
    let mut scanner = fixture.scanner().await;
    let mut ranges = Vec::new();
    for _ in 0..4 {
        let frame = scanner.frame().await;
        let delivers_later_answer = frame.relay_range == Some((A_END, B_END));
        assert_eq!(
            fixture.sink.deliver(&frame).await.unwrap(),
            RelaySinkOutcome::TerminalDelivered
        );
        ranges.push(frame.relay_range);
        assert_eq!(
            fixture.mock.posts().len(),
            if delivers_later_answer { 2 } else { 1 },
            "only the window over the later occurrence may add a POST"
        );
        if delivers_later_answer {
            break;
        }
    }
    assert!(
        ranges.contains(&Some((A_END, B_END))),
        "the idle read window stalled: {ranges:?}"
    );
    scanner.at_cursor(B_END).await;
    let posts = fixture.mock.posts();
    assert_eq!(
        posts.len(),
        2,
        "the worker's first answer and the separate idle answer post once each"
    );
    assert!(
        posts
            .iter()
            .all(|(_, body)| body["content"].as_str().unwrap().contains(FIRST_ANSWER))
    );
    drop(scanner);
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn idle_scanner_keeps_an_unposted_visible_prefix_before_the_sent_wrapper_proof_pg() {
    let _policy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let prefix_answer = "earlier visible response";
    let prefix = terminal_jsonl(prefix_answer);
    let first = sized_turn(706_833, &[176_000, 176_000, 176_000], FIRST_ANSWER);
    let next = sized_turn(605_873, &[166_579, 166_579], NEXT_ANSWER);
    let fixture = Fixture::with_output(
        "unposted visible prefix before sent proof",
        &prefix,
        &first,
        &next,
    )
    .await;
    let mut scanner = fixture.scanner().await;
    let initial = scanner.frame().await;
    let retry = scanner.frame().await;
    for frame in [&initial, &retry] {
        assert_eq!(frame.relay_range.map(|(start, _)| start), Some(0));
        assert!(frame.payload.starts_with(&prefix));
    }
    assert_eq!(
        fixture.mock.posts().len(),
        1,
        "only the worker's sent answer is visible so far"
    );
    assert_eq!(
        fixture.sink.deliver(&initial).await.unwrap(),
        RelaySinkOutcome::TerminalDelivered
    );
    let posts = fixture.mock.posts();
    assert_eq!(posts.len(), 2);
    assert!(
        posts[1].1["content"]
            .as_str()
            .unwrap()
            .contains(prefix_answer)
    );
    let a_end = fixture.first.len() as u64;
    let b_end = a_end + fixture.next.len() as u64;
    scanner.at_cursor(a_end).await;
    let mut delivered_tail = false;
    for _ in 0..4 {
        let frame = scanner.frame().await;
        if frame.relay_range == Some((a_end, b_end)) {
            assert_eq!(
                fixture.sink.deliver(&frame).await.unwrap(),
                RelaySinkOutcome::TerminalDelivered
            );
            delivered_tail = true;
            break;
        }
    }
    assert!(
        delivered_tail,
        "the committed visible prefix must release the later window"
    );
    scanner.at_cursor(b_end).await;
    let posts = fixture.mock.posts();
    assert_eq!(posts.len(), 3);
    assert!(
        posts[2].1["content"]
            .as_str()
            .unwrap()
            .contains(NEXT_ANSWER)
    );
    drop(scanner);
    fixture.close().await;
}
