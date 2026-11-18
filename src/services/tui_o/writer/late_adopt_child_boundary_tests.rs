//! Exercise lifecycle boundaries through the actual managed host, retaining its generation.
use super::*;
use crate::services::tui_o::channel_policy;
use crate::services::tui_o::writer::adoption::{LegacyEpoch, LegacyTurnIdentity};
use std::sync::atomic::AtomicBool;

struct Probe {
    delivered: Arc<Mutex<Vec<String>>>,
    initial: PathBuf,
    path: Mutex<PathBuf>,
    frontier: AtomicU64,
    live: AtomicBool,
    turn: Mutex<Option<LegacyTurnIdentity>>,
    bad_epoch: AtomicBool,
    reset: AtomicU64,
}
impl LegacyView for Probe {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, _: &str) -> LegacyCursor {
        if !self.live.load(Ordering::SeqCst) {
            return LegacyCursor::NoPane;
        }
        let path = self.path.lock().unwrap().clone();
        LegacyCursor::Bound {
            offset: std::fs::metadata(&path).unwrap().len(),
            path,
        }
    }
    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        Ok(self.cursor(tmux))
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier.load(Ordering::SeqCst))
    }
    fn historical_frontier(
        &self,
        binding: &BindingEvent,
        _: Option<&LegacyTurnIdentity>,
        _: Option<&crate::services::tui_o::writer::adoption::LegacyDelivery>,
        eof: u64,
    ) -> Option<u64> {
        let source = crate::services::tui_o::writer::rotation::bound_source(binding)?;
        (source.path == self.initial
            && binding.execution_nonce == "binding-host"
            && !self.delivered.lock().unwrap().is_empty())
        .then_some(eof)
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
    fn epoch(&self, _: u64) -> LegacyEpoch {
        LegacyEpoch {
            turn: self.turn.lock().unwrap().clone(),
            reset_incarnation: self.reset.load(Ordering::SeqCst),
            ..Default::default()
        }
    }
    fn late_epoch(&self, channel: u64) -> Result<LegacyEpoch, String> {
        if self.bad_epoch.load(Ordering::SeqCst) {
            Err("fixture epoch unavailable".into())
        } else {
            Ok(self.epoch(channel))
        }
    }
}
fn running(f: &Fixture) -> (Arc<Probe>, PathBuf) {
    let next = f.harness._runtime.path().join("running.jsonl");
    std::fs::write(&next, row("running", "current answer")).unwrap();
    let probe = Arc::new(Probe {
        delivered: f.old.clone(),
        initial: next.clone(),
        path: Mutex::new(next.clone()),
        frontier: AtomicU64::new(0),
        live: AtomicBool::new(true),
        turn: Mutex::new(Some((
            1,
            "fixture-start".into(),
            Some("binding-host".into()),
            Some(0),
            Some("fixture-turn".into()),
        ))),
        bad_epoch: AtomicBool::new(false),
        reset: AtomicU64::new(0),
    });
    *f.io.base.legacy.lock().unwrap() = Some(probe.clone());
    *f.io.base.custody.lock().unwrap() = Ok(Custody::Row);
    f.io.base.busy.store(true, Ordering::SeqCst);
    f.io.log.replace(vec![
        bound(1, source_id_for("prior", &f.path).unwrap()),
        bound(2, source_id_for("running", &next).unwrap()),
        audit(3),
    ]);
    (probe, next)
}
fn start_child(f: &Fixture) -> ManagedWriterHandle {
    let mut handles = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership()
            .into_iter()
            .filter(|r| r.0 == CHANNEL)
            .collect(),
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert_eq!(handles.len(), 1);
    handles.pop().unwrap()
}
async fn released(f: &Fixture, candidate: &channel_policy::Candidate) {
    spin().await;
    f.io.base.busy.store(false, Ordering::SeqCst);
    for _ in 0..12 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if candidate.peek() == Adoption::Released {
            break;
        }
    }
    assert_eq!(candidate.peek(), Adoption::Released);
}
async fn legacy_once(f: &Fixture) {
    assert_eq!(
        claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
            f.old.lock().unwrap().push("current answer".into());
        })
        .await,
        Ok(BodySend::Sent(()))
    );
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    assert!(f.harness.port.posts().is_empty());
}
async fn ready(f: &Fixture) {
    assert!(
        drove_until(|| f.ready.accepts(CHANNEL)).await,
        "boundary did not activate O: {:?}",
        f.io.base.alarms.0.lock().unwrap()
    );
}

