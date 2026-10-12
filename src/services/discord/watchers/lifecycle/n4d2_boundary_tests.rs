use super::n4d2_entry_support::*;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::legacy_supervision::{RetiredForTest, test_support};
use crate::services::discord::inflight::o_seed_observation::TerminalBarrier;
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::discord::task_supervisor::watcher_completion::{self, Outcome};
use crate::services::tmux_common as tc;
use crate::services::tui_o::cutover;

const BODY: &str = "fresh N4d2 response survives without Legacy seeds";

fn transcript(provider: &ProviderKind, terminal: bool) -> String {
    let answer = if *provider == ProviderKind::Codex {
        serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":"n4d2-answer", "content":[{"type":"output_text", "text":BODY}]}})
    } else {
        serde_json::json!({"type":"assistant", "uuid":"n4d2-answer", "message":{"id":"answer", "content":[{"type":"text", "text":BODY}]}})
    };
    let mut bytes = format!("{answer}\n");
    if terminal {
        let stop = if *provider == ProviderKind::Codex {
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":BODY}})
        } else {
            serde_json::json!({"type":"result", "result":BODY})
        };
        bytes.push_str(&format!("{stop}\n"));
    }
    bytes
}

fn seed_legacy(fixture: &InstallFixture) {
    let mut row = crate::services::discord::inflight::InflightTurnState::new(
        fixture.provider.clone(),
        fixture.channel.get(),
        Some("n4d2-old".into()),
        7,
        111,
        112,
        "old prompt".into(),
        Some("old-native".into()),
        Some(fixture.session.clone()),
        Some(fixture.output.clone()),
        None,
        0,
    );
    row.turn_start_offset = Some(0);
    row.turn_nonce = Some("old-nonce".into());
    row.full_response = "legacy restored body".into();
    test_support::seed_backfill_row(&row);
}

async fn joined(ticket: watcher_completion::Ticket) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(8), ticket.wait())
            .await
            .expect("outgoing watcher self exits"),
        Outcome::Returned
    );
}

#[tokio::test]
async fn n4d2_restore_actual_first_chunk_is_rowless_for_both_providers() {
    if !isolated(
        module_path!(),
        "n4d2_restore_actual_first_chunk_is_rowless_for_both_providers",
    ) {
        return;
    }
    for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let (fixture, discord) = fixture(provider.clone(), 632_520_700 + index as u64).await;
        seed_legacy(&fixture);
        let _retired = RetiredForTest::new(provider.as_str(), fixture.channel.get());
        let rows = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let before = test_support::tree_fingerprint(&rows);
        let observed = observe(&fixture);
        let submitted = Scope::new(&fixture.shared);
        restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        assert_eq!(
            submitted.submissions(),
            1,
            "public restore submitted a watcher"
        );
        until(&observed, "actual initial EOF", |s| {
            s.event_count("outer_eof") > 0
        })
        .await;
        let fresh = transcript(&provider, false);
        append(&fixture, fresh.as_bytes());
        until(&observed, "actual first decoded source chunk", |s| {
            s.decoded_chunks.concat() == fresh
        })
        .await;
        fixture.cancel_and_join().await;
        let returned = submitted.join_all().await;
        assert_eq!(returned.len(), 1);
        assert!(
            returned
                .iter()
                .all(|(_, outcome)| *outcome == Outcome::Returned)
        );
        assert_zero_loaders(&observed);
        let snapshot = observed.snapshot();
        assert!(
            snapshot.watcher_initialization_observed
                && snapshot.stream_decoder_initialization_observed
        );
        assert!(snapshot.decoded_source_chunks > 0);
        assert!(
            snapshot.parser_responses.iter().any(|body| body == BODY),
            "{snapshot:?}"
        );
        assert_eq!(test_support::tree_fingerprint(&rows), before);
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert!(
            discord
                .requests_for(fixture.channel.get())
                .iter()
                .all(|request| !request.contains(BODY))
        );
    }
}

