//! Actor/store coverage uses real native files and strict P5 history with a controlled host observer.
//! Production legacy/Herdr discovery and sweep wiring are covered separately by health fixtures.
use super::*;
use crate::services::tmux_common::TmuxSourceAuthority;
use crate::services::tui_o::writer::actor::codex_retention::for_hop;
use crate::services::tui_o::writer::codex_snapshot::{
    Coverage, HostPath, Inputs, Observation as CodexObservation, Observer, Pane, PaneState, Target,
};
use crate::services::tui_o::writer::transition_port::Idle;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events as p5;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

const CURRENT: &str = "11111111111111111111111111111111";
const PRIOR: &str = "22222222222222222222222222222222";

struct ClearP5;
impl Drop for ClearP5 {
    fn drop(&mut self) {
        p5::forget_channel_for_tests(CHANNEL);
        p5::set_test_root(None);
    }
}

fn native(session: &str, number: usize) -> Vec<u8> {
    let at = (Utc::now() - TimeDelta::days(8)).to_rfc3339();
    let header = serde_json::json!({"type":"session_meta","timestamp":at,"payload":{
        "id":session,"cwd":"/work/project","source":"cli"
    }});
    let message = serde_json::json!({"type":"response_item","timestamp":at,"payload":{
        "type":"message","role":"assistant","id":format!("message-{number}"),
        "content":[{"type":"output_text","text":format!("old response {number}")}]
    }});
    [header, message].into_iter().flat_map(codex_line).collect()
}

fn native_chain() -> Chain {
    native_chain_with_old(1)
}

fn native_chain_with_old(old: usize) -> Chain {
    let (harness, path, source, bindings) = started(&row("init", "already delivered"));
    let pane = format!("hrot-ba-{}", uuid::Uuid::new_v4().simple());
    let (mut sources, mut paths) = (vec![source], vec![path.clone()]);
    for n in 1..=old + 1 {
        let session = format!("019a5f47-28b5-7000-8000-{n:012}");
        let name = format!("rollout-2026-10-16T00-00-00-{session}.jsonl");
        let (next_path, next) = transcript(&path, &name, &session, &native(&session, n));
        bindings.commit(hop_at(
            n as u64 + 1,
            &pane,
            sources.last(),
            &next,
            BindingCause::Startup,
            "",
        ));
        sources.push(next);
        paths.push(next_path);
    }
    for event in bindings.events.lock().unwrap().iter_mut() {
        event.provider = ShadowProvider::Codex;
        event.tmux_session = pane.clone();
        event.execution_nonce = if event.seq < sources.len() as u64 {
            PRIOR
        } else {
            CURRENT
        }
        .into();
    }
    Chain {
        harness,
        bindings,
        pane,
        sources,
        paths,
        _hosts: None,
    }
}

#[derive(Clone)]
struct HostObservation {
    pane: String,
    sources: Vec<SourceId>,
    created: DateTime<Utc>,
    herdr: bool,
    // Mutations fire after refresh captured slow discovery and before its borrowed-authority read.
    change: Arc<Mutex<Option<&'static str>>>,
    live_delta: Arc<Mutex<Option<&'static str>>>,
    refreshes: Arc<AtomicUsize>,
    p5_path: PathBuf,
}

impl HostObservation {
    fn new(chain: &Chain, herdr: bool, p5_path: PathBuf) -> Self {
        Self {
            pane: chain.pane.clone(),
            sources: chain.sources.clone(),
            created: Utc::now() - TimeDelta::days(1),
            herdr,
            change: Arc::default(),
            live_delta: Arc::default(),
            refreshes: Arc::default(),
            p5_path,
        }
    }

