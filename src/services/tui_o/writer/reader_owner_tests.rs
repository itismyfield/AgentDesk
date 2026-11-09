//! Reader counting after a later execution in the same pane replaced the source's writer.

use super::*;

/// Hops through `ids`, gives each event the pane and execution nonce `run` names for its seq,
/// and reads every source to its end.
fn executions(
    harness: &Harness,
    bindings: &Arc<FakeBindings>,
    ids: &[SourceId],
    run: impl Fn(u64) -> (&'static str, String),
) {
    for (seq, pair) in (2..).zip(ids.windows(2)) {
        bindings.commit(hop(seq, &pair[0], &pair[1]));
    }
    for event in bindings.events.lock().unwrap().iter_mut() {
        let (pane, nonce) = run(event.seq);
        (event.tmux_session, event.execution_nonce) = (pane.into(), nonce);
    }
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut sources, mut writer, mut deriver, _) = resume(harness, bindings.clone());
    let captured = sources.capture(&mut writer, &mut deriver, &mut VecDeque::new());
    captured.unwrap();
}

fn restarted<B: BindingEvents>(
    harness: &Harness,
    bindings: Arc<B>,
) -> (Sources<B>, HealthyWriter, UnitDeriver) {
    restarted_as(harness, bindings, ShadowProvider::Claude)
}

fn restarted_as<B: BindingEvents>(
    harness: &Harness,
    bindings: Arc<B>,
    provider: ShadowProvider,
) -> (Sources<B>, HealthyWriter, UnitDeriver) {
    let (mut writer, health, counts) = health_writer(harness);
    let mut sources = Sources::new(harness.channel_id, provider, bindings);
    let mut deriver = UnitDeriver::new(harness.channel_id, provider);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    (sources, (writer, health, counts), deriver)
}

/// The published count and alarm after a restart, with every source left unretired.
fn restart_count(run: impl Fn(u64) -> (&'static str, String)) -> (usize, bool) {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, run);
    let (_, (mut writer, health, counts), _) = restarted(&harness, bindings);
    let alarmed = active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}"));
    let store = writer.store();
    assert!(ids.iter().all(|s| !store.cursor(s).unwrap().retired));
    (*counts.lock().unwrap().last().unwrap(), alarmed)
}

const LAST_HOP: u64 = MAX_READERS as u64 + 2;

#[test]
fn restart_counts_only_sources_an_unreplaced_execution_may_still_write() {
    let each_hop_new = restart_count(|seq| ("tmux", format!("execution-{seq}")));
    assert_eq!(each_hop_new, (1, false), "each hop from a new execution");
    let one_execution = restart_count(|_| ("tmux", "one".into()));
    assert_eq!(one_execution, (MAX_READERS + 2, true), "one execution");
    let other_pane = restart_count(|seq| {
        (
            if seq < LAST_HOP { "other" } else { "tmux" },
            format!("e{seq}"),
        )
    });
    assert_eq!(other_pane, (2, false), "each pane keeps its latest writer");
}

#[test]
fn other_pane_tail_does_not_hide_replacement_in_the_original_pane() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, |seq| {
        ("tmux", format!("execution-{seq}"))
    });
    let mut other = hop(LAST_HOP + 1, &ids[0], &ids[0]);
    other.tmux_session = "other".into();
    other.execution_nonce = "other-execution".into();
    other.record = BindingRecord::Rejected {
        detail: "another pane".into(),
        closes: None,
    };
    bindings.commit(other);
    let (_, (mut writer, health, counts), _) = restarted(&harness, bindings);
    let alarmed = active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}"));
    assert_eq!(
        (*counts.lock().unwrap().last().unwrap(), alarmed),
        (1, false)
    );
    assert!(
        ids.iter()
            .all(|s| !writer.store().cursor(s).unwrap().retired)
    );
}

#[test]
fn prior_execution_chain_leaves_every_replaced_source_uncounted() {
    let nonce = |seq| if seq < LAST_HOP { "prior" } else { "current" };
    let chain = restart_count(|seq| ("tmux", nonce(seq).into()));
    assert_eq!(chain, (1, false));
}

#[test]
fn replaced_source_that_grows_counts_until_read_again() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, |seq| {
        ("tmux", format!("execution-{seq}"))
    });
    let (mut sources, (mut writer, health, counts), mut deriver) = restarted(&harness, bindings);
    for source in &ids[..MAX_READERS] {
        append(&source.path, &row("late", "late write"));
    }
    sources.tend(&mut writer).unwrap();
    let reason = format!("tui_o:too_many_readers:{CHANNEL}");
    assert_eq!(counts.lock().unwrap().last(), Some(&(MAX_READERS + 1)));
    assert!(active(&health).contains(&reason));
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&1));
    assert!(!active(&health).contains(&reason));
}

#[test]
fn unreadable_binding_log_counts_every_reader_until_read_again() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, |seq| {
        ("tmux", format!("execution-{seq}"))
    });
    let (mut sources, (mut writer, health, counts), _) = restarted(&harness, bindings.clone());
    let reason = format!("tui_o:too_many_readers:{CHANNEL}");
    let mut later = hop(LAST_HOP + 1, &ids[0], &ids[0]);
    later.record = BindingRecord::Rejected {
        detail: "held".into(),
        closes: None,
    };
    (later.tmux_session, later.execution_nonce) = ("tmux".into(), format!("execution-{LAST_HOP}"));
    bindings.commit(later);
    bindings.fail(Some("log unreadable"));
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&ids.len()));
    assert!(active(&health).contains(&reason));
    bindings.fail(None);
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&1));
    assert!(!active(&health).contains(&reason));
}

