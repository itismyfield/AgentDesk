use super::*;
use crate::services::discord::input_runtime::supervisor::mapping::{Check, Edge, Probe};
use tokio::sync::oneshot;

fn edge(parent: u64, thread: u64) -> Edge {
    Edge {
        writer: "claude".into(),
        parent,
        thread,
    }
}

fn present(parent: u64, thread: u64, at: &'static str) -> HoldCause {
    HoldCause::MappingPresent(edge(parent, thread), at)
}

fn line(rig: &Rig, cause: &HoldCause) -> String {
    cause.health("claude", rig.channel)
}

fn checks(rig: &Rig) -> usize {
    rig.world.mapping_checks.load(Ordering::SeqCst)
}

async fn close(supervisor: &mut Supervisor<Fake>) -> Result<(), Deferred> {
    let (ack, reply) = oneshot::channel();
    supervisor
        .input_command(SupervisorCmd::Close { ack }, false)
        .await;
    reply.await.unwrap()
}

fn codex_runtime() -> Arc<crate::services::discord::SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    shared
}

#[tokio::test]
async fn x13_mapping_violation_freezes_then_latches_held() {
    for (index, reverse) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_325_130 + index as u64);
        let other = rig.channel + 100;
        let (parent, thread) = match reverse {
            false => (rig.channel, other),
            true => (other, rig.channel),
        };
        (rig.world.parents()).insert(ChannelId::new(parent), ChannelId::new(thread));
        drop(rig.received(11));
        let mut supervisor = rig.supervisor(Request::Ledger, vec![11]);
        let cause = present(parent, thread, "after_freeze");
        assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
        assert_eq!(
            rig.gate.mode(),
            Mode::Held,
            "a drained, frozen channel holds"
        );
        assert!(!supervisor.admission_open());
        assert!(rig.gate.admit().is_err());
        assert_eq!(rig.world.log(), ["subscribe", "freeze"]);
        assert_eq!(checks(&rig), 1);
        assert_eq!(rig.registry.health_reasons(), [line(&rig, &cause)]);
        assert!(
            line(&rig, &cause).contains("writer=claude")
                && line(&rig, &cause).contains("recheck=next_boot")
        );
        let parents = rig.world.parents();
        assert_eq!(
            parents.get(&ChannelId::new(parent)).map(|t| t.get()),
            Some(thread)
        );
        assert_eq!(
            rows(&rig.root, rig.channel).row(11).unwrap().state,
            RowState::Received
        );
        assert!(!rows(&rig.root, rig.channel).boundary_since(1));
        drop(supervisor);
    }
}

