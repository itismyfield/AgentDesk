//! Actual retired installer, provider decoder, terminal custody and independent O delivery.
use super::*;
use crate::services::discord::health::legacy_supervision::{RetiredForTest, test_support};
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot, TerminalBarrier};
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::discord::task_supervisor::watcher_completion::{self, Outcome};
use crate::services::discord::tmux::{InstallFixture, OOnlyInstallOutcome};
use crate::services::tui_o::store::ledger::PieceOutcome;
use poise::serenity_prelude::Http;
use std::io::Write;

const TERMINAL_CHILD: &str = "ADK_RETIRED_TERMINAL_E2E_CHILD";
const CHANNEL: u64 = 63_251_200;

#[path = "o_retired_terminal_race_tests.rs"]
mod await_races;
#[path = "o_retired_terminal_legacy_fixture_tests.rs"]
mod legacy_fixture;
#[path = "o_retired_terminal_source_tests.rs"]
mod source_contract;

fn child(name: &str) -> bool {
    if std::env::var_os(TERMINAL_CHILD).is_some() {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        r#"#!/bin/sh
while [ "${1#-}" != "$1" ]; do
  [ "$1" = "-V" ] && { echo 'tmux fixture'; exit 0; }
  shift
done
case "$1" in
  has-session) exit 0 ;;
  list-panes) echo 0; exit 0 ;;
  capture-pane) printf '%s\n' '⏺ Running a fixture command'; exit 0 ;;
  *) exit 97 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture", "--test-threads=1"])
        .env(TERMINAL_CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("TMPDIR", root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .env(cutover::test_override::CHILD_ENV, "1")
        .env(
            cutover::test_override::CHANNELS_ENV,
            serde_json::to_string(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]).unwrap(),
        )
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

async fn fixture() -> (
    InstallFixture,
    test_support::MockDiscord,
    writer::WriterFixture,
) {
    let discord = test_support::MockDiscord::start().await;
    let (fixture, writer) = configured_fixture(discord.http.clone()).await;
    (fixture, discord, writer)
}

async fn configured_fixture(http: Arc<Http>) -> (InstallFixture, writer::WriterFixture) {
    let fixture = configured_install_fixture(http).await;
    let writer = writer_for_fixture(&fixture);
    (fixture, writer)
}

fn writer_for_fixture(fixture: &InstallFixture) -> writer::WriterFixture {
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    writer::WriterFixture::new(
        &root,
        Path::new(&fixture.output),
        ShadowProvider::Claude,
        CHANNEL,
        &fixture.session,
    )
}

async fn configured_install_fixture(http: Arc<Http>) -> InstallFixture {
    let mut fixture = InstallFixture::new(ProviderKind::Claude, CHANNEL).await;
    fixture.http = http;
    let generation =
        crate::services::tmux_common::session_temp_path(&fixture.session, "generation");
    std::fs::write(&generation, "retired-terminal-generation").unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &fixture.session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: fixture.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let mut row = inflight_with_identity_offset(CHANNEL, &fixture.session, 710, STARTED, Some(0));
    row.current_msg_id = 88010;
    row.output_path = Some(fixture.output.clone());
    row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    let path = test_support::seed_backfill_row(&row);
    drop(crate::services::discord::inflight::lock_inflight_state_path(&path).unwrap());
    fixture
}

fn append(fixture: &InstallFixture, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.output)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

async fn until(observed: &Guard, what: &str, done: impl Fn(&Snapshot) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let snapshot = observed.snapshot();
        if done(&snapshot) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out on {what}: key={snapshot:?} raw={:?}",
            observed.raw_process_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn joined(ticket: watcher_completion::Ticket) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), ticket.wait())
            .await
            .unwrap(),
        Outcome::Returned
    );
}