    fn inputs(&self) -> Result<Inputs, String> {
        let delta = *self.live_delta.lock().unwrap();
        let log = p5::codex::observe_channel(CHANNEL, 65_536)?;
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: self.created,
            execution_nonce: CURRENT.into(),
            tmux_session: self.pane.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
            host: Some("mac-mini".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: self.sources[0].path.parent().map(Path::to_path_buf),
            first_prompt_digest: Some(
                if delta == Some("context") {
                    "changed"
                } else {
                    "original"
                }
                .into(),
            ),
            source_policy: (!self.herdr).then(|| "shadow".into()),
        };
        Ok(Inputs {
            target: Target {
                channel: CHANNEL,
                tmux: self.pane.clone(),
                nonce: CURRENT.into(),
                host: "mac-mini".into(),
                owner_runtime_root: context.owner_runtime_root.clone(),
                host_path: if self.herdr {
                    HostPath::Herdr {
                        identity: "configured-host-v1".into(),
                    }
                } else {
                    HostPath::Legacy
                },
            },
            context,
            current: self.sources.last().unwrap().clone(),
            events: log.events,
            tail: log.tail,
            log_digest: log.digest,
            census_digest: if delta == Some("census") {
                "complete-census-v2"
            } else {
                "complete-census-v1"
            }
            .into(),
            panes: vec![
                Pane {
                    name: self.pane.clone(),
                    state: PaneState::Live,
                },
                Pane {
                    name: "other-dead-pane".into(),
                    state: PaneState::Dead,
                },
            ],
            coverage: Coverage::Ok(()),
        })
    }
}

impl Observer for HostObservation {
    fn observe(&self, _authority: &TmuxSourceAuthority<'_>) -> Result<Inputs, String> {
        self.inputs()
    }

    fn refresh(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Arc<dyn Observer>, String>> + Send + '_>,
    > {
        Box::pin(async move {
            self.inputs()?;
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            if let Some(change) = self.change.lock().unwrap().take() {
                if change == "tail" {
                    let log = p5::codex::observe_channel(CHANNEL, 65_536)?;
                    let mut event = log.events.last().unwrap().clone();
                    event.seq += 1;
                    event.tmux_session = "other-dead-pane".into();
                    event.old = None;
                    event.new = p5::BindingTarget::Rejected {
                        payload_session_id: "diagnostic".into(),
                        payload_transcript_path: None,
                        reason: "new full tail".into(),
                    };
                    append(
                        &self.p5_path,
                        &codex_line(serde_json::to_value(event).unwrap()),
                    );
                } else {
                    *self.live_delta.lock().unwrap() = Some(change);
                }
            }
            Ok(Arc::new(self.clone()) as Arc<dyn Observer>)
        })
    }
}

fn write_p5(chain: &Chain, path: &Path) {
    let events = chain
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| p5::BindingEvent {
            seq: index as u64 + 1,
            channel_id: CHANNEL,
            provider: "codex".into(),
            tmux_session: chain.pane.clone(),
            execution_nonce: Some(
                if index + 1 < chain.sources.len() {
                    PRIOR
                } else {
                    CURRENT
                }
                .into(),
            ),
            old: index.checked_sub(1).map(|old| chain.sources[old].clone()),
            new: p5::BindingTarget::Source(source.clone()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: Utc::now(),
            },
            committed_at: Utc::now(),
        });
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes: Vec<_> = events
        .flat_map(|event| codex_line(serde_json::to_value(event).unwrap()))
        .collect();
    std::fs::write(path, bytes).unwrap();
}

fn context(chain: &Chain, observer: &HostObservation) -> Context {
    Context {
        tmux: chain.pane.clone(),
        nonce: CURRENT.into(),
        lock: Arc::default(),
        codex: Some(Arc::new(observer.clone())),
    }
}