#[tokio::test]
async fn n4d2_generation_missing_is_fail_closed_and_retry_charges_failure() {
    if !isolated(
        module_path!(),
        "n4d2_generation_missing_is_fail_closed_and_retry_charges_failure",
    ) {
        return;
    }
    use crate::services::discord::health::{
        n4d2_absence, n4d2_arm_absence, retry_pending_watcher_respawn,
    };
    let (fixture, _discord) = fixture(ProviderKind::Claude, 632_520_710).await;
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let marker = tc::session_temp_path(&fixture.session, "generation");
    std::fs::rename(&marker, format!("{marker}.missing")).unwrap();
    let submitted = Scope::new(&fixture.shared);
    assert_eq!(
        fixture.install().await,
        OOnlyInstallOutcome::Deferred(OOnlyInstallReason::SourceUnavailable)
    );
    assert_eq!(submitted.submissions(), 0);
    n4d2_arm_absence(&fixture.provider, fixture.channel, 100);
    assert!(
        retry_pending_watcher_respawn(
            &fixture.registry,
            &fixture.provider,
            std::slice::from_ref(&fixture.shared),
            fixture.channel,
            100
        )
        .await
    );
    let absent = n4d2_absence(&fixture.provider, fixture.channel)
        .expect("missing generation keeps recovery obligation");
    assert_eq!(
        (absent.0, absent.1, absent.3),
        (100, 1, Some(OOnlyInstallReason::SourceUnavailable))
    );
    assert!(absent.2 > 100);
    assert_eq!(submitted.submissions(), 0);
    assert!(
        !retry_pending_watcher_respawn(
            &fixture.registry,
            &fixture.provider,
            std::slice::from_ref(&fixture.shared),
            fixture.channel,
            101
        )
        .await
    );
    assert_eq!(
        n4d2_absence(&fixture.provider, fixture.channel),
        Some(absent)
    );
}

#[derive(Clone, Copy, Debug)]
enum Invalidation {
    Source,
    NonRegular,
    Generation,
    Owner,
    Reset,
}