fn zero_legacy(observed: &Guard, discord: &test_support::MockDiscord) {
    for snapshot in [observed.snapshot(), observed.raw_process_snapshot()] {
        assert_eq!(snapshot.writable_inflight_load_calls, 0, "{snapshot:?}");
        assert_eq!(snapshot.readonly_inflight_load_calls, 0, "{snapshot:?}");
        assert_eq!(snapshot.compatibility_backfill_attempts, 0, "{snapshot:?}");
        assert_eq!(snapshot.legacy_prefix_restore_calls, 0, "{snapshot:?}");
        assert_eq!(
            snapshot.event_count("capsule_legacy_adopted"),
            0,
            "{snapshot:?}"
        );
        assert_eq!(
            snapshot.event_count("legacy_row_seed_adopted"),
            0,
            "{snapshot:?}"
        );
        assert_eq!(
            snapshot.event_count("terminal_preflight_entries"),
            0,
            "{snapshot:?}"
        );
    }
    assert!(
        discord.requests_for(CHANNEL).iter().all(|request| {
            !((request.starts_with("POST ") || request.starts_with("PATCH "))
                && request.contains(BODY))
        }),
        "Legacy body transport: {:?}",
        discord.requests_for(CHANNEL)
    );
}

fn posted(writer: &writer::WriterFixture) -> Vec<String> {
    let channel = writer.channel();
    let ledger = channel.ledger();
    (0..ledger.next_serial())
        .filter_map(|serial| ledger.piece(serial))
        .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
        .map(|piece| piece.payload.clone())
        .collect()
}

async fn finish_writer(
    stop: tokio::sync::watch::Sender<bool>,
    actor: tokio::task::JoinHandle<Option<crate::services::tui_o::writer::deliver::StopCause>>,
) {
    // A replaced source may already have stopped the actor; its actual task still joins.
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(10), actor)
        .await
        .unwrap()
        .unwrap();
}

#[derive(Clone, Copy)]
enum Case {
    CancelAfterParse,
    NoCancel,
    PendingReceipt,
    Task,
}

