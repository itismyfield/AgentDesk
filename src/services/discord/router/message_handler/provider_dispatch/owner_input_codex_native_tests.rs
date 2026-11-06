//! A Codex start meets its pane's rollout: while the latest turn shows no end of its own (another
//! turn's end, an unnamed one, no start at all, a torn line) it waits unclaimed or, once claimed,
//! unsent; that turn's own complete or abort runs it once.
use super::*;

/// Stamps the markers and incarnation a spawned Codex pane `name` would carry; its session id.
pub(in super::super) fn execution_proof(name: &str, channel: u64) -> String {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    let proof = tc::session_temp_path(name, "proof");
    fs::create_dir_all(std::path::Path::new(&proof).parent().unwrap()).unwrap();
    tc::write_tmux_owner_marker(name).unwrap();
    tc::host_marker::record_tmux_host_marker(name);
    let kind = crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
    tc::write_tmux_runtime_kind_marker(name, kind).unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let prepared =
        PreparedIncarnation::prepare("codex", name, Some(channel), Some(&sid), false).unwrap();
    let stamped = discord::stamp_spawn_markers(name, Some(&prepared));
    prepared.finish_spawn(stamped).unwrap();
    sid
}

/// The rollout's first record: its session and the workspace its pane runs in.
pub(in super::super) fn meta(sid: &str, workspace: &Path) -> String {
    let cwd = fs::canonicalize(workspace).unwrap();
    row(json!({"type":"session_meta","payload":{"id":sid,"cwd":cwd,"source":"cli"}}))
}

/// A fresh routine's start as the routine executor keeps it.
pub(in super::super) fn routine_context(origin: Option<&str>) -> Value {
    json!({"origin":"routine","source":"routine","is_dm":false,
        "metadata":{"human_input":{"origin_id":origin},"routine_id":"routine-7772"}})
}

fn row(value: Value) -> String {
    format!("{value}\n")
}

/// A Codex turn's opening records through its prompt.
fn opening(turn: &str, text: &str) -> String {
    row(
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn,
        "root_turn_id":turn}}),
    ) + &row(json!({"type":"turn_context","payload":{"turn_id":turn}}))
        + &row(
            json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":text}]}}),
        )
}

/// A Codex turn's answer and its `task_complete`.
fn answer_for(turn: &str, text: &str) -> String {
    row(
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text}]}}),
    ) + &row(
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,
            "last_agent_message":text}}),
    )
}

fn complete(turn: Option<&str>) -> String {
    row(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}}))
}

fn abort(turn: Option<&str>) -> String {
    row(
        json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":turn,
        "reason":"interrupted"}}),
    )
}

/// A start line Codex has not finished writing, and the rest it writes next.
const TORN_START: (&str, &str) = (
    "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_sta",
    "rted\",\"turn_id\":\"t1\",\"root_turn_id\":\"t1\"}}\n",
);

/// What the start's pane's rollout shows when the start meets it, and how it goes on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Rollout {
    /// The last turn ended with its own complete: the start runs at once.
    Ended,
    /// A turn runs; another turn's complete and abort and an unnamed complete do not end it.
    Foreign,
    /// A turn runs; an unnamed abort does not end it, its own abort does.
    Aborted,
    /// No turn ever started: user and tool records and an end with no start prove no end.
    NoStart,
    /// After an ended turn a new start line is torn, then completed, then that turn ends.
    Torn,
    /// Ended at the early read; a turn opens before the late read of the claimed start.
    Late,
}

/// The start's caller: a `turn/start`, or the routine executor's fresh routine.
#[derive(Clone, Copy, PartialEq, Debug)]
enum By {
    Http,
    Routine,
}