#[tokio::test]
async fn x13_mapping_latch_survives_removal_reboot_call_and_tick() {
    let rig = Rig::new(6_325_139);
    let parent = ChannelId::new(rig.channel);
    rig.world.parents().insert(parent, parent);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let first = present(rig.channel, rig.channel, "after_freeze");
    assert_eq!(supervisor.boot().await, Landing::Held(first.clone()));
    rig.world.parents().remove(&parent);
    for _ in 0..2 {
        assert_eq!(supervisor.boot().await, Landing::Held(first.clone()));
    }
    (rig.world.parents()).insert(ChannelId::new(rig.channel + 7), parent);
    assert_eq!(supervisor.boot().await, Landing::Held(first.clone()));
    assert_eq!(
        rig.world.log().iter().filter(|e| **e == "freeze").count(),
        1
    );
    assert_eq!(rig.world.effects("clear") + rig.world.effects("move"), 0);
    assert!(!supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(rig.registry.health_reasons(), [line(&rig, &first)]);
}

#[tokio::test]
async fn x13_mapping_latch_cannot_be_reset_by_release_or_reregistration() {
    let rig = Rig::new(6_325_140);
    rig.world
        .parents()
        .insert(ChannelId::new(rig.channel), ChannelId::new(1));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let cause = present(rig.channel, 1, "after_freeze");
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    assert!(supervisor.release());
    rig.world.parents().clear();
    for _ in 0..2 {
        let again = rig
            .registry
            .register(&ProviderKind::Claude, rig.channel, &rig.root);
        assert_eq!(again.err(), Some(Refused::Latched));
    }
    assert_eq!(rig.registry.health_reasons(), [line(&rig, &cause)]);
    assert_eq!(rig.gate.mode(), Mode::Held);
    drop(rig);
    // A lost supervisor poisons the channel without erasing the first evidence.
    let lost = Rig::new(6_325_141);
    lost.world
        .parents()
        .insert(ChannelId::new(7), ChannelId::new(lost.channel));
    let mut supervisor = lost.supervisor(Request::Ledger, vec![]);
    let cause = present(7, lost.channel, "after_freeze");
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    supervisor.registration.report(&cause, false);
    drop(supervisor);
    let health = lost.registry.health_reasons();
    assert!(health.contains(&line(&lost, &cause)));
    assert!(health.contains(&held("supervisor_lost").health("claude", lost.channel)));
    let again = lost
        .registry
        .register(&ProviderKind::Claude, lost.channel, &lost.root);
    assert_eq!(again.err(), Some(Refused::Poisoned));
}

#[tokio::test]
async fn x13_mapping_new_process_rechecks_empty_views() {
    let rig = Rig::new(6_325_142);
    drop(rig.received(11));
    rig.world
        .parents()
        .insert(ChannelId::new(rig.channel), ChannelId::new(5));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let cause = present(rig.channel, 5, "after_freeze");
    assert_eq!(supervisor.boot().await, Landing::Held(cause));
    assert!(supervisor.release());
    // A new process: fresh registry, gates and runtime view over the same durable root.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--ignored", "--test-threads=1"])
        .arg("services::discord::input_runtime::supervisor::tests::mapping_guard::x13_mapping_new_process_child")
        .env("ADK_X13_B4_ROOT", &rig.root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert_eq!(
        rows(&rig.root, rig.channel).row(11).unwrap().state,
        RowState::Received
    );
}

#[tokio::test]
#[ignore = "child process of x13_mapping_new_process_rechecks_empty_views"]
async fn x13_mapping_new_process_child() {
    let mut rig = Rig::new(6_325_142);
    rig.root = PathBuf::from(std::env::var_os("ADK_X13_B4_ROOT").expect("parent root"));
    assert!(rig.registry.health_reasons().is_empty());
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert!(supervisor.admission_open());
    assert_eq!(
        rig.world.log()[..4],
        ["subscribe", "freeze", "clear", "move"]
    );
    assert_eq!(
        rows(&rig.root, rig.channel).row(11).unwrap().state,
        RowState::Received
    );
    assert!(supervisor.release());
}

// One fresh boot whose real map gains `parent → thread` at the nth `at` fake event.
async fn late(channel: u64, inputs: Vec<u64>, at: &'static str, nth: usize) -> (Rig, Landing) {
    let rig = Rig::new(channel);
    *rig.world.inject.lock().unwrap() = Some((at, nth, (rig.channel, 9)));
    let mut supervisor = rig.supervisor(Request::Ledger, inputs);
    let landing = supervisor.boot().await;
    assert!(!supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Held, "{at}");
    assert_eq!(
        supervisor.boot().await,
        landing,
        "the same process never moves on"
    );
    (rig, landing)
}

#[tokio::test(start_paused = true)]
async fn x13_mapping_late_violation_stops_at_each_effect_boundary() {
    {
        let (rig, landing) = late(6_325_143, vec![11], "move", 0).await;
        assert_eq!(
            landing,
            Landing::Held(present(6_325_143, 9, "move_prepare"))
        );
        assert_eq!(rig.world.effects("collect"), 0);
    }
    {
        let (rig, landing) = late(6_325_144, vec![11, 12], "evidence", 0).await;
        assert_eq!(landing, Landing::Held(present(6_325_144, 9, "move_stage")));
        let restored = rows(&rig.root, rig.channel);
        assert_eq!(
            restored.staged_since(1),
            BTreeSet::from([11]),
            "the earlier stage stays"
        );
        assert!(!restored.boundary_since(1));
        assert_eq!(rig.world.effects("evidence"), 1);
    }
    {
        let (rig, landing) = late(6_325_145, vec![11], "evidence", 0).await;
        assert_eq!(landing, Landing::Held(present(6_325_145, 9, "move_commit")));
        let restored = rows(&rig.root, rig.channel);
        assert_eq!(restored.staged_since(1), BTreeSet::from([11]));
        assert!(!restored.boundary_since(1));
        assert_eq!(rig.world.effects("delete"), 0);
    }
    for nth in 0..4 {
        let channel = 6_325_146 + nth as u64;
        let (rig, landing) = late(channel, vec![11], "delete", nth).await;
        let at = if nth == 3 {
            "move_actor"
        } else {
            "move_delete"
        };
        assert_eq!(landing, Landing::Held(present(channel, 9, at)));
        assert_eq!(
            rig.world.effects("delete"),
            nth + 1,
            "no deletion after the edge"
        );
        assert_eq!(rig.world.effects("actor"), 0);
        assert!(rows(&rig.root, rig.channel).boundary_since(1));
    }
    // A committed move resumes at its first deletion; an edge found while collecting stops it.
    let rig = Rig::new(6_325_150);
    let mut ledger = rig.ledger();
    let staged = Entry::Staged {
        key: 11,
        input: json!({ "text": 11 }),
        state: RowState::Received,
    };
    ledger.append_entry(&staged, &[]).unwrap();
    let commit = Entry::MoveCommitted {
        first_staged_seq: 1,
        ids: vec![11],
    };
    ledger.append_entry(&commit, &[]).unwrap();
    drop(ledger);
    *rig.world.inject.lock().unwrap() = Some(("collect", 0, (rig.channel, 9)));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![11]);
    let cause = present(rig.channel, 9, "move_delete");
    assert_eq!(supervisor.boot().await, Landing::Held(cause));
    assert_eq!(rig.world.effects("delete"), 0);
    assert_eq!(rig.gate.mode(), Mode::Held);
}

#[tokio::test(start_paused = true)]
async fn x13_mapping_late_violation_stops_clear_replay_at_its_boundaries() {
    for (index, confirmed) in [true, false].into_iter().enumerate() {
        let rig = Rig::new(6_325_165 + index as u64);
        let ledger = rig.received(11);
        rig.world.durable_ticket(rig.channel, &ledger, &[11]);
        drop(ledger);
        let world = rig.world.clone();
        let channel = rig.channel;
        let then: Then = Some(Box::new(move || {
            world
                .parents()
                .insert(ChannelId::new(channel), ChannelId::new(9));
        }));
        rig.world.reset(confirmed, then);
        let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
        let at = if confirmed {
            "after_clear"
        } else {
            "before_clear"
        };
        assert_eq!(
            supervisor.boot().await,
            Landing::Held(present(channel, 9, at))
        );
        assert_eq!(
            rig.world.effects("clear"),
            1,
            "no clear retry after the edge"
        );
        assert_eq!(rig.world.effects("move"), 0);
        assert_eq!(rig.gate.mode(), Mode::Held);
        let row = rows(&rig.root, rig.channel).row(11).unwrap().state;
        // The cutoff applied before the reset stays durable either way.
        assert_eq!(row, RowState::Abandoned(AbandonReason::UserClear));
    }
}

#[tokio::test]
async fn x13_mapping_checks_all_runtime_views_and_reports_pending() {
    let claude = crate::services::discord::make_shared_data_for_tests();
    let codex = codex_runtime();
    let both = [ProviderKind::Claude, ProviderKind::Codex];
    let probe = Probe::default();
    assert_eq!(probe.check(41), Check::Pending);
    assert!(probe.adopt(&both, vec![claude.clone(), codex.clone()]));
    assert!(
        !probe.adopt(&both[..1], vec![claude.clone()]),
        "the first adoption is final"
    );
    assert_eq!(probe.check(41), Check::Empty);
    let writer = |parent, thread| Edge {
        writer: "codex".into(),
        parent,
        thread,
    };
    codex
        .dispatch
        .thread_parents
        .insert(ChannelId::new(41), ChannelId::new(42));
    assert_eq!(probe.clone().check(41), Check::Mapped(writer(41, 42)));
    assert_eq!(probe.check(42), Check::Mapped(writer(41, 42)));
    assert_eq!(probe.check(43), Check::Empty);
    for runtimes in [
        vec![claude.clone()],
        vec![claude.clone(), claude.clone()],
        vec![],
    ] {
        let partial = Probe::default();
        assert!(partial.adopt(&both, runtimes));
        assert_eq!(partial.check(41), Check::Unavailable);
    }
    for (index, flag) in ["pending", "unavailable"].into_iter().enumerate() {
        let rig = Rig::new(6_325_152 + index as u64);
        let flag = match flag {
            "pending" => &rig.world.mapping_pending,
            _ => &rig.world.mapping_unavailable,
        };
        flag.store(true, Ordering::SeqCst);
        let cause = match index {
            0 => HoldCause::RuntimeViewPending,
            _ => HoldCause::MappingUnavailable,
        };
        let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
        assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
        assert!(!supervisor.admission_open());
        assert_eq!(rig.gate.mode(), Mode::Frozen);
        assert_eq!(rig.registry.health_reasons(), [line(&rig, &cause)]);
        assert_eq!(rig.world.effects("clear") + rig.world.effects("move"), 0);
        flag.store(false, Ordering::SeqCst);
        assert_eq!(supervisor.boot().await, Landing::Admitted);
        assert!(rig.registry.health_reasons().is_empty());
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn x13_legacy_handback_clears_only_stale_mapping_health() {
    let rig = Rig::new(6_325_154);
    drop(rig.received(41));
    rig.world.mapping_pending.store(true, Ordering::SeqCst);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::RuntimeViewPending)
    );
    supervisor
        .registration
        .report(&HoldCause::MappingUnavailable, true);
    let flush = held("ledger_close_flush_unconfirmed");
    supervisor.registration.report(&flush, true);
    supervisor.config.request = Request::Legacy;
    assert_eq!(supervisor.boot().await, Landing::HandedBack);
    assert_eq!(*rig.world.enqueued.lock().unwrap(), [41]);
    let health = rig.registry.health_reasons();
    assert_eq!(health, [line(&rig, &flush)], "only the mapping lines leave");
}

#[tokio::test]
async fn x13_mapping_latch_blocks_same_boot_legacy_handback() {
    let rig = Rig::new(6_325_155);
    drop(rig.received(41));
    rig.world
        .parents()
        .insert(ChannelId::new(rig.channel), ChannelId::new(3));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let cause = present(rig.channel, 3, "after_freeze");
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    rig.world.parents().clear();
    supervisor.config.request = Request::Legacy;
    supervisor.registration.report(&cause, false);
    supervisor
        .registration
        .report(&HoldCause::MappingUnavailable, false);
    for _ in 0..2 {
        assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    }
    assert_eq!(supervisor.handbacks, 0);
    assert!(rig.world.enqueued.lock().unwrap().is_empty());
    assert_eq!(rig.world.effects("move"), 0);
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(rig.registry.health_reasons(), [line(&rig, &cause)]);
}

#[tokio::test(start_paused = true)]
async fn x13_mapping_requires_last_writer_drain_and_real_freeze_ack() {
    let rig = Rig::new(6_325_156);
    let permit = rig.gate.admit().unwrap();
    rig.world
        .parents()
        .insert(ChannelId::new(rig.channel), ChannelId::new(4));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let mut boot = Box::pin(supervisor.boot());
    assert!(futures::poll!(boot.as_mut()).is_pending());
    assert_eq!((rig.gate.mode(), checks(&rig)), (Mode::Closing, 0));
    permit.validate(&ProviderKind::Claude, rig.channel).unwrap();
    drop(permit);
    let cause = present(rig.channel, 4, "after_freeze");
    assert_eq!(boot.await, Landing::Held(cause));
    assert_eq!((rig.gate.mode(), checks(&rig)), (Mode::Held, 1));
    assert_eq!(rig.world.log(), ["subscribe", "freeze"]);
    drop((supervisor, rig));
    // A drain timeout or a refused freeze is no empty-map proof and never reports as one.
    let rig = Rig::new(6_325_157);
    let permit = rig.gate.admit().unwrap();
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(held("drain_timeout"))
    );
    assert_eq!((rig.gate.mode(), checks(&rig)), (Mode::Closing, 0));
    drop((permit, supervisor, rig));
    let rig = Rig::new(6_325_158);
    rig.world.freeze_failed.store(true, Ordering::SeqCst);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Held(held("freeze")));
    assert_eq!((rig.gate.mode(), checks(&rig)), (Mode::Closing, 0));
    assert_eq!(rig.world.log(), ["subscribe", "freeze"]);
    assert!(supervisor.registration.latched().is_none());
    assert!(!supervisor.admission_open());
}