async fn terminal_case(case: Case) {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, discord, writer) = fixture().await;
    if matches!(case, Case::NoCancel) {
        let mut row =
            inflight_with_identity_offset(CHANNEL, &fixture.session, 710, STARTED, Some(0));
        row.current_msg_id = 88010;
        row.output_path = Some(fixture.output.clone());
        row.set_relay_owner_kind(RelayOwnerKind::Watcher);
        let path = test_support::seed_backfill_row(&row);
        drop(crate::services::discord::inflight::lock_inflight_state_path(&path).unwrap());
    }
    let _retired = RetiredForTest::new("claude", CHANNEL);
    let _owned =
        cutover::test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&inflight);
    let frontier_before = fixture.shared.relay_frontier_token(fixture.channel);
    let observed = Guard::new(&fixture.provider, CHANNEL);
    observed.register_source(Path::new(&fixture.output));
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    let pending_post = matches!(case, Case::PendingReceipt).then(|| writer.hold_next_post());
    writer.acquired();
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let handle = fixture.handle();
    let ticket = watcher_completion::observe(&handle.cancel).unwrap();
    until(&observed, "actual initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let payload = transcript(if matches!(case, Case::Task) {
        Turn::Task
    } else {
        Turn::Claude
    });
    append(&fixture, payload.as_bytes());
    tokio::time::timeout(Duration::from_secs(25), barrier.wait())
        .await
        .unwrap();
    let parsed = observed.snapshot();
    assert!(parsed.event_count("parsed_terminal") > 0, "{parsed:?}");
    assert!(
        parsed.parser_responses.iter().any(|body| body == BODY),
        "{parsed:?}"
    );
    assert!(
        parsed.decoded_chunks.concat().contains(&payload),
        "{parsed:?}"
    );
    if !matches!(case, Case::NoCancel) {
        crate::services::discord::tmux::tmux_watcher::cancel_for_test(
            &handle.cancel,
            Ordering::Release,
        );
    }
    barrier.release();
    if matches!(case, Case::NoCancel) {
        until(&observed, "terminal dispatch", |s| {
            s.event_count("retired_terminal_consumed") > 0
                || s.event_count("terminal_preflight_entries") > 0
        })
        .await;
        crate::services::discord::tmux::tmux_watcher::cancel_for_test(
            &handle.cancel,
            Ordering::Release,
        );
    }
    joined(ticket).await;
    if let Some(pending_post) = pending_post {
        tokio::time::timeout(Duration::from_secs(25), pending_post.entered())
            .await
            .unwrap();
        assert!(writer.posts().is_empty());
        assert!(
            posted(&writer).is_empty(),
            "a prepared piece is not a receipt"
        );
        assert!(writer.channel().ledger().unresolved().is_some());
        assert_eq!(
            fixture.shared.relay_frontier_token(fixture.channel),
            frontier_before
        );
        assert!(
            dr::read_record(&fixture.provider, CHANNEL)
                .and_then(|r| r.delivered_frontier)
                .is_none()
        );
        assert_eq!(test_support::tree_fingerprint(&inflight), before);
        pending_post.release();
    }
    until(&observed, "actual O POST and ledger outcome", |_| {
        writer.posts() == [BODY] && posted(&writer) == [BODY]
    })
    .await;
    finish_writer(stop, actor).await;
    let joined_submissions = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 1);
    assert_eq!(joined_submissions.len(), 1);
    assert!(
        joined_submissions
            .iter()
            .all(|(_, outcome)| *outcome == Outcome::Returned)
    );
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    eprintln!(
        "T6_B_KEY={key:?}\nT6_B_RAW={raw:?}\nT6_B_POSTS={:?}\nT6_B_LEDGER={:?}\nT6_B_HTTP={:?}",
        writer.posts(),
        posted(&writer),
        discord.requests_for(CHANNEL)
    );
    zero_legacy(&observed, &discord);
    assert!(key.watcher_initialization_observed && key.stream_decoder_initialization_observed);
    assert_eq!(key.retired_terminal_ranges, [(0, payload.len() as u64)]);
    assert_eq!(key.event_count("retired_terminal_consumed"), 1);
    assert!(key.event_count("post_stream_exit") > 0, "{key:?}");
    assert_eq!(writer.posts(), [BODY]);
    assert_eq!(posted(&writer), [BODY]);
    assert_eq!(
        fixture.shared.relay_frontier_token(fixture.channel),
        frontier_before
    );
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
    if matches!(case, Case::Task) {
        let requests = discord.requests_for(CHANNEL);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST "))
                .count(),
            1,
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|request| request.contains("background work")),
            "{requests:?}"
        );
    }
}

#[tokio::test]
async fn t6_b_retired_terminal_after_own_cancel_consumes_once_and_o_posts_once_through_join() {
    if child("t6_b_retired_terminal_after_own_cancel_consumes_once_and_o_posts_once_through_join") {
        terminal_case(Case::CancelAfterParse).await;
    }
}

#[tokio::test]
async fn t6_b_no_cancel_terminal_reaches_single_dispatch_before_legacy_preflight() {
    if child("t6_b_no_cancel_terminal_reaches_single_dispatch_before_legacy_preflight") {
        terminal_case(Case::NoCancel).await;
    }
}

#[tokio::test]
async fn t6_b_pending_o_post_does_not_promote_consumption_to_receipt() {
    if child("t6_b_pending_o_post_does_not_promote_consumption_to_receipt") {
        terminal_case(Case::PendingReceipt).await;
    }
}

#[tokio::test]
async fn t6_b_task_terminal_promotes_native_card_without_loading_legacy_row() {
    if child("t6_b_task_terminal_promotes_native_card_without_loading_legacy_row") {
        terminal_case(Case::Task).await;
    }
}

#[derive(Clone, Copy, Debug)]
enum Invalidated {
    Owner,
    Source,
    Generation,
    Reset,
    OtherCancel,
    Pause,
    Epoch,
    Resume,
    Delivered,
    RecentStop,
    OwnershipFalse,
    OwnershipError,
}