/// A log that cannot be watched, as a failed subscription leaves it.
struct Unwatched(Arc<FakeBindings>);

impl BindingEvents for Unwatched {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        self.0.binding_events_since(channel, after)
    }

    fn subscribe(&self, _channel: u64) -> tokio::sync::watch::Receiver<u64> {
        tokio::sync::watch::channel(u64::MAX).1
    }
}

#[test]
fn unwatched_log_judges_again_after_a_rebind_that_adds_no_hop() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, |_| ("tmux", "a".into()));
    let unwatched = Arc::new(Unwatched(bindings.clone()));
    let (mut sources, (mut writer, health, counts), _) = restarted(&harness, unwatched);
    let reason = format!("tui_o:too_many_readers:{CHANNEL}");
    assert_eq!(counts.lock().unwrap().last(), Some(&ids.len()));
    assert!(active(&health).contains(&reason));
    let current = BindingTarget::Source(ids.last().unwrap().clone());
    let mut rebind = bound(LAST_HOP + 1, None, current, BindingCause::Resume, None);
    (rebind.tmux_session, rebind.execution_nonce) = ("tmux".into(), "b".into());
    bindings.commit(rebind);
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&1));
    assert!(!active(&health).contains(&reason));
}

/// A verified Codex pane where execution `n` bound only its own rollout, each under its own nonce,
/// so the current policy holds every earlier execution's bind.
#[cfg(unix)]
#[test]
fn verified_restart_leaves_out_sources_earlier_executions_bound_in_the_pane() {
    use crate::services::tmux_common as tc;
    use crate::services::tui_o::store::rotation::{Rotation, Successor};
    use crate::services::tui_o::writer::binding::ChannelBindingLog;
    use crate::services::tui_prompt_dedupe::binding_context::{
        self as contexts, BindingContext, PreparedIncarnation,
    };
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
    use p5::codex::{Claim, ClaimEvidence, Decision};
    const VERIFIED: u64 = 794_901;
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            p5::forget_channel_for_tests(VERIFIED);
            p5::set_test_root(None);
            dedupe::reset_state_for_tests();
        }
    }
    let _shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = contexts::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    p5::set_test_root(Some(root.path()));
    let _clear = Clear;
    let pane = format!("reader-count-{}", uuid::Uuid::new_v4().simple());
    dedupe::register_tmux_channel(&pane, VERIFIED);
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let sessions = sessions.canonicalize().unwrap();
    let ids: Vec<SourceId> = (0..=MAX_READERS + 1)
        .map(|n| {
            let id = format!("019e660d-4859-7522-9cee-{n:012}");
            let path = sessions.join(format!("rollout-{id}.jsonl"));
            std::fs::write(&path, b"").unwrap();
            source_id_for(&id, &path).unwrap()
        })
        .collect();
    let harness = Harness::build_channel(VERIFIED, |_| {
        let empty = hex::encode(sha2::Sha256::digest(b""));
        (ids.iter().cloned())
            .map(|source_id| InitSource {
                source_id,
                delivery_start: 0,
                prefix_hash: empty.clone(),
            })
            .collect()
    });
    harness.gate.acquired();
    for source in &ids {
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: pane.clone(),
            channel_id: Some(VERIFIED),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.clone()),
            first_prompt_digest: None,
            source_policy: Some("verified".into()),
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        std::fs::write(
            tc::session_temp_path(&pane, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        let claim = Claim {
            session_id: source.session_id.clone(),
            path: Some(source.path.clone()),
            evidence: ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("startup".into()),
                first_prompt_digest: None,
            },
        };
        let verified = Decision::Verified(source.clone());
        tc::with_tmux_source_authority(&pane, |_| {
            p5::codex::commit_claim(&context, &claim, verified, || Ok(())).unwrap()
        });
    }
    let log = Arc::new(ChannelBindingLog::new(VERIFIED, ShadowProvider::Codex));
    let delivered = log.binding_events_since(VERIFIED, 0).unwrap();
    let held = (delivered.iter())
        .filter(|e| matches!(e.record, BindingRecord::Rejected { .. }))
        .count();
    assert_eq!(
        (delivered.len(), held),
        (ids.len(), ids.len() - 1),
        "{delivered:#?}"
    );
    let mut rotation = Rotation::default();
    for (seq, pair) in (2..).zip(ids.windows(2)) {
        let next = Successor {
            seq: Some(seq),
            tmux_session: Some(pane.clone()),
            ..Successor::from(pair[1].clone())
        };
        rotation.successors.insert(source_key(&pair[0]), next);
    }
    let mut store = harness.channel();
    store.write_rotation(&rotation).unwrap();
    store.set_binding_checkpoint(ids.len() as u64).unwrap();
    let (_, (_, health, counts), _) = restarted_as(&harness, log, ShadowProvider::Codex);
    assert_eq!(counts.lock().unwrap().last(), Some(&1));
    let reason = format!("tui_o:too_many_readers:{VERIFIED}");
    assert!(!active(&health).contains(&reason));
}
