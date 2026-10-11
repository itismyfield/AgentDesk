use super::*;
use crate::services::discord::inflight::o_seed_observation::InitialParseBarrier;
use crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation;
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, clear_tmux_runtime_binding, register_tmux_runtime_binding,
};

enum BindingSource {
    NativeWithWrapperRelay,
    Absent,
}

async fn wrapper_source_case(binding: BindingSource) {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let discord = test_support::MockDiscord::start().await;
    let fixture = configured_install_fixture(discord.http.clone()).await;
    let runtime = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let native = match binding {
        BindingSource::NativeWithWrapperRelay => {
            let native = runtime.join("native-binding.jsonl");
            std::fs::write(&native, []).unwrap();
            register_tmux_runtime_binding(
                &fixture.session,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: native.display().to_string(),
                    relay_output_path: Some(fixture.output.clone()),
                    input_fifo_path: None,
                    session_id: Some("e2e".into()),
                    last_offset: 0,
                    relay_last_offset: Some(0),
                },
            );
            Some(native)
        }
        BindingSource::Absent => {
            assert!(clear_tmux_runtime_binding(&fixture.session));
            None
        }
    };
    let writer = writer::WriterFixture::new(
        &runtime,
        native
            .as_deref()
            .unwrap_or_else(|| Path::new(&fixture.output)),
        ShadowProvider::Claude,
        CHANNEL,
        &fixture.session,
    );
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
    let handle = fixture.handle();
    assert_eq!(
        handle.output_path, fixture.output,
        "the installer prefers its existing wrapper"
    );
    let ticket = watcher_completion::observe(&handle.cancel).unwrap();
    until(
        &observed,
        "wrapper EOF under the accepted binding contract",
        |s| s.event_count("outer_eof") > 0,
    )
    .await;
    let payload = transcript(Turn::Claude);
    append(&fixture, payload.as_bytes());
    if let Some(native) = native.as_ref() {
        // The independent writer reads the native source, while the watcher reads its relay.
        let native_payload = format!(
            "{}\n{payload}",
            serde_json::json!({"type":"system", "subtype":"init", "session_id":"e2e"})
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(native)
            .unwrap()
            .write_all(native_payload.as_bytes())
            .unwrap();
    }
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &handle.cancel,
        Ordering::Release,
    );
    barrier.release();
    joined(ticket).await;
    until(&observed, "independent selected-source O receipt", |_| {
        writer.posts() == [BODY] && posted(&writer) == [BODY]
    })
    .await;
    finish_writer(stop, actor).await;
    let joined = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 1);
    assert_eq!(joined, [(joined[0].0, Outcome::Returned)]);
    zero_legacy(&observed, &discord);
    let key = observed.snapshot();
    assert_eq!(
        key.retired_terminal_ranges,
        [(0, payload.len() as u64)],
        "{key:?}"
    );
    assert_eq!(key.event_count("retired_terminal_consumed"), 1);
    assert_eq!(key.event_count("post_stream_exit"), 1);
    assert_eq!(writer.posts(), [BODY]);
    assert_eq!(posted(&writer), [BODY]);
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
    eprintln!(
        "T6_B_WRAPPER_KEY={key:?}\nT6_B_WRAPPER_RAW={:?}",
        observed.raw_process_snapshot()
    );
}

#[tokio::test]
async fn t6_b_native_binding_with_wrapper_relay_preserves_installer_selection() {
    if child(
        "source_contract::t6_b_native_binding_with_wrapper_relay_preserves_installer_selection",
    ) {
        wrapper_source_case(BindingSource::NativeWithWrapperRelay).await;
    }
}

#[tokio::test]
async fn t6_b_confirmed_absent_binding_keeps_current_wrapper_terminal() {
    if child("source_contract::t6_b_confirmed_absent_binding_keeps_current_wrapper_terminal") {
        wrapper_source_case(BindingSource::Absent).await;
    }
}

#[tokio::test]
async fn t6_b_pause_epoch_change_self_exits_without_test_cancellation() {
    if child("source_contract::t6_b_pause_epoch_change_self_exits_without_test_cancellation") {
        retained_case_with_cancel(Invalidated::Epoch, false).await;
    }
}

#[tokio::test]
async fn t6_b_resume_request_self_exits_without_test_cancellation() {
    if child("source_contract::t6_b_resume_request_self_exits_without_test_cancellation") {
        retained_case_with_cancel(Invalidated::Resume, false).await;
    }
}

