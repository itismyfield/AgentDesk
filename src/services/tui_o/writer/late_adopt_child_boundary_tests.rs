//! Exercise lifecycle boundaries through the actual managed host, retaining its generation.
use super::*;
use crate::services::tui_o::channel_policy;
use crate::services::tui_o::writer::adoption::{LegacyEpoch, LegacyTurnIdentity};
use std::sync::atomic::AtomicBool;

struct Probe {
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
    for _ in 0..24 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if f.ready.accepts(CHANNEL) {
            return;
        }
    }
    panic!(
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