async fn retained_case(changed: Invalidated) {
    retained_case_with_cancel(changed, true).await;
}

async fn retained_case_with_cancel(changed: Invalidated, cancel_from_test: bool) {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, discord, writer) = fixture().await;
    let _retired = RetiredForTest::new("claude", CHANNEL);
    let _owned =
        cutover::test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&inflight);
    let observed = Guard::new(&fixture.provider, CHANNEL);
    observed.register_source(Path::new(&fixture.output));
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let handle = fixture.handle();
    let ticket = watcher_completion::observe(&handle.cancel).unwrap();
    until(&observed, "initial EOF before authority change", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let payload = transcript(Turn::Claude);
    append(&fixture, payload.as_bytes());
    barrier.wait().await;
    let successor_cancel = Arc::new(AtomicBool::new(false));
    let alternate_ownership = match changed {
        Invalidated::OwnershipFalse => Some(cutover::test_override::force_channels(&[])),
        Invalidated::OwnershipError => Some(cutover::test_override::force_channels(&[(
            CHANNEL,
            RuntimeHandoffKind::CodexTui,
        )])),
        _ => None,
    };
    match changed {
        Invalidated::Owner => std::fs::write(
            crate::services::tmux_common::tmux_owner_path(&fixture.session),
            "different-runtime-owner",
        )
        .unwrap(),
        Invalidated::Source => {
            std::fs::rename(&fixture.output, format!("{}.opened", fixture.output)).unwrap();
            std::fs::write(&fixture.output, &payload).unwrap();
        }
        Invalidated::Generation => {
            let generation =
                crate::services::tmux_common::session_temp_path(&fixture.session, "generation");
            std::fs::write(&generation, "new-generation").unwrap();
            filetime::set_file_mtime(
                generation,
                filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() + 60, 0),
            )
            .unwrap();
        }
        Invalidated::Reset => {
            let coord = fixture.shared.tmux_relay_coord(fixture.channel);
            coord.confirmed_end_offset.store(100, Ordering::Release);
            assert!(coord.reset_confirmed_frontier(100, 0));
        }
        Invalidated::OtherCancel => fixture
            .shared
            .tmux_watchers
            .replace_paired_cancel_for_test(&fixture.session, successor_cancel.clone()),
        Invalidated::Pause => handle.paused.store(true, Ordering::Release),
        Invalidated::Epoch => {
            handle.pause_epoch.fetch_add(1, Ordering::AcqRel);
        }
        Invalidated::Resume => {
            *handle.resume_offset.lock().unwrap() = Some(0);
        }
        Invalidated::Delivered => handle.turn_delivered.store(true, Ordering::Release),
        Invalidated::RecentStop => {
            crate::services::discord::tmux::record_recent_turn_stop(
                fixture.channel,
                Some(&fixture.session),
                "fixture terminal stopped",
            )
            .await
        }
        Invalidated::OwnershipFalse | Invalidated::OwnershipError => {}
    }
    if cancel_from_test {
        crate::services::discord::tmux::tmux_watcher::cancel_for_test(
            &handle.cancel,
            Ordering::Release,
        );
    }
    barrier.release();
    joined(ticket).await;
    assert!(handle.cancel.load(Ordering::Acquire));
    finish_writer(stop, actor).await;
    let joined_submissions = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 1);
    assert_eq!(joined_submissions.len(), 1);
    assert!(
        joined_submissions
            .iter()
            .all(|(_, outcome)| *outcome == Outcome::Returned)
    );
    let pending =
        crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::pending(
            &fixture.shared,
            fixture.channel,
        )
        .await;
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    eprintln!(
        "T6_B_RETAINED={changed:?}\nT6_B_KEY={key:?}\nT6_B_RAW={raw:?}\nT6_B_CAPSULES={pending:?}"
    );
    zero_legacy(&observed, &discord);
    assert_eq!(key.event_count("retired_terminal_consumed"), 0, "{key:?}");
    assert!(key.retired_terminal_ranges.is_empty());
    assert!(key.event_count("retired_terminal_retained") > 0, "{key:?}");
    assert!(key.event_count("post_stream_exit") > 0, "{key:?}");
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert!(pending[0].origin_is_retired());
    assert_eq!(pending[0].body, BODY);
    assert!(!pending[0].has_startup_snapshot && !pending[0].has_identity);
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
    assert!(
        !successor_cancel.load(Ordering::Acquire),
        "old custody cannot cancel another incarnation"
    );
    drop(alternate_ownership);
}