async fn boundary_case(case: &str) {
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, path) = running(&f);
    let handle = start_child(&f);
    released(&f, &candidate).await;
    let run = candidate
        .legacy_run()
        .expect("the exact refused run must be frozen");
    assert_eq!(run.binding.execution_nonce, "binding-host");
    assert_eq!(run.turn, probe.epoch(CHANNEL).turn);
    legacy_once(&f).await;
    // Merely changing reset observations cannot end the frozen source/turn authority.
    probe.reset.store(1, Ordering::SeqCst);
    spin().await;
    assert_eq!(candidate.peek(), Adoption::Released);
    append(&path, &closed());
    let end = std::fs::metadata(&path).unwrap().len();
    let current = match case {
        "turn" => {
            *probe.turn.lock().unwrap() = None;
            f.io.base.busy.store(true, Ordering::SeqCst);
            path.clone()
        }
        "session" => {
            probe.live.store(false, Ordering::SeqCst);
            path.clone()
        }
        "bind" => {
            let next = f.harness._runtime.path().join("next-run.jsonl");
            std::fs::write(
                &next,
                b"{\"type\":\"user\",\"message\":{\"content\":\"next\"}}\n",
            )
            .unwrap();
            *probe.path.lock().unwrap() = next.clone();
            let mut event = bound(4, source_id_for("next-run", &next).unwrap());
            event.execution_nonce = "next-execution".into();
            let mut events = f.io.log.events.lock().unwrap().clone();
            events.push(event);
            f.io.log.replace(events);
            *probe.turn.lock().unwrap() = Some((
                2,
                "next-start".into(),
                Some("binding-host".into()),
                Some(0),
                Some("next-turn".into()),
            ));
            f.io.base.busy.store(true, Ordering::SeqCst);
            next
        }
        _ => unreachable!(),
    };
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if candidate.peek() == Adoption::Deferred {
            break;
        }
    }
    assert_eq!(
        candidate.peek(),
        Adoption::Deferred,
        "{case} must retry even before the next source is deliverable"
    );
    assert!(f.harness.port.posts().is_empty());
    assert_eq!(f.ready.hosted_by(CHANNEL), Some(handle.generation()));
    if case == "bind" {
        append(&current, &closed());
    }
    probe.frontier.store(
        if case == "bind" {
            std::fs::metadata(&current).unwrap().len()
        } else {
            end
        },
        Ordering::SeqCst,
    );
    f.io.base.busy.store(false, Ordering::SeqCst);
    ready(&f).await;
    append(&current, &row("after-boundary", "next O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["next O answer"]);
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    handle.stop_and_join().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn childheld_turn_end_retries_the_same_host() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_turn_end_retries_the_same_host"
    )) {
        return;
    }
    boundary_case("turn").await;
}
#[tokio::test(start_paused = true)]
async fn childheld_session_end_retries_without_inventing_a_delivery() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_session_end_retries_without_inventing_a_delivery"
    )) {
        return;
    }
    boundary_case("session").await;
}
#[tokio::test(start_paused = true)]
async fn childheld_new_execution_bind_retries_while_the_next_run_is_busy() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_new_execution_bind_retries_while_the_next_run_is_busy"
    )) {
        return;
    }
    boundary_case("bind").await;
}