async fn targeted(chain: &Chain, run: &Run, observer: &HostObservation) -> (Request, ScopeDigest) {
    let mut request = chain.request("ba-native-target");
    request.termination = None;
    let discovery = ask(
        run,
        Operation::Inspect(request.clone()),
        context(chain, observer),
    )
    .await;
    let observation = discovery
        .diagnostics
        .as_ref()
        .unwrap()
        .codex
        .as_ref()
        .unwrap();
    let CodexObservation::Complete(snapshot) = observation else {
        panic!("complete fixture: {observation:?}")
    };
    let claim = for_hop(snapshot, &chain.key(1), &chain.sources[1], 3, &chain.pane).unwrap();
    request.termination = Some(Termination::Retention(Box::new(claim)));
    let inspected = ask(
        run,
        Operation::Inspect(request.clone()),
        context(chain, observer),
    )
    .await;
    assert!(
        matches!(
            inspected
                .hops
                .iter()
                .find(|hop| hop.judged.key == chain.key(1))
                .unwrap()
                .judged
                .verdict,
            Verdict::Eligible(_)
        ),
        "{:?}",
        inspected.hops
    );
    let digest = inspected.scope.unwrap().unwrap().digest;
    (request, digest)
}

async fn native_retention(changed: Option<&'static str>, herdr: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let root = crate::config::runtime_root().unwrap();
    p5::set_test_root(Some(&root));
    let _clear = ClearP5;
    let chain = native_chain();
    let path = root.join(format!("binding_events/{CHANNEL}.log"));
    write_p5(&chain, &path);
    let observer = HostObservation::new(&chain, herdr, path.clone());
    let run = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let (request, digest) = targeted(&chain, &run, &observer).await;
    let cursor_before = chain.cursor_file(1);
    let source_before = std::fs::read(&chain.paths[1]).unwrap();
    let p5_before = std::fs::read(&path).unwrap();
    *observer.change.lock().unwrap() = changed;
    let reply = ask(
        &run,
        Operation::AutomaticApply(
            request,
            digest,
            Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
        ),
        context(&chain, &observer),
    )
    .await;
    assert_eq!(
        observer.refreshes.load(Ordering::SeqCst),
        1,
        "one slow refresh for the one eligible candidate"
    );
    if let Some(changed) = changed {
        assert!(
            !chain.cursor(1).retired,
            "[{changed}:write_zero] {:?}",
            reply.hops
        );
        assert_eq!(
            chain.cursor_file(1),
            cursor_before,
            "[{changed}:cursor_bytes]"
        );
        let hop = reply
            .hops
            .iter()
            .find(|hop| hop.judged.key == chain.key(1))
            .unwrap();
        assert_eq!(
            hop.outcome,
            Outcome::Held(Hold::InspectScopeChanged),
            "[{changed}:specific_hold]"
        );
    } else {
        assert!(
            chain.cursor(1).retired,
            "[ba:actual_cursor_retired] {:?}",
            reply.hops
        );
        let audit = chain.cursor(1).transition_cleanup.unwrap();
        let evidence = audit.retention.unwrap();
        assert_eq!(evidence.min_age_secs, 604800);
        assert_eq!(evidence.claim.old, chain.sources[1]);
        assert_eq!(
            (
                evidence.claim.binding_seq,
                evidence.claim.binder_seq,
                evidence.claim.hop_seq
            ),
            (2, 2, 3)
        );
        assert_eq!(evidence.scope, digest.to_string());
        assert!(
            evidence
                .observed_at
                .signed_duration_since(evidence.last_dialogue)
                >= TimeDelta::days(7)
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            p5_before,
            "age/retirement never publish P5"
        );
    }
    assert!(
        !chain.cursor(0).retired,
        "unproven non-native backlog remains"
    );
    assert!(!chain.cursor(2).retired, "current native source remains");
    assert_eq!(
        std::fs::read(&chain.paths[1]).unwrap(),
        source_before,
        "native transcript is unchanged"
    );
    halt(run.stop, run.task).await;
}

#[tokio::test(start_paused = true)]
async fn ba_native_actor_retires_exact_aged_source_with_durable_retention_audit() {
    native_retention(None, false).await;
}

#[tokio::test(start_paused = true)]
async fn ba_configured_host_actor_retires_exact_aged_source_with_durable_retention_audit() {
    native_retention(None, true).await;
}

