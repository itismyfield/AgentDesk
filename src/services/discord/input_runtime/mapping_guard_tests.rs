use super::*;
use tokio::sync::oneshot;

fn mapped_line(rig: &Rig) -> String {
    HoldCause::MappedThread.health("claude", rig.channel)
}

async fn close(supervisor: &mut Supervisor<Fake>) -> Result<(), Deferred> {
    let (ack, reply) = oneshot::channel();
    supervisor
        .input_command(SupervisorCmd::Close { ack }, false)
        .await;
    reply.await.unwrap()
}

#[tokio::test]
async fn x13_mapping_both_endpoints_hold_before_freeze_and_recheck_the_actual_map() {
    for (index, reverse) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_325_130 + index as u64);
        let channel = ChannelId::new(rig.channel);
        let other = ChannelId::new(rig.channel + 100);
        let (parent, thread) = if reverse {
            (other, channel)
        } else {
            (channel, other)
        };
        rig.world.parents.insert(parent, thread);
        let mut supervisor = rig.supervisor(Request::Ledger, vec![11]);
        for _ in 0..2 {
            assert_eq!(
                supervisor.boot().await,
                Landing::Held(HoldCause::MappedThread)
            );
            assert!(!supervisor.admission_open());
            assert_eq!(rig.gate.mode(), Mode::Closing);
            assert!(rig.gate.admit().is_err());
            assert!(rig.health().contains(&mapped_line(&rig)));
            assert_eq!(
                rig.world.parents.get(&parent).map(|edge| *edge),
                Some(thread)
            );
            assert!(!rig.root.join("input_ledger").exists());
            assert!(rig.world.log().iter().all(|entry| *entry == "subscribe"));
        }
        rig.world.parents.remove(&parent);
        rig.world
            .parents
            .insert(other, ChannelId::new(rig.channel + 200));
        assert_eq!(supervisor.boot().await, Landing::Admitted);
        assert!(supervisor.admission_open());
        assert!(!rig.health().contains(&mapped_line(&rig)));
        assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 3);
        assert_eq!(
            rig.world
                .log()
                .iter()
                .filter(|entry| **entry == "freeze")
                .count(),
            1
        );
        let restored = rows(&rig.root, rig.channel);
        assert_eq!(restored.row(11).unwrap().state, RowState::Received);
        assert_eq!(restored.open_rows().count(), 1);
        let closing = supervisor.registration.closing(&rig.gate).unwrap();
        closing.open_ledger().unwrap();
        assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
        assert_eq!(
            mapping::inspect(&rig.world.parents, rig.channel),
            mapping::Check::Empty
        );
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn x13_mapping_waits_for_the_last_admitted_writer_before_inspection() {
    let rig = Rig::new(6_325_132);
    let permit = rig.gate.admit().unwrap();
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let mut boot = Box::pin(supervisor.boot());
    assert!(futures::poll!(boot.as_mut()).is_pending());
    assert_eq!(rig.gate.mode(), Mode::Closing);
    assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 0);
    permit.validate(&ProviderKind::Claude, rig.channel).unwrap();
    rig.world
        .parents
        .insert(ChannelId::new(rig.channel), ChannelId::new(rig.channel + 1));
    drop(permit);
    assert_eq!(boot.await, Landing::Held(HoldCause::MappedThread));
    assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 1);
    assert!(!supervisor.admission_open());
    assert_eq!(rig.world.log(), ["subscribe"]);
    assert!(supervisor.release());
}

#[tokio::test(start_paused = true)]
async fn x13_mapping_drain_timeout_is_not_an_empty_mapping_proof() {
    let rig = Rig::new(6_325_133);
    let permit = rig.gate.admit().unwrap();
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    supervisor
        .registration
        .report(&HoldCause::MappedThread, true);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(held("drain_timeout"))
    );
    assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 0);
    assert!(rig.health().contains(&mapped_line(&rig)));
    assert!(!supervisor.admission_open());
    drop(permit);
    assert!(supervisor.release());
}

#[tokio::test]
async fn x13_mapping_unavailable_preserves_prior_hold_and_retries_without_a_cached_pass() {
    let rig = Rig::new(6_325_134);
    let parent = ChannelId::new(rig.channel);
    rig.world.parents.insert(parent, parent);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::MappedThread)
    );
    rig.world.mapping_unavailable.store(true, Ordering::SeqCst);
    rig.world.parents.remove(&parent);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::MappingUnavailable)
    );
    assert!(rig.health().contains(&mapped_line(&rig)));
    let unavailable = HoldCause::MappingUnavailable.health("claude", rig.channel);
    assert!(rig.health().contains(&unavailable));
    assert_eq!(rig.world.log(), ["subscribe", "subscribe"]);
    rig.world.mapping_unavailable.store(false, Ordering::SeqCst);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert!(!rig.health().contains(&mapped_line(&rig)));
    assert!(!rig.health().contains(&unavailable));
    rig.world.parents.insert(parent, parent);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::MappedThread)
    );
    assert!(!supervisor.admission_open());
    assert!(supervisor.release());
}