#[tokio::test(start_paused = true)]
async fn childheld_restart_busy_session_normalizes_again_without_o_claims() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_restart_busy_session_normalizes_again_without_o_claims"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_first, old) = super::childheld::child_boot(&f);
    let (probe, path) = running(&f);
    let handle = start_child(&f);
    released(&f, &old).await;
    handle.stop_and_join().await.unwrap();
    let (_restart, new) = super::childheld::child_boot(&f);
    assert_eq!(new.peek(), Adoption::Deferred);
    f.io.base.busy.store(true, Ordering::SeqCst);
    let handle = start_child(&f);
    released(&f, &new).await;
    legacy_once(&f).await;
    append(&path, &closed());
    probe
        .frontier
        .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
    ready(&f).await;
    assert_eq!(new.peek(), Adoption::Committed);
    assert!(f.harness.port.posts().is_empty());
    handle.stop_and_join().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn childheld_unknown_run_authority_preserves_existing_hold() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_unknown_run_authority_preserves_existing_hold"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, _) = running(&f);
    probe.bad_epoch.store(true, Ordering::SeqCst);
    let handle = start_child(&f);
    released(&f, &candidate).await;
    assert!(candidate.legacy_run().is_none());
    assert_eq!(
        claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
            panic!("unknown authority cannot post");
        })
        .await,
        Err(cutover::IdentityError::ChildHeld)
    );
    assert!(
        cutover::boot_ownership()
            .into_iter()
            .find(|r| r.0 == CHANNEL)
            .unwrap()
            .2
            .is_some()
    );
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn childheld_boundary_survives_managed_host_stop_and_only_then_reoffers_boot() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_boundary_survives_managed_host_stop_and_only_then_reoffers_boot"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, path) = running(&f);
    let handle = start_child(&f);
    released(&f, &candidate).await;
    handle.stop_and_join().await.unwrap();
    assert!(!f.ready.is_hosted(CHANNEL));
    assert!(
        cutover::boot_ownership()
            .into_iter()
            .find(|r| r.0 == CHANNEL)
            .unwrap()
            .2
            .is_none()
    );
    legacy_once(&f).await;
    append(&path, &closed());
    probe
        .frontier
        .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
    for _ in 0..24 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if candidate.peek() == Adoption::Deferred {
            break;
        }
    }
    assert_eq!(
        candidate.peek(),
        Adoption::Deferred,
        "the run observer outlives its stopped host"
    );
    let handle = start_child(&f);
    ready(&f).await;
    append(&path, &row("after-reoffer", "new O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["new O answer"]);
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn childheld_same_execution_source_rotation_waits_for_its_own_turn_end() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_same_execution_source_rotation_waits_for_its_own_turn_end"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, _) = running(&f);
    let handle = start_child(&f);
    released(&f, &candidate).await;
    let next = f.harness._runtime.path().join("compacted.jsonl");
    std::fs::write(&next, row("compacted", "current answer")).unwrap();
    *probe.path.lock().unwrap() = next.clone();
    let mut events = f.io.log.events.lock().unwrap().clone();
    events.push(bound(4, source_id_for("compacted", &next).unwrap()));
    f.io.log.replace(events);
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
    }
    assert_eq!(candidate.peek(), Adoption::Released);
    assert!(!f.ready.accepts(CHANNEL));
    legacy_once(&f).await;
    append(&next, &closed());
    probe
        .frontier
        .store(std::fs::metadata(&next).unwrap().len(), Ordering::SeqCst);
    ready(&f).await;
    append(&next, &row("after-compaction", "next O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["next O answer"]);
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn childheld_already_closed_answer_retries_after_legacy_delivery_without_new_bytes() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_already_closed_answer_retries_after_legacy_delivery_without_new_bytes"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, path) = running(&f);
    append(&path, &closed());
    let end = std::fs::metadata(&path).unwrap().len();
    let handle = start_child(&f);
    released(&f, &candidate).await;
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
    }
    assert_eq!(candidate.peek(), Adoption::Released);
    legacy_once(&f).await;
    probe.frontier.store(end, Ordering::SeqCst);
    ready(&f).await;
    append(&path, &row("after-delivery", "next O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["next O answer"]);
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    handle.stop_and_join().await.unwrap();
}

struct DebtProbe {
    base: Arc<Probe>,
    old: PathBuf,
    ack: AtomicU64,
}
impl LegacyView for DebtProbe {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, tmux: &str) -> LegacyCursor {
        self.base.cursor(tmux)
    }
    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        self.base.late_cursor(tmux)
    }
    fn frontier(&self, channel: u64, tmux: &str, eof: u64) -> Option<u64> {
        self.base.frontier(channel, tmux, eof)
    }
    fn historical_frontier(
        &self,
        binding: &BindingEvent,
        _: Option<&LegacyTurnIdentity>,
        _: Option<&crate::services::tui_o::writer::adoption::LegacyDelivery>,
        _eof: u64,
    ) -> Option<u64> {
        let source = crate::services::tui_o::writer::rotation::bound_source(binding)?;
        (source.path == self.old
            && binding.execution_nonce == "binding-host"
            && self.ack.load(Ordering::SeqCst) != 0)
            .then_some(self.ack.load(Ordering::SeqCst))
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
    fn epoch(&self, channel: u64) -> LegacyEpoch {
        self.base.epoch(channel)
    }
}
async fn drove_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !done() && std::time::Instant::now() < deadline {
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        spin().await;
    }
    done()
}
async fn drive_until(done: impl FnMut() -> bool) {
    assert!(drove_until(done).await, "test completion was not observed");
}
async fn no_init(f: &Fixture) {
    let reads = f.io.log.reads.load(Ordering::SeqCst);
    drive_until(|| f.ready.accepts(CHANNEL) || f.io.log.reads.load(Ordering::SeqCst) >= reads + 40)
        .await;
    assert!(!f.ready.accepts(CHANNEL), "B cannot settle A's body");
    assert!(
        !init_path(&f.harness, CHANNEL).exists(),
        "A still owes its body"
    );
    assert!(f.harness.port.posts().is_empty());
}
async fn debt_case(boundary: &str) {
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, old) = running(&f);
    let silent = boundary == "empty";
    if silent {
        std::fs::write(&old, b"{\"type\":\"file-history-snapshot\"}\n").unwrap();
    }
    let debt = Arc::new(DebtProbe {
        base: probe.clone(),
        old: old.clone(),
        ack: AtomicU64::new(0),
    });
    *f.io.base.legacy.lock().unwrap() = Some(debt.clone());
    let handle = start_child(&f);
    released(&f, &candidate).await;
    append(&old, &closed());
    let next = f.harness._runtime.path().join("new-execution.jsonl");
    let fresh = boundary != "bind";
    let injected = Arc::new(AtomicBool::new(false));
    if boundary == "fence" {
        let path = old.clone();
        let signal = injected.clone();
        test_hook::set(CHANNEL, Step::AfterFence, move || {
            append(&path, &row("late-fragment", "late fragment"));
            signal.store(true, Ordering::SeqCst);
            Ok(())
        });
    }
    if fresh {
        std::fs::write(&next, b"{\"type\":\"user\",\"sessionId\":\"next\",\"uuid\":\"prompt\",\"message\":{\"content\":\"next\"}}\n").unwrap();
    } else {
        std::fs::copy(&old, &next).unwrap();
    }
    let mut event = bound(4, source_id_for("next", &next).unwrap());
    event.execution_nonce = "new-execution".into();
    let mut events = f.io.log.events.lock().unwrap().clone();
    events.push(event);
    if fresh {
        let log = Arc::downgrade(&f.io.log);
        let (state, current, path) = (candidate.clone(), probe.clone(), next.clone());
        *f.io.log.on_read.lock().unwrap() = Some(Box::new(move || {
            if state.peek() != Adoption::Deferred {
                return false;
            }
            *current.path.lock().unwrap() = path.clone();
            current
                .frontier
                .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
            log.upgrade().unwrap().replace(events.clone());
            true
        }));
        if boundary != "session" {
            *probe.turn.lock().unwrap() = None;
        } else {
            probe.live.store(false, Ordering::SeqCst);
        }
    } else {
        *probe.path.lock().unwrap() = next.clone();
        probe
            .frontier
            .store(std::fs::metadata(&next).unwrap().len(), Ordering::SeqCst);
        f.io.log.replace(events);
    }
    drive_until(|| candidate.peek() != Adoption::Released).await;
    if !silent {
        no_init(&f).await;
        legacy_once(&f).await;
        debt.ack.store(
            std::fs::metadata(&debt.old).unwrap().len(),
            Ordering::SeqCst,
        );
    }
    if boundary == "fence" {
        drive_until(|| injected.load(Ordering::SeqCst)).await;
        no_init(&f).await;
        assert_eq!(
            claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
                f.old.lock().unwrap()[0].push_str(" + late fragment");
            })
            .await,
            Ok(BodySend::Sent(()))
        );
        debt.ack.store(
            std::fs::metadata(&debt.old).unwrap().len(),
            Ordering::SeqCst,
        );
    }
    drive_until(|| f.ready.accepts(CHANNEL)).await;
    let expected = if boundary == "fence" {
        "current answer + late fragment"
    } else {
        "current answer"
    };
    if silent {
        assert!(f.old.lock().unwrap().is_empty());
    } else {
        assert_eq!(*f.old.lock().unwrap(), [expected]);
    }
    append(&next, &row("after-ack", "O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["O answer"]);
    handle.stop_and_join().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn ch3fix_new_bind_keeps_a_owed_until_its_own_ack() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_new_bind_keeps_a_owed_until_its_own_ack"
    )) {
        return;
    }
    debt_case("bind").await;
}
#[tokio::test(start_paused = true)]
async fn ch3fix_turn_boundary_fresh_takeover_keeps_a_owed() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_turn_boundary_fresh_takeover_keeps_a_owed"
    )) {
        return;
    }
    debt_case("turn").await;
}
#[tokio::test(start_paused = true)]
async fn ch3fix_session_boundary_fresh_takeover_keeps_a_owed() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_session_boundary_fresh_takeover_keeps_a_owed"
    )) {
        return;
    }
    debt_case("session").await;
}
#[tokio::test(start_paused = true)]
async fn ch3fix_pending_does_not_hide_frozen_pane_end() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_pending_does_not_hide_frozen_pane_end"
    )) {
        return;
    }
    pending_case(false).await;
}