#[tokio::test(start_paused = true)]
async fn ba_refresh_to_final_context_change_writes_zero() {
    native_retention(Some("context"), false).await;
}

#[tokio::test(start_paused = true)]
async fn ba_refresh_to_final_full_tail_after_cutoff_change_writes_zero() {
    native_retention(Some("tail"), false).await;
}

#[tokio::test(start_paused = true)]
async fn ba_refresh_to_final_census_change_writes_zero() {
    native_retention(Some("census"), false).await;
}

#[path = "transition_actor_ba_backlog_tests.rs"]
mod backlog;

#[tokio::test(start_paused = true)]
async fn ba_actual_budget_not_found_retry_holds_before_due_then_posted_allows_retirement() {
    use crate::services::tui_o::writer::transition_cleanup::{Owed, Refusal};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let root = crate::config::runtime_root().unwrap();
    p5::set_test_root(Some(&root));
    let _clear = ClearP5;
    let chain = native_chain();
    let path = root.join(format!("binding_events/{CHANNEL}.log"));
    write_p5(&chain, &path);
    let observer = HostObservation::new(&chain, false, path);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let (request, digest) = targeted(&chain, &actor, &observer).await;
    halt(actor.stop, actor.task).await;
    let before = chain.harness.port.posts().len();
    let (held, applied) =
        crate::services::tui_o::writer::actor::exercise_local_retry_retention_for_tests(
            chain.harness.writer(),
            Arc::clone(&chain.bindings),
            || context(&chain, &observer),
            request,
            digest,
            piece("ba-retry", "owed BA retry"),
            || {
                chain.harness.port.say(BOT + 1, "history readable");
                chain
                    .harness
                    .port
                    .replies
                    .lock()
                    .unwrap()
                    .push_back(crate::services::tui_o::writer::tests::Reply::Unsent);
            },
        )
        .await;
    let candidate = held
        .hops
        .iter()
        .find(|hop| hop.judged.key == chain.key(1))
        .unwrap();
    assert_eq!(
        candidate.judged.verdict,
        Verdict::Refused(Refusal::OutputOwed(Owed::Queue))
    );
    assert!(
        chain.cursor(1).retired,
        "after actual retry Posted fresh Inspect/Apply retires source: {:?}",
        applied.hops
    );
    assert!(
        chain
            .cursor(1)
            .transition_cleanup
            .unwrap()
            .retention
            .is_some()
    );
    assert_eq!(
        chain.harness.port.posts().len(),
        before + 2,
        "exactly the uncertain POST and its existing single retry; retention adds none"
    );
}
fn regrowth_row(id: &str, body: &str) -> Vec<u8> {
    codex_line(
        serde_json::json!({"type":"response_item","timestamp":Utc::now().to_rfc3339(),"payload":{
            "type":"message","role":"assistant","id":id,"content":[{"type":"output_text","text":body}]
        }}),
    )
}