#[tokio::test]
async fn t6_b_reset_between_partial_utf8_and_successor_terminal_keeps_old_provenance() {
    if !child(
        "source_contract::t6_b_reset_between_partial_utf8_and_successor_terminal_keeps_old_provenance",
    ) {
        return;
    }
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
    writer.acquired();
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let first = fixture.handle();
    let first_join = watcher_completion::observe(&first.cancel).unwrap();
    until(&observed, "first wrapper EOF before reset tail", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let prefix = transcript(Turn::Claude);
    let tail_body = "후속 UTF-8 provenance survives a reset";
    let tail = text_turn(tail_body, "reset-tail");
    let scalar = tail.find('후').unwrap();
    let split = scalar + 1;
    let mut first_append = prefix.as_bytes().to_vec();
    first_append.extend_from_slice(&tail.as_bytes()[..split]);
    append(&fixture, &first_append);
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(&first.cancel, Ordering::Release);
    barrier.release();
    joined(first_join).await;
    let carried = capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert!(
        carried[0].origin_is_retired() && carried[0].has_utf8_carry,
        "{carried:?}"
    );
    assert_eq!(carried[0].buffer.as_bytes(), &tail.as_bytes()[..scalar]);
    drop(barrier);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let successor = fixture.handle();
    let second_join = watcher_completion::observe(&successor.cancel).unwrap();
    until(&observed, "successor adopts original UTF-8 capsule", |s| {
        s.event_count("capsule_o_adopted") > 0
    })
    .await;
    let incarnation_before = fixture
        .shared
        .relay_frontier_token(fixture.channel)
        .reset_incarnation;
    let coord = fixture.shared.tmux_relay_coord(fixture.channel);
    coord.confirmed_end_offset.store(100, Ordering::Release);
    assert!(coord.reset_confirmed_frontier(100, 0));
    assert_ne!(
        fixture
            .shared
            .relay_frontier_token(fixture.channel)
            .reset_incarnation,
        incarnation_before
    );
    append(&fixture, &tail.as_bytes()[split..]);
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &successor.cancel,
        Ordering::Release,
    );
    barrier.release();
    joined(second_join).await;
    until(
        &observed,
        "independent O receipts remain distinct from source consumption",
        |_| writer.posts() == [BODY, tail_body] && posted(&writer) == [BODY, tail_body],
    )
    .await;
    finish_writer(stop, actor).await;
    let joined = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 2);
    assert_eq!(joined.len(), 2);
    assert!(
        joined
            .iter()
            .all(|(_, result)| *result == Outcome::Returned)
    );
    zero_legacy(&observed, &discord);
    let key = observed.snapshot();
    let pending = capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(
        key.retired_terminal_ranges,
        [(0, prefix.len() as u64)],
        "{key:?}"
    );
    assert_eq!(key.event_count("retired_terminal_consumed"), 1);
    assert!(key.event_count("retired_terminal_retained") > 0);
    assert_eq!(key.event_count("post_stream_exit"), 2);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert!(pending[0].origin_is_retired());
    assert_eq!(pending[0].body, tail_body);
    assert!(!pending[0].has_startup_snapshot && !pending[0].has_identity);
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
    eprintln!(
        "T6_B_RESET_KEY={key:?}\nT6_B_RESET_RAW={:?}\nT6_B_RESET_PENDING={pending:?}",
        observed.raw_process_snapshot()
    );
}

#[tokio::test]
async fn t6_b_successor_merges_fresh_task_context_from_real_nonterminal_capsule() {
    if !child(
        "source_contract::t6_b_successor_merges_fresh_task_context_from_real_nonterminal_capsule",
    ) {
        return;
    }
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
    writer.acquired();
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let first = fixture.handle();
    let first_join = watcher_completion::observe(&first.cancel).unwrap();
    until(
        &observed,
        "initial EOF before nonterminal task custody",
        |s| s.event_count("outer_eof") > 0,
    )
    .await;
    let barrier = InitialParseBarrier::arm(&fixture.provider, CHANNEL);
    let soft = format!(
        "{}\n",
        serde_json::json!({"type":"system", "subtype":"stop_hook_summary", "session_id":"e2e"})
    );
    let user = format!(
        "{}\n",
        serde_json::json!({"type":"user", "message":{"role":"user", "content":"fresh task prompt"}})
    );
    let tail = format!("{user}{}", transcript(Turn::Task));
    let payload = format!("{soft}{tail}");
    append(&fixture, payload.as_bytes());
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(&first.cancel, Ordering::Release);
    barrier.release();
    joined(first_join).await;
    let carried = capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert!(carried[0].origin_is_retired(), "{carried:?}");
    assert!(carried[0].body.is_empty(), "{carried:?}");
    assert_eq!(carried[0].buffer, tail);
    assert_eq!(carried[0].offset, payload.len() as u64);
    assert!(!carried[0].has_startup_snapshot && !carried[0].has_identity);
    assert_eq!(observed.snapshot().event_count("parsed_terminal"), 0);
    assert!(discord.requests_for(CHANNEL).is_empty());
    drop(barrier);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let successor = fixture.handle();
    let second_join = watcher_completion::observe(&successor.cancel).unwrap();
    barrier.wait().await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &successor.cancel,
        Ordering::Release,
    );
    barrier.release();
    joined(second_join).await;
    until(
        &observed,
        "independent source unit has a real O receipt",
        |_| writer.posts() == [BODY] && posted(&writer) == [BODY],
    )
    .await;
    finish_writer(stop, actor).await;
    let joined = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 2);
    assert_eq!(joined.len(), 2);
    assert!(
        joined
            .iter()
            .all(|(_, result)| *result == Outcome::Returned)
    );
    zero_legacy(&observed, &discord);
    let key = observed.snapshot();
    let requests = discord.requests_for(CHANNEL);
    eprintln!(
        "T6_B_FRESH_CONTEXT_KEY={key:?}\nT6_B_FRESH_CONTEXT_RAW={:?}\nT6_B_FRESH_CONTEXT_HTTP={requests:?}",
        observed.raw_process_snapshot()
    );
    assert_eq!(key.event_count("capsule_o_adopted"), 1);
    assert_eq!(key.event_count("parsed_nonterminal"), 1);
    assert_eq!(key.event_count("post_stream_exit"), 2);
    assert_eq!(key.retired_terminal_ranges, [(0, payload.len() as u64)]);
    assert_eq!(key.event_count("retired_terminal_consumed"), 1);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST "))
            .count(),
        1,
        "fresh native task context must reach its actual card POST: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("background work"))
    );
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
}