#[tokio::test]
async fn t6_b_retains_terminal_when_owner_source_generation_or_reset_changes() {
    if !child("t6_b_retains_terminal_when_owner_source_generation_or_reset_changes") {
        return;
    }
    for changed in [
        Invalidated::Owner,
        Invalidated::Source,
        Invalidated::Generation,
        Invalidated::Reset,
    ] {
        retained_case(changed).await;
    }
}

#[tokio::test]
async fn t6_b_retains_terminal_when_cancel_arc_is_replaced_without_canceling_successor() {
    if child("t6_b_retains_terminal_when_cancel_arc_is_replaced_without_canceling_successor") {
        retained_case(Invalidated::OtherCancel).await;
    }
}

#[tokio::test]
async fn t6_b_retains_terminal_on_pause_epoch_resume_delivery_and_recent_stop() {
    if !child("t6_b_retains_terminal_on_pause_epoch_resume_delivery_and_recent_stop") {
        return;
    }
    for changed in [
        Invalidated::Pause,
        Invalidated::Epoch,
        Invalidated::Resume,
        Invalidated::Delivered,
        Invalidated::RecentStop,
    ] {
        retained_case(changed).await;
    }
}

#[tokio::test]
async fn t6_b_ownership_false_and_error_preserve_terminal_without_legacy_fallback() {
    if !child("t6_b_ownership_false_and_error_preserve_terminal_without_legacy_fallback") {
        return;
    }
    for changed in [Invalidated::OwnershipFalse, Invalidated::OwnershipError] {
        retained_case(changed).await;
    }
}

fn text_turn(body: &str, id: &str) -> String {
    let rows = [
        serde_json::json!({"type":"assistant", "uuid":id, "apiBlockIndex":0,
            "message":{"id":id, "content":[{"type":"text", "text":body}]}}),
        serde_json::json!({"type":"result", "result":body}),
    ];
    rows.iter().map(|row| format!("{row}\n")).collect()
}