#[tokio::test]
async fn n4d2_terminal_invalidated_authority_self_exits_and_retains_custody() {
    if !isolated(
        module_path!(),
        "n4d2_terminal_invalidated_authority_self_exits_and_retains_custody",
    ) {
        return;
    }
    for (index, change) in [
        Invalidation::Source,
        Invalidation::NonRegular,
        Invalidation::Generation,
        Invalidation::Owner,
        Invalidation::Reset,
    ]
    .into_iter()
    .enumerate()
    {
        let (fixture, discord) = fixture(ProviderKind::Claude, 632_520_720 + index as u64).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let _owned = cutover::test_override::force_channels(&[(
            fixture.channel.get(),
            RuntimeHandoffKind::ClaudeTui,
        )]);
        seed_legacy(&fixture);
        let rows = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let before = test_support::tree_fingerprint(&rows);
        let observed = observe(&fixture);
        let submitted = Scope::new(&fixture.shared);
        let barrier = TerminalBarrier::arm(&fixture.provider, fixture.channel.get());
        restore_tmux_watchers(&fixture.http, &fixture.shared).await;
        let handle = fixture.handle();
        let ticket = watcher_completion::observe(&handle.cancel).unwrap();
        until(&observed, "initial EOF before invalidation", |s| {
            s.event_count("outer_eof") > 0
        })
        .await;
        let bytes = transcript(&fixture.provider, true);
        append(&fixture, bytes.as_bytes());
        barrier.wait().await;
        match change {
            Invalidation::Source => {
                std::fs::rename(&fixture.output, format!("{}.opened", fixture.output)).unwrap();
                std::fs::write(&fixture.output, &bytes).unwrap();
            }
            Invalidation::NonRegular => {
                std::fs::rename(&fixture.output, format!("{}.opened", fixture.output)).unwrap();
                let output = std::ffi::CString::new(fixture.output.as_bytes()).unwrap();
                assert_eq!(
                    unsafe { libc::mkfifo(output.as_ptr(), 0o600) },
                    0,
                    "non-regular temporary output fixture must be a FIFO"
                );
            }
            Invalidation::Generation => {
                let generation = tc::session_temp_path(&fixture.session, "generation");
                std::fs::write(&generation, "changed-generation").unwrap();
                filetime::set_file_mtime(
                    generation,
                    filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() + 60, 0),
                )
                .unwrap();
            }
            Invalidation::Owner => {
                std::fs::write(tc::tmux_owner_path(&fixture.session), "different-owner").unwrap();
            }
            Invalidation::Reset => {
                let frontier = fixture.shared.tmux_relay_coord(fixture.channel);
                frontier.confirmed_end_offset.store(100, Ordering::Release);
                assert!(frontier.reset_confirmed_frontier(100, 0));
            }
        }
        barrier.release();
        let _ = tokio::time::timeout(Duration::from_secs(8), async {
            while !handle.cancel.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            handle.cancel.load(Ordering::Acquire),
            "{change:?}: old incarnation must cancel itself"
        );
        joined(ticket).await;
        assert_eq!(submitted.submissions(), 1);
        assert_eq!(submitted.join_all().await.len(), 1);
        let pending = crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
        assert_eq!(pending.len(), 1, "{change:?}: {pending:?}");
        assert!(pending[0].origin_is_retired());
        assert_eq!(pending[0].body, BODY);
        assert!(!pending[0].has_identity && !pending[0].has_startup_snapshot);
        assert_eq!(
            observed.snapshot().event_count("retired_terminal_consumed"),
            0
        );
        assert!(observed.snapshot().event_count("retired_terminal_retained") > 0);
        assert_zero_loaders(&observed);
        assert_eq!(test_support::tree_fingerprint(&rows), before);
        assert!(
            discord
                .requests_for(fixture.channel.get())
                .iter()
                .all(|request| !request.contains(BODY))
        );
    }
}