#[tokio::test]
async fn x13_mapping_close_flush_success_and_failure_never_clear_mapping_or_reopen_admission() {
    let rig = Rig::new(6_325_135);
    let parent = ChannelId::new(rig.channel);
    rig.world.parents.insert(parent, parent);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::MappedThread)
    );
    assert_eq!(close(&mut supervisor).await, Ok(()));
    assert!(rig.health().contains(&mapped_line(&rig)));
    assert!(!supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Closing);
    rig.break_ledger(true);
    let mut lease = supervisor.slot.lend().unwrap();
    lease.needs_reopen = true;
    supervisor.slot.restore(lease);
    assert_eq!(close(&mut supervisor).await, Err(Deferred::Persistence));
    let flush = held("ledger_close_flush_unconfirmed").health("claude", rig.channel);
    assert!(rig.health().contains(&flush));
    assert!(rig.health().contains(&mapped_line(&rig)));
    rig.break_ledger(false);
    let mut lease = supervisor.slot.lend().unwrap();
    lease.needs_reopen = true;
    supervisor.slot.restore(lease);
    rig.world.parents.remove(&parent);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert!(!rig.health().contains(&mapped_line(&rig)));
    assert!(
        rig.health().contains(&flush),
        "an empty map does not prove a Close flush"
    );
    assert!(
        !supervisor.admission_open(),
        "Close survives a successful boot"
    );
    assert_eq!(close(&mut supervisor).await, Ok(()));
    assert!(!rig.health().contains(&flush));
    assert!(!supervisor.admission_open());
    assert!(supervisor.release());
}

#[tokio::test]
async fn x13_mapping_legacy_handback_skips_inspection_without_clearing_a_mapping_hold() {
    let rig = Rig::new(6_325_136);
    let parent = ChannelId::new(rig.channel);
    rig.world.parents.insert(parent, parent);
    rig.world.mapping_unavailable.store(true, Ordering::SeqCst);
    let mut supervisor = rig.supervisor(Request::Legacy, vec![]);
    supervisor
        .registration
        .report(&HoldCause::MappedThread, true);
    assert_eq!(supervisor.boot().await, Landing::HandedBack);
    assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 0);
    assert!(rig.health().contains(&mapped_line(&rig)));
    assert!(rig.world.parents.contains_key(&parent));
    assert!(!supervisor.admission_open());
    assert!(supervisor.release());
}

#[test]
fn x13_mapping_hold_slots_are_independent_of_close_and_gate_failures() {
    let registry = registry();
    let dir = sandbox();
    let registration = registry
        .register(&ProviderKind::Claude, 6_325_137, dir.path())
        .unwrap();
    let causes = [
        HoldCause::MappedThread,
        HoldCause::MappingUnavailable,
        held("freeze"),
        held("ledger_close_flush_unconfirmed"),
    ];
    for cause in &causes {
        registration.report(cause, true);
    }
    assert_eq!(registry.health_reasons().len(), 4);
    registration.report(&causes[3], false);
    registration.report(&causes[2], false);
    assert_eq!(registry.health_reasons().len(), 2);
    registration.report(&HoldCause::MappedThread, false);
    assert_eq!(
        registry.health_reasons(),
        [HoldCause::MappingUnavailable.health("claude", 6_325_137)]
    );
    assert_eq!(HoldCause::MappedThread.slot(), "mapped_thread");
}

#[tokio::test]
async fn x13_mapping_empty_does_not_substitute_for_a_gate_freeze_ack() {
    let rig = Rig::new(6_325_138);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    rig.world.freeze_failed.store(true, Ordering::SeqCst);
    assert_eq!(supervisor.boot().await, Landing::Held(held("freeze")));
    assert_eq!(rig.world.mapping_checks.load(Ordering::SeqCst), 1);
    assert_eq!(rig.gate.mode(), Mode::Closing);
    assert!(!supervisor.admission_open());
    assert_eq!(rig.world.log(), ["subscribe", "freeze"]);
    assert_eq!(close(&mut supervisor).await, Ok(()));
    assert!(
        rig.health()
            .contains(&held("freeze").health("claude", rig.channel))
    );
    assert!(!supervisor.admission_open());
    assert!(supervisor.release());
}

#[test]
fn x13_mapping_production_source_census() {
    let output = std::process::Command::new("python3")
        .arg("scripts/test_input_mapping_census.py")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