#[tokio::test]
async fn t6_b_terminal_prefix_keeps_next_turn_and_utf8_carry_for_real_successor_install() {
    if !child("t6_b_terminal_prefix_keeps_next_turn_and_utf8_carry_for_real_successor_install") {
        return;
    }
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, discord, writer, legacy, recorded) =
        legacy_fixture::fixture_with_real_legacy_custody().await;
    let _retired = RetiredForTest::new("claude", CHANNEL);
    let _owned =
        cutover::test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&inflight);
    let observed = Guard::new(&fixture.provider, CHANNEL);
    observed.register_source(Path::new(&fixture.output));
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    writer.acquired();
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let first = fixture.handle();
    let first_join = watcher_completion::observe(&first.cancel).unwrap();
    until(&observed, "first install initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let first_payload = transcript(Turn::Claude);
    let tail_body = "후속 응답 — tail remains";
    let tail = text_turn(tail_body, "tail-answer");
    let scalar = tail
        .as_bytes()
        .windows("후".len())
        .position(|bytes| bytes == "후".as_bytes())
        .unwrap();
    let split = scalar + 1;
    let mut first_append = first_payload.as_bytes().to_vec();
    first_append.extend_from_slice(&tail.as_bytes()[..split]);
    append(&fixture, &first_append);
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(&first.cancel, Ordering::Release);
    barrier.release();
    joined(first_join).await;
    let pending =
        crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::pending(
            &fixture.shared,
            fixture.channel,
        )
        .await;
    let retired = pending
        .iter()
        .filter(|p| p.origin_is_retired())
        .collect::<Vec<_>>();
    if retired.len() != 1 || !retired[0].has_utf8_carry || !retired[0].body.is_empty() {
        finish_writer(stop, actor).await;
        let joined = submitted.join_all().await;
        assert_eq!(joined.len(), 1, "{joined:?}");
        zero_legacy(&observed, &discord);
        assert_eq!(retired.len(), 1, "terminal tail was discarded: {pending:?}");
        assert!(retired[0].body.is_empty(), "{pending:?}");
        assert!(retired[0].has_utf8_carry, "{pending:?}");
        unreachable!("invalid terminal tail is asserted after every submitted task joins");
    }
    let pending_tail = retired[0].clone();
    assert!(
        pending_tail.body.is_empty(),
        "only the completed terminal is settled: {pending:?}"
    );
    assert!(pending_tail.has_utf8_carry, "{pending:?}");
    assert_eq!(pending_tail.offset, first_append.len() as u64);
    assert_eq!(pending_tail.buffer.as_bytes(), &tail.as_bytes()[..scalar]);
    assert!(!pending_tail.has_startup_snapshot && !pending_tail.has_identity);
    let legacy_after_prefix = pending
        .into_iter()
        .filter(|p| !p.origin_is_retired())
        .collect::<Vec<_>>();
    let recorded_after_prefix = crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::recorded_original(&fixture.shared, fixture.channel);
    drop(barrier);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let successor = fixture.handle();
    assert!(!Arc::ptr_eq(&first.cancel, &successor.cancel));
    let successor_join = watcher_completion::observe(&successor.cancel).unwrap();
    until(&observed, "actual O capsule adoption", |s| {
        s.event_count("capsule_o_adopted") > 0
    })
    .await;
    append(&fixture, &tail.as_bytes()[split..]);
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &successor.cancel,
        Ordering::Release,
    );
    barrier.release();
    joined(successor_join).await;
    until(&observed, "both independent native O deliveries", |_| {
        writer.posts() == [BODY, tail_body] && posted(&writer) == [BODY, tail_body]
    })
    .await;
    finish_writer(stop, actor).await;
    let joined_submissions = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 2);
    assert_eq!(joined_submissions.len(), 2);
    assert!(
        joined_submissions
            .iter()
            .all(|(_, outcome)| *outcome == Outcome::Returned)
    );
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    eprintln!(
        "T6_B_TAIL_KEY={key:?}\nT6_B_TAIL_RAW={raw:?}\nT6_B_TAIL_POSTS={:?}\nT6_B_TAIL_LEDGER={:?}",
        writer.posts(),
        posted(&writer)
    );
    zero_legacy(&observed, &discord);
    assert_eq!(
        legacy_after_prefix, legacy,
        "terminal prefix must preserve Legacy custody"
    );
    assert_eq!(
        recorded_after_prefix, recorded,
        "terminal prefix must preserve the original Legacy row"
    );
    let legacy_after_successor =
        crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::pending(
            &fixture.shared,
            fixture.channel,
        )
        .await
        .into_iter()
        .filter(|p| !p.origin_is_retired())
        .collect::<Vec<_>>();
    assert_eq!(legacy_after_successor, legacy);
    assert_eq!(crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::recorded_original(&fixture.shared, fixture.channel), recorded);
    assert_eq!(
        key.retired_terminal_ranges,
        [
            (0, first_payload.len() as u64),
            (
                first_payload.len() as u64,
                (first_payload.len() + tail.len()) as u64
            )
        ]
    );
    assert_eq!(key.event_count("retired_terminal_consumed"), 2);
    assert_eq!(key.event_count("post_stream_exit"), 2);
    assert!(
        key.parser_responses
            .iter()
            .any(|response| response == tail_body),
        "{key:?}"
    );
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
}