#[tokio::test]
async fn n4d2_terminal_temporary_source_busy_retries_then_consumes() {
    if !isolated(
        module_path!(),
        "n4d2_terminal_temporary_source_busy_retries_then_consumes",
    ) {
        return;
    }
    let (fixture, _discord) = fixture(ProviderKind::Claude, 632_520_730).await;
    let nonce = tc::session_temp_path(&fixture.session, "spawn_nonce");
    let saved_nonce = format!("{nonce}.n4d2-backup");
    let context = crate::services::tui_prompt_dedupe::binding_context::BindingContext {
        schema: 1,
        provider: fixture.provider.as_str().into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: fixture.session.clone(),
        channel_id: Some(fixture.channel.get()),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root().canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: None,
    };
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        context.clone(),
    )
    .unwrap();
    std::fs::write(&nonce, &context.execution_nonce).unwrap();
    assert_eq!(
        crate::services::tui_prompt_dedupe::binding_context::execution_context(
            fixture.provider.as_str(),
            &context.execution_nonce,
        )
        .unwrap(),
        context
    );
    let expected_witness = crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(&fixture.session);
    assert!(expected_witness.spawn_nonce_hash.is_some());
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let _owned = cutover::test_override::force_channels(&[(
        fixture.channel.get(),
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, fixture.channel.get());
    restore_tmux_watchers(&fixture.http, &fixture.shared).await;
    let handle = fixture.handle();
    until(&observed, "actual EOF", |s| s.event_count("outer_eof") > 0).await;
    append(&fixture, transcript(&fixture.provider, true).as_bytes());
    barrier.wait().await;
    let (held, entered) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let session = fixture.session.clone();
    let held_nonce = nonce.clone();
    let held_saved_nonce = saved_nonce.clone();
    let lock = std::thread::spawn(move || {
        tc::with_tmux_source_authority(&session, |_| {
            std::fs::rename(&held_nonce, &held_saved_nonce).unwrap();
            std::fs::create_dir(&held_nonce).unwrap();
            assert!(std::fs::read_to_string(&held_nonce).is_err());
            let unreadable = crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(&session);
            assert_eq!(unreadable.generation, expected_witness.generation);
            assert!(unreadable.spawn_nonce_hash.is_none());
            held.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        })
    });
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    barrier.release();
    until(&observed, "temporary source lock retained terminal", |s| {
        s.event_count("retired_terminal_retained") > 0
    })
    .await;
    assert!(!handle.cancel.load(Ordering::Acquire));
    assert_eq!(
        observed.snapshot().event_count("retired_terminal_consumed"),
        0
    );
    release.send(()).unwrap();
    lock.join().unwrap();
    let retained = observed.snapshot().event_count("retired_terminal_retained");
    until(
        &observed,
        "retry retains temporary nonce I/O failure",
        |s| {
            handle.cancel.load(Ordering::Acquire)
                || s.event_count("retired_terminal_retained") >= retained + 2
        },
    )
    .await;
    assert!(
        !handle.cancel.load(Ordering::Acquire),
        "nonce I/O must remain retryable"
    );
    assert!(observed.snapshot().event_count("retired_terminal_retained") >= retained + 2);
    assert_eq!(
        observed.snapshot().event_count("retired_terminal_consumed"),
        0
    );
    tc::with_tmux_source_authority(&fixture.session, |_| {
        std::fs::remove_dir(&nonce).unwrap();
        std::fs::rename(&saved_nonce, &nonce).unwrap();
    });
    assert_eq!(
        crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(&fixture.session),
        expected_witness
    );
    until(&observed, "retry consumes after nonce I/O recovery", |s| {
        s.event_count("retired_terminal_consumed") == 1
    })
    .await;
    fixture.cancel_and_join().await;
    assert_eq!(submitted.join_all().await.len(), 1);
    assert_zero_loaders(&observed);
}

#[tokio::test]
async fn n4d2_codex_terminal_consumes_and_independent_o_writer_receipts_once() {
    if !isolated(
        module_path!(),
        "n4d2_codex_terminal_consumes_and_independent_o_writer_receipts_once",
    ) {
        return;
    }
    let (fixture, discord) = fixture(ProviderKind::Codex, 632_520_740).await;
    let _retired = RetiredForTest::new("codex", fixture.channel.get());
    let _owned = cutover::test_override::force_channels(&[(
        fixture.channel.get(),
        RuntimeHandoffKind::CodexTui,
    )]);
    let writer = OWriter::new(&fixture);
    let pending_receipt = writer.hold_next_post();
    let (stop, actor) = writer.start();
    seed_legacy(&fixture);
    let rows = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&rows);
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, fixture.channel.get());
    restore_tmux_watchers(&fixture.http, &fixture.shared).await;
    until(&observed, "actual Codex EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let bytes = transcript(&fixture.provider, true);
    append(&fixture, bytes.as_bytes());
    barrier.wait().await;
    assert!(
        observed
            .snapshot()
            .parser_responses
            .iter()
            .any(|body| body == BODY)
    );
    barrier.release();
    tokio::time::timeout(Duration::from_secs(8), pending_receipt.entered())
        .await
        .unwrap();
    until(&observed, "Codex custody consumed before O receipt", |s| {
        s.event_count("retired_terminal_consumed") == 1
    })
    .await;
    assert!(writer.posts().is_empty() && writer.receipts().is_empty());
    pending_receipt.release();
    until(
        &observed,
        "Codex terminal consumed and O receipt committed",
        |s| {
            s.event_count("retired_terminal_consumed") == 1
                && writer.posts() == [BODY]
                && writer.receipts() == [BODY]
        },
    )
    .await;
    fixture.cancel_and_join().await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(8), actor)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(submitted.submissions(), 1);
    assert_eq!(submitted.join_all().await.len(), 1);
    assert_zero_loaders(&observed);
    assert_eq!(
        observed.snapshot().retired_terminal_ranges,
        [(0, bytes.len() as u64)]
    );
    assert_eq!(writer.posts(), [BODY]);
    assert_eq!(writer.receipts(), [BODY]);
    assert_eq!(test_support::tree_fingerprint(&rows), before);
    assert!(
        discord
            .requests_for(fixture.channel.get())
            .iter()
            .all(|request| !request.contains(BODY))
    );
}