async fn native_regrowth(configured: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let root = crate::config::runtime_root().unwrap();
    p5::set_test_root(Some(&root));
    let _clear = ClearP5;
    let chain = native_chain();
    let path = root.join(format!("binding_events/{CHANNEL}.log"));
    write_p5(&chain, &path);
    let observer = HostObservation::new(&chain, configured, path);
    let readers = configured.then_some(ReaderConfig { dormant: false });
    let actor = run_writer(
        &chain,
        readers,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let (request, digest) = targeted(&chain, &actor, &observer).await;
    let job = request.job_id.clone();
    let retired = ask(
        &actor,
        Operation::AutomaticApply(
            request,
            digest,
            Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
        ),
        context(&chain, &observer),
    )
    .await;
    assert!(
        chain.cursor(1).retired,
        "actual BA retirement: {:?}",
        retired.hops
    );
    let old_cursor = chain.cursor(1);
    let old_bytes = std::fs::read(&chain.paths[1]).unwrap();
    assert_eq!(old_cursor.captured_through, old_bytes.len() as u64);
    assert!(
        old_cursor
            .transition_cleanup
            .as_ref()
            .unwrap()
            .retention
            .is_some()
    );
    let posted = chain.harness.port.posts().len();
    chain.harness.alarms.taken();
    append(
        &chain.paths[1],
        &regrowth_row("native-after-retire", "native regrew after retention"),
    );
    polls(4).await;
    assert!(
        !chain.cursor(1).retired,
        "regrowth un-retires native cursor"
    );
    assert_eq!(
        chain.audit(1).as_deref(),
        Some(job.as_str()),
        "retention audit survives unretire"
    );
    assert_eq!(
        chain.harness.port.posts()[posted..],
        ["native regrew after retention"],
        "capture resumes at previous exact cursor, without replaying old dialogue"
    );
    assert_eq!(
        grown(&chain.harness),
        [WriterAlarm::RetiredSourceGrew {
            source: chain.sources[1].clone(),
            cleanup_history: Some(job)
        }]
    );
    assert_eq!(
        std::fs::read(&chain.paths[1]).unwrap()[..old_bytes.len()],
        old_bytes
    );
    assert_eq!(
        chain.cursor(1).captured_through,
        std::fs::metadata(&chain.paths[1]).unwrap().len()
    );
    halt(actor.stop, actor.task).await;

    let posted = chain.harness.port.posts().len();
    let actor = run_writer(
        &chain,
        readers,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    polls(5).await;
    assert_eq!(
        chain.harness.port.posts().len(),
        posted,
        "restored ledger/cursor does not duplicate regrowth delivery"
    );
    append(
        &chain.paths[1],
        &regrowth_row("native-after-restore", "native appended after restore"),
    );
    polls(4).await;
    assert_eq!(
        chain.harness.port.posts()[posted..],
        ["native appended after restore"],
        "restored actor reads only newly appended native bytes once"
    );
    assert_eq!(
        chain.cursor(1).captured_through,
        std::fs::metadata(&chain.paths[1]).unwrap().len()
    );
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn ba_legacy_native_regrowth_unretires_and_restores_without_duplicate_delivery() {
    native_regrowth(false).await;
}

#[tokio::test(start_paused = true)]
async fn ba_configured_native_regrowth_unretires_and_restores_without_duplicate_delivery() {
    native_regrowth(true).await;
}

#[tokio::test(start_paused = true)]
async fn ba_unknown_native_dialogue_age_never_becomes_retired() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let root = crate::config::runtime_root().unwrap();
    p5::set_test_root(Some(&root));
    let _clear = ClearP5;
    let chain = native_chain();
    append(
        &chain.paths[1],
        &codex_line(serde_json::json!({
            "type":"future_dialogue_envelope", "timestamp":Utc::now().to_rfc3339(),
            "payload":{"message":"a newer dialogue shape is unknown"}
        })),
    );
    let path = root.join(format!("binding_events/{CHANNEL}.log"));
    write_p5(&chain, &path);
    let observer = HostObservation::new(&chain, false, path);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let (request, digest) = targeted(&chain, &actor, &observer).await;
    let before = chain.cursor_file(1);
    let reply = ask(
        &actor,
        Operation::AutomaticApply(
            request,
            digest,
            Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
        ),
        context(&chain, &observer),
    )
    .await;
    assert!(
        !chain.cursor(1).retired,
        "[ba:unknown_age] unknown dialogue cannot authorize retirement"
    );
    assert_eq!(
        chain.cursor_file(1),
        before,
        "unknown age writes no cursor or audit"
    );
    let target = reply
        .hops
        .iter()
        .find(|hop| hop.judged.key == chain.key(1))
        .unwrap();
    assert_eq!(
        target.outcome,
        Outcome::Held(Hold::Policy(
            crate::services::tui_o::writer::transition_port::PolicyHold::AgeUnknown
        ))
    );
    halt(actor.stop, actor.task).await;
}