#[tokio::test(start_paused = true)]
async fn ch3fix_pending_then_fresh_b_waits_for_a_ack() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_pending_then_fresh_b_waits_for_a_ack"
    )) {
        return;
    }
    pending_case(true).await;
}
async fn pending_case(fresh: bool) {
    let f = Fixture::new(false);
    let (_boot, candidate) = super::childheld::child_boot(&f);
    let (probe, old) = running(&f);
    let debt = Arc::new(DebtProbe {
        base: probe.clone(),
        old: old.clone(),
        ack: AtomicU64::new(0),
    });
    *f.io.base.legacy.lock().unwrap() = Some(debt.clone());
    let handle = start_child(&f);
    released(&f, &candidate).await;
    append(&old, &closed());
    let mut events = f.io.log.events.lock().unwrap().clone();
    let mut waiting = bound(4, source_id_for("running", &old).unwrap());
    waiting.record = BindingRecord::Bound {
        old: None,
        new: BindingTarget::Pending {
            payload_session_id: "pending".into(),
            payload_transcript_path: f.harness._runtime.path().join("pending.jsonl"),
        },
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: "SessionStart".into(),
            received_at: Utc::now(),
            reclaims: false,
        },
    };
    waiting.execution_nonce = "pending-execution".into();
    events.push(waiting);
    f.io.log.replace(events);
    probe.live.store(false, Ordering::SeqCst);
    drive_until(|| candidate.peek() != Adoption::Released).await;
    assert_eq!(candidate.peek(), Adoption::Deferred);
    no_init(&f).await;
    let path = f.harness._runtime.path().join("pending.jsonl");
    if fresh {
        std::fs::write(&path, b"{\"type\":\"user\",\"sessionId\":\"pending\",\"uuid\":\"prompt-b\",\"message\":{\"content\":\"prompt\"}}\n").unwrap();
    } else {
        std::fs::write(&path, row("pending", "B answer")).unwrap();
        append(&path, &closed());
    }
    *probe.path.lock().unwrap() = path.clone();
    probe
        .frontier
        .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
    let mut events = f.io.log.events.lock().unwrap().clone();
    let mut resolved = event(
        5,
        BindingRecord::Resolved {
            resolves_seq: 4,
            source: source_id_for("pending", &path).unwrap(),
        },
    );
    resolved.execution_nonce = "pending-execution".into();
    events.push(resolved);
    f.io.log.replace(events);
    no_init(&f).await;
    legacy_once(&f).await;
    debt.ack.store(
        std::fs::metadata(&debt.old).unwrap().len(),
        Ordering::SeqCst,
    );
    drive_until(|| f.ready.accepts(CHANNEL)).await;
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ch3fix_fresh_takeover_rechecks_a_after_the_fence() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_fresh_takeover_rechecks_a_after_the_fence"
    )) {
        return;
    }
    debt_case("fence").await;
}

#[tokio::test(start_paused = true)]
async fn ch3fix_quiet_a_needs_no_fabricated_receipt() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ch3fix_quiet_a_needs_no_fabricated_receipt"
    )) {
        return;
    }
    debt_case("empty").await;
}