#[tokio::test(start_paused = true)]
async fn x13_close_and_clear_completion_do_not_clear_mapping_latch() {
    let rig = Rig::new(6_325_159);
    let ledger = rig.received(11);
    rig.world.durable_ticket(rig.channel, &ledger, &[11]);
    drop(ledger);
    let world = rig.world.clone();
    let channel = rig.channel;
    let then: Then = Some(Box::new(move || {
        world
            .parents()
            .insert(ChannelId::new(channel), ChannelId::new(8));
    }));
    rig.world.reset(true, then);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let cause = present(channel, 8, "after_clear");
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    rig.world.parents().clear();
    assert_eq!(close(&mut supervisor).await, Ok(()));
    rig.break_ledger(true);
    let mut lease = supervisor.slot.lend().unwrap();
    lease.needs_reopen = true;
    supervisor.slot.restore(lease);
    assert_eq!(close(&mut supervisor).await, Err(Deferred::Persistence));
    rig.break_ledger(false);
    let mut lease = supervisor.slot.lend().unwrap();
    lease.needs_reopen = true;
    supervisor.slot.restore(lease);
    assert_eq!(close(&mut supervisor).await, Ok(()));
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    assert_eq!(rig.world.effects("clear") + rig.world.effects("move"), 1);
    assert!(!supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(rig.registry.health_reasons(), [line(&rig, &cause)]);
}

#[test]
fn x13_mapping_hold_slots_are_independent_of_close_and_gate_failures() {
    let registry = registry();
    let dir = sandbox();
    let registration = registry
        .register(&ProviderKind::Claude, 6_325_137, dir.path())
        .unwrap();
    let causes = [
        HoldCause::RuntimeViewPending,
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
    registration.report(&HoldCause::RuntimeViewPending, false);
    assert_eq!(
        registry.health_reasons(),
        [HoldCause::MappingUnavailable.health("claude", 6_325_137)]
    );
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