#[test]
fn n4d2_dormant_census_counts_actual_production_callers() {
    let script = r#"
import re, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from scripts import check_durable_frontier_writer_call_sites as c
from scripts import check_legacy_supervision_census as b
root = Path(sys.argv[1])
all_files = c._SKIP_PIN._lexical_rust_files(root, Path('src'))
basename = {p for p in all_files if c.is_test_file(p.name)}
resolved = c._SKIP_PIN._INVENTORY.test_only_module_files(production_files=[p for p in all_files if p not in basename], all_files=all_files)
skips = basename | resolved
sources = {p.relative_to(root).as_posix(): c._production_text(p) for p in all_files}
errors = b.audit(sources, skips={p.relative_to(root).as_posix() for p in skips})
assert not errors, errors
definitions = callers = 0
for path, code in sources.items():
    if root / path in skips:
        continue
    for item in re.findall(r'\b(?:pub(?:\([^)]*\))?\s+)?use\s+[^;]+;', code):
        if 'restore_retired_o_watchers' in item:
            assert not re.search(r'\bas\b', item), f'{path}: unclassified boot API alias'
    code = re.sub(r'\b(?:pub(?:\([^)]*\))?\s+)?use\s+[^;]+;', '', code)
    definitions += len(re.findall(r'\bfn\s+restore_retired_o_watchers\b', code))
    code = re.sub(r'\bfn\s+restore_retired_o_watchers\b', 'fn DEFINITION', code)
    uses = list(re.finditer(r'\brestore_retired_o_watchers\b', code))
    callers += len(uses)
    assert not uses, f'{path}: owner-runtime boot API call or alias exists'
assert definitions == 1, definitions
assert callers == 0, callers
guards = [
    ('health/watcher_respawn/live_bridge_guard.rs', 'retry_pending_watcher_respawn', 'super::super::legacy_supervision::is_retired(provider.as_str(),channel_id.get())'),
    ('health/watcher_respawn/live_bridge_guard.rs', 'complete_force_clean_watcher_recovery', 'super::super::legacy_supervision::is_retired(provider.as_str(),channel_id.get())'),
    ('watchers/lifecycle/o_only_install.rs', 'install_retired_o_watcher', '!crate::services::discord::health::legacy_supervision::is_retired(request.provider.as_str(),request.channel_id.get())'),
    ('watchers/lifecycle/restore.rs', 'restore_tmux_watchers', 'crate::services::discord::health::legacy_supervision::is_retired(provider.as_str(),channel_id.get())'),
    ('watchers/lifecycle/o_only_restore.rs', 'restore_retired_o_watchers', '!crate::services::discord::health::legacy_supervision::is_retired(provider.as_str(),channel)'),
]
for suffix, function, expected in guards:
    body = b.function_text(sources['src/services/discord/' + suffix], function)
    conditions = [re.sub(r',\)', ')', re.sub(r'\s+', '', cond)) for cond in re.findall(r'\bif\s+([^{}]+)\{', body) if 'is_retired' in cond]
    assert conditions == [expected], (function, conditions)
print('canonical_retired_production_writers=0')
print('restore_retired_o_watchers_owner_runtime_boot_callers=0')
print('canonical_retired_condition_unchanged=true')
print('activation_ready=false')
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script, env!("CARGO_MANIFEST_DIR")])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("canonical_retired_production_writers=0"));
    assert!(report.contains("restore_retired_o_watchers_owner_runtime_boot_callers=0"));
    assert!(report.contains("canonical_retired_condition_unchanged=true"));
    assert!(report.contains("activation_ready=false"));
}