async fn codex_start(id: u64, by: By, case: Rollout) {
    use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
    const ORIGIN: &str = "G-codex-native";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let codex = ProviderKind::Codex;
    let start = Some(by == By::Routine);
    let rig = IntakeRig::build(
        id,
        false,
        codex.clone(),
        Some(pool.clone()),
        start,
        Some(ORIGIN),
        None,
    );
    let rig = rig.await;
    let key = discord::adk_session::build_adk_session_key(&rig.shared, rig.channel, &codex, None);
    let key = key.await.expect("channel session key");
    discord::inflight::seed_session_row_keyed(&pool, &key, rig.channel.get(), None).await;
    let append = |rows: &str| {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&rig.transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, rows.as_bytes()).unwrap();
    };
    let (earlier, ended) = (opening("t0", "earlier prompt"), answer_for("t0", "earlier"));
    // Each step keeps the start waiting; the last record ends the turn it waits on.
    let (begin, steps, end): (String, Vec<(&str, String)>, String) = match case {
        Rollout::Ended => (earlier + &ended, vec![], String::new()),
        Rollout::Late => (earlier + &ended, vec![], answer_for("t2", "late")),
        Rollout::Foreign => (
            opening("native-turn", "native prompt"),
            vec![
                ("another turn's complete", complete(Some("other-turn"))),
                ("another turn's abort", abort(Some("other-turn"))),
                ("an unnamed complete", complete(None)),
            ],
            answer_for("native-turn", "native answer"),
        ),
        Rollout::Aborted => (
            opening("native-turn", "native prompt"),
            vec![("an unnamed abort", abort(None))],
            abort(Some("native-turn")),
        ),
        Rollout::NoStart => (
            String::new(),
            vec![
                (
                    "user and tool records",
                    row(json!({"type":"response_item","payload":{"type":"message",
                        "role":"user","content":[{"type":"input_text","text":"typed"}]}}))
                        + &row(json!({"type":"response_item","payload":{
                            "type":"function_call","name":"shell","arguments":"{}"}})),
                ),
                ("an end with no start", complete(Some("ghost"))),
                ("a started turn", opening("t2", "native prompt")),
            ],
            answer_for("t2", "native answer"),
        ),
        Rollout::Torn => (
            earlier + &ended + TORN_START.0,
            vec![("the completed start line", TORN_START.1.to_string())],
            answer_for("t1", "native answer"),
        ),
    };
    append(&begin);
    let shim = Shim::install(&rig);
    if case == Rollout::Late {
        let row = discord::runtime_store::discord_inflight_root().unwrap();
        let row = row.join("codex").join(format!("{id}.json"));
        shim.busy_once_claimed(&row, &rig.transcript);
        fs::write(shim.dir.join("busy-rows"), opening("t2", "native prompt")).unwrap();
    }
    let channels = [OriginChannel {
        provider: "codex".into(),
        channel_id: rig.channel.get(),
    }];
    let answer = || lookup(&pool, "agent", ORIGIN, &channels);
    assert!(matches!(
        answer().await,
        Ok(Some(InputResult::Accepted { .. }))
    ));
    let (drive, mut settled) = rig.drive();
    if case == Rollout::Ended {
        let bridge = bridge_probe(&rig);
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        codex_runs_once(&rig, &pool, ORIGIN, &mut settled, bridge, append).await;
        drop(rig);
        pool.close().await;
        db.drop().await;
        return;
    }
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    let held = next_settle(&mut settled, &rig.trace).await;
    let waits = (
        held,
        events(&rig.renderer),
        origin_receipts(&pool, ORIGIN).await,
    );
    let expected = (Err(START_WAITS.to_string()), vec![], vec![]);
    assert_eq!(waits, expected, "{}", rig.trace.text());
    let headless = format!("[session-strategy] channel={id}");
    if case == Rollout::Late {
        assert!(!shim.dir.join("busy-armed").exists(), "no claimed read");
        wait("the held attempt's row cleared", || {
            !discord::runtime_store::discord_inflight_root()
                .unwrap()
                .join("codex")
                .join(format!("{id}.json"))
                .exists()
        });
    } else {
        // Held before its claim: no headless turn began and nothing was posted.
        let started = rig.trace.text().contains(&headless);
        let posted = (rig.mock.written_texts().len(), rig.mock.deleted().len());
        assert_eq!((started, posted), (false, (0, 0)), "{}", rig.trace.text());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !matches!(answer().await, Ok(Some(InputResult::Accepted { .. }))) {
        assert!(std::time::Instant::now() < deadline, "not accepted again");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for (step, rows) in std::iter::once(("the first read", String::new())).chain(steps) {
        append(&rows);
        // The watch reads the pane twice in this window and hands nothing.
        let again = tokio::time::timeout(Duration::from_secs(5), settled.recv()).await;
        assert!(again.is_err(), "handed after {step}: {again:?}");
        let state = rig.state().await;
        let left = (
            events(&rig.renderer),
            origin_receipts(&pool, ORIGIN).await,
            state["items"].as_array().unwrap().len(),
            state["head_injection"].is_null(),
        );
        assert_eq!(left, (vec![], vec![], 1, true), "after {step}: {state}");
        if case != Rollout::Late {
            assert!(
                !rig.trace.text().contains(&headless),
                "claimed after {step}"
            );
        }
    }
    let bridge = bridge_probe(&rig);
    append(&end);
    codex_runs_once(&rig, &pool, ORIGIN, &mut settled, bridge, append).await;
    drop(rig);
    pool.close().await;
    db.drop().await;
}

fn bridge_probe(rig: &IntakeRig) -> tokio::sync::oneshot::Receiver<()> {
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    bridge
}

/// The start, now on a pane whose turn ended, runs once as its own turn with no other wake: one
/// paste and Enter, one completed receipt, and an empty queue.
async fn codex_runs_once(
    rig: &IntakeRig,
    pool: &sqlx::PgPool,
    origin: &str,
    settled: &mut tokio::sync::mpsc::UnboundedReceiver<Result<(), String>>,
    bridge: tokio::sync::oneshot::Receiver<()>,
    append: impl Fn(&str),
) {
    let outbox = pool.clone();
    let worker = tokio::spawn(async move {
        loop {
            let sent = "UPDATE message_outbox SET status = 'sent' WHERE status = 'pending'";
            let _ = sqlx::query(sent).execute(&outbox).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let ran = next_settle(settled, &rig.trace).await;
    assert_eq!(ran, Ok(()), "{}", rig.trace.text());
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    // Codex records the submitted prompt, answers it and ends the start's own turn.
    let body = fs::read_to_string(rig.renderer.join("body")).unwrap();
    append(&(opening("own-turn", &body) + &answer_for("own-turn", "own turn answer")));
    tokio::time::timeout(Duration::from_secs(90), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    let again = tokio::time::timeout(Duration::from_secs(6), settled.recv()).await;
    worker.abort();
    let after = (events(&rig.renderer), origin_receipts(pool, origin).await);
    let expected = (
        vec!["paste".to_string(), "Enter".to_string()],
        vec!["completed".to_string()],
    );
    assert_eq!(after, expected, "{}", rig.trace.text());
    assert!(!matches!(again, Ok(Some(_))), "a second handoff: {again:?}");
    assert!(rig.state().await["items"].as_array().unwrap().is_empty());
}

/// Runs `codex_start` in its own child process, as its tmux and Postgres fixtures need.
async fn codex_pg(name: &str, id: u64, by: By, case: Rollout) {
    let test =
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::";
    if creator_child(&format!("{test}rollback::native_read::codex::{name}")) {
        codex_start(id, by, case).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_start_on_a_pane_whose_turn_ended_runs_at_once_pg() {
    let name = "a_codex_start_on_a_pane_whose_turn_ended_runs_at_once_pg";
    codex_pg(name, 7_772_201, By::Http, Rollout::Ended).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_start_waits_through_other_and_unnamed_ends_pg() {
    let name = "a_codex_start_waits_through_other_and_unnamed_ends_pg";
    codex_pg(name, 7_772_202, By::Http, Rollout::Foreign).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_routine_waits_through_other_and_unnamed_ends_pg() {
    let name = "a_codex_routine_waits_through_other_and_unnamed_ends_pg";
    codex_pg(name, 7_772_203, By::Routine, Rollout::Foreign).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_start_runs_after_its_turns_own_abort_pg() {
    let name = "a_codex_start_runs_after_its_turns_own_abort_pg";
    codex_pg(name, 7_772_204, By::Http, Rollout::Aborted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_routine_runs_after_its_turns_own_abort_pg() {
    let name = "a_codex_routine_runs_after_its_turns_own_abort_pg";
    codex_pg(name, 7_772_205, By::Routine, Rollout::Aborted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_start_on_a_rollout_with_no_start_waits_pg() {
    let name = "a_codex_start_on_a_rollout_with_no_start_waits_pg";
    codex_pg(name, 7_772_206, By::Http, Rollout::NoStart).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_routine_on_a_rollout_with_no_start_waits_pg() {
    let name = "a_codex_routine_on_a_rollout_with_no_start_waits_pg";
    codex_pg(name, 7_772_207, By::Routine, Rollout::NoStart).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_start_waits_while_a_new_start_line_is_torn_pg() {
    let name = "a_codex_start_waits_while_a_new_start_line_is_torn_pg";
    codex_pg(name, 7_772_208, By::Http, Rollout::Torn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_routine_waits_while_a_new_start_line_is_torn_pg() {
    let name = "a_codex_routine_waits_while_a_new_start_line_is_torn_pg";
    codex_pg(name, 7_772_209, By::Routine, Rollout::Torn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_codex_start_waits_unsent_for_a_turn_opened_before_its_late_read_pg() {
    let name = "a_claimed_codex_start_waits_unsent_for_a_turn_opened_before_its_late_read_pg";
    codex_pg(name, 7_772_210, By::Http, Rollout::Late).await;
}
