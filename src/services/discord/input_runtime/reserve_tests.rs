use super::*;
use crate::services::discord::input_runtime::fence::effect;
use crate::services::discord::input_runtime::supervisor::activation::plan::{
    InputMode, InputSelection,
};
use crate::services::discord::input_runtime::supervisor::activation::scope::{
    ChannelKind, Refusal, RouteBinding, RoutingSnapshot,
};
use crate::services::discord::input_runtime::supervisor::reserve::{BootPlan, PlanError};

// A named routing change and the refusal it must produce.
type Case = (
    &'static str,
    Box<dyn Fn(u64, &mut RoutingSnapshot)>,
    Refusal,
);

// One Claude TUI agent whose primary channel is `channel`, selected for O turns.
fn config(channel: u64) -> crate::config::Config {
    serde_yaml::from_str(&format!(
        "server: {{}}\ndata: {{}}\n\
         agents:\n  - id: scope\n    name: Scope\n    channels:\n      claude: {{id: '{channel}', runtime: tui}}\n\
         tui_o:\n  turn: {{channels: [{channel}]}}\n  writer: {{all_tui: true}}\n"
    ))
    .unwrap()
}

fn selection(channels: &[u64]) -> InputSelection {
    InputSelection {
        mode: InputMode::Ledger,
        channels: Some(channels.iter().copied().collect()),
    }
}

fn binding(channel: u64, original: ProviderKind, writer: ProviderKind) -> RouteBinding {
    RouteBinding {
        agent: "scope".into(),
        channel: Some(channel),
        original_provider: Some(original),
        writer_provider: Some(writer),
    }
}

fn snapshot(channel: u64) -> RoutingSnapshot {
    let claude = ProviderKind::Claude;
    RoutingSnapshot {
        channel,
        channel_kind: Some(ChannelKind::NonThread),
        primary: Some(vec![binding(channel, claude.clone(), claude)]),
        alt: Some(Vec::new()),
        cc: Some(Vec::new()),
        cdx: Some(Vec::new()),
        overrides: Some(Vec::new()),
    }
}

// The prepared inputs of one boot plan, reserved on a fresh process registry.
fn reserve(
    registry: &'static Registry,
    root: &Path,
    channel: u64,
    snapshot: Option<RoutingSnapshot>,
    presence: Presence,
) -> Result<BootPlan, PlanError> {
    let snapshots = snapshot.map(|s| (channel, s)).into_iter().collect();
    let responsibility = BTreeMap::from([(channel, presence)]);
    let config = config(channel);
    registry.reserve_boot(
        root,
        &selection(&[channel]),
        &config,
        &snapshots,
        &responsibility,
    )
}

#[test]
fn x13_e1_reservation_closes_before_supervisor_adopt() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let (registry, dir, channel) = (registry(), sandbox(), 6_325_170);
    let plan = reserve(
        registry,
        dir.path(),
        channel,
        Some(snapshot(channel)),
        Presence::Absent,
    );
    let plan = plan.unwrap();
    assert_eq!((plan.reserved.len(), plan.refused.len()), (1, 0));
    let gate = fence::lookup(&ProviderKind::Claude, channel).expect("protected at reservation");
    let _health = test_health::Clear::new(&gate);
    assert_eq!(
        gate.mode(),
        Mode::Closing,
        "closed before any supervisor starts"
    );
    assert!(plan.reserved[0].hold.is_none());
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let found = fence::lookup(&provider, channel).unwrap();
        assert!(
            Arc::ptr_eq(&found, &gate),
            "skip consumers see the protection"
        );
        let refused = effect::admit(&provider, channel).err();
        assert_eq!(
            refused,
            Some(Failure::Mode(Mode::Closing)),
            "no Legacy admit"
        );
    }
    assert!(registry.used());
    assert!(!dir.path().join("input_ledger").exists());
}

#[tokio::test]
async fn x13_e1_adopt_reuses_gate_closing_and_epoch() {
    let rig = Rig::new(6_325_171);
    let permit = rig.gate.admit().unwrap();
    let mut plan = reserve(
        rig.registry,
        &rig.root,
        rig.channel,
        Some(snapshot(rig.channel)),
        Presence::Absent,
    )
    .unwrap();
    let reserved = plan.reserved.pop().unwrap();
    let key = ("claude".to_owned(), rig.channel);
    let closing = rig
        .registry
        .closings
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap();
    assert!(Arc::ptr_eq(reserved.gate.as_ref().unwrap(), &rig.gate));
    let supervisor = rig.adopt(reserved, Request::Ledger, vec![], None).unwrap();
    let adopted = supervisor.registration.closing(&rig.gate).unwrap();
    assert!(
        Arc::ptr_eq(&adopted, &closing),
        "the reservation's close, not a new one"
    );
    assert!(Arc::ptr_eq(supervisor.gate.as_ref().unwrap(), &rig.gate));
    assert_eq!(rig.gate.mode(), Mode::Closing);
    permit
        .validate(&ProviderKind::Claude, rig.channel)
        .expect("adoption keeps the epoch");
    drop((permit, supervisor));
    // A config naming another channel or root never adopts the reservation.
    let other = registry();
    let reserved = other.reserve_for_test(&ProviderKind::Claude, rig.channel, &rig.root.join("x"));
    assert_eq!(
        rig.adopt(reserved.unwrap(), Request::Ledger, vec![], None)
            .err(),
        Some(Refused::Mismatch)
    );
}

#[tokio::test]
async fn x13_e2_reservation_enforces_writer_scope_and_responsibility() {
    let (claude, codex) = (ProviderKind::Claude, ProviderKind::Codex);
    let cases: Vec<Case> = vec![
        (
            "alt",
            Box::new(|c, s| {
                s.alt
                    .as_mut()
                    .unwrap()
                    .push(binding(c, ProviderKind::Codex, ProviderKind::Codex))
            }),
            Refusal::CrossProviderTarget,
        ),
        (
            "cc",
            Box::new(|c, s| {
                s.cc.as_mut()
                    .unwrap()
                    .push(binding(c, ProviderKind::Codex, ProviderKind::Codex))
            }),
            Refusal::CrossProviderTarget,
        ),
        (
            "cdx",
            Box::new(|c, s| {
                s.cdx
                    .as_mut()
                    .unwrap()
                    .push(binding(c, ProviderKind::Codex, ProviderKind::Codex))
            }),
            Refusal::CrossProviderTarget,
        ),
        (
            "override",
            Box::new(|c, s| {
                s.overrides.as_mut().unwrap().push(binding(
                    c,
                    ProviderKind::Claude,
                    ProviderKind::Codex,
                ))
            }),
            Refusal::CrossProviderTarget,
        ),
        (
            "original",
            Box::new(|c, s| {
                s.primary = Some(vec![binding(c, ProviderKind::Codex, ProviderKind::Claude)])
            }),
            Refusal::PrimaryProviderMismatch,
        ),
        (
            "writer",
            Box::new(|c, s| {
                s.primary = Some(vec![binding(c, ProviderKind::Claude, ProviderKind::Codex)])
            }),
            Refusal::PrimaryProviderMismatch,
        ),
        (
            "thread",
            Box::new(|_, s| s.channel_kind = Some(ChannelKind::Thread)),
            Refusal::Thread,
        ),
        (
            "unknown",
            Box::new(|_, s| s.alt = None),
            Refusal::SnapshotUnknown,
        ),
    ];
    for (index, (name, mutate, reason)) in cases.iter().enumerate() {
        // Without responsibility: refused, reported, no gate, no WAL.
        let channel = 6_325_200 + index as u64;
        {
            let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
            let (registry, dir) = (registry(), sandbox());
            let mut routing = snapshot(channel);
            mutate(channel, &mut routing);
            let plan = reserve(
                registry,
                dir.path(),
                channel,
                Some(routing),
                Presence::Absent,
            )
            .unwrap();
            assert!(plan.reserved.is_empty(), "{name}");
            assert_eq!(plan.refused, [(channel, *reason)], "{name}");
            assert!(fence::lookup(&claude, channel).is_none(), "{name}");
            assert!(fence::lookup(&codex, channel).is_none(), "{name}");
            let line = HoldCause::ScopeRefused(*reason).health("claude", channel);
            assert_eq!(registry.health_reasons(), [line], "{name}");
            assert!(!dir.path().join("input_ledger").exists());
        }
        // With responsibility: protected and held; no Legacy fallback, Move or admission.
        let rig = Rig::new(6_325_220 + index as u64);
        drop(rig.received(11));
        let mut routing = snapshot(rig.channel);
        mutate(rig.channel, &mut routing);
        let mut plan = reserve(
            rig.registry,
            &rig.root,
            rig.channel,
            Some(routing),
            Presence::Present,
        )
        .unwrap();
        assert!(plan.refused.is_empty(), "{name}");
        let mut supervisor = rig
            .adopt(plan.reserved.pop().unwrap(), Request::Ledger, vec![], None)
            .unwrap();
        let cause = HoldCause::ScopeHeld(*reason);
        assert_eq!(supervisor.boot().await, Landing::Held(cause), "{name}");
        assert_eq!(rig.gate.mode(), Mode::Closing, "{name}");
        assert!(!supervisor.admission_open());
        assert_eq!(rig.world.log(), Vec::<&str>::new(), "{name}");
        assert_eq!(
            rows(&rig.root, rig.channel).row(11).unwrap().state,
            RowState::Received
        );
    }
    // No routing snapshot at all is unknown, never a candidate.
    {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let (registry, dir, channel) = (registry(), sandbox(), 6_325_251);
        let plan = reserve(registry, dir.path(), channel, None, Presence::Absent).unwrap();
        assert!(plan.reserved.is_empty());
        assert_eq!(plan.refused, [(channel, Refusal::SnapshotUnknown)]);
        assert!(fence::lookup(&claude, channel).is_none());
    }
    // A supported scope whose responsibility is unreadable is still held.
    let rig = Rig::new(6_325_240);
    let routing = Some(snapshot(rig.channel));
    let mut plan = reserve(
        rig.registry,
        &rig.root,
        rig.channel,
        routing,
        Presence::Unreadable,
    )
    .unwrap();
    let mut supervisor = rig
        .adopt(plan.reserved.pop().unwrap(), Request::Ledger, vec![], None)
        .unwrap();
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::LedgerUnreadable)
    );
    assert_eq!(rig.gate.mode(), Mode::Closing);
    assert_eq!(rig.world.log(), Vec::<&str>::new());
}

#[tokio::test]
async fn x13_e1_boot_selection_is_sealed_for_process_lifetime() {
    let rig = Rig::new(6_325_241);
    let mut routing = snapshot(rig.channel);
    let plan = reserve(
        rig.registry,
        &rig.root,
        rig.channel,
        Some(routing.clone()),
        Presence::Absent,
    );
    let mut plan = plan.unwrap();
    let other = 6_325_250;
    for (channel, presence) in [(other, Presence::Absent), (rig.channel, Presence::Present)] {
        let again = reserve(
            rig.registry,
            &rig.root,
            channel,
            Some(snapshot(channel)),
            presence,
        );
        assert!(matches!(again, Err(PlanError::Sealed)));
    }
    let codex = (rig.registry).register(&ProviderKind::Codex, rig.channel, &rig.root);
    assert_eq!(codex.err(), Some(Refused::Sealed));
    assert!(fence::lookup(&ProviderKind::Claude, other).is_none());
    // Changing the prepared snapshot afterwards changes nothing already installed.
    routing.alt.as_mut().unwrap().push(binding(
        rig.channel,
        ProviderKind::Codex,
        ProviderKind::Codex,
    ));
    let mut supervisor = rig
        .adopt(plan.reserved.pop().unwrap(), Request::Legacy, vec![], None)
        .unwrap();
    assert_eq!(supervisor.boot().await, Landing::HandedBack);
    assert!(supervisor.release());
    let again = reserve(
        rig.registry,
        &rig.root,
        rig.channel,
        Some(snapshot(rig.channel)),
        Presence::Absent,
    );
    assert!(
        matches!(again, Err(PlanError::Sealed)),
        "a handback never reopens the plan"
    );
    let reregister = (rig.registry).register(&ProviderKind::Claude, rig.channel, &rig.root);
    assert_eq!(reregister.err(), Some(Refused::Sealed));
}

#[tokio::test]
async fn x13_channel_wide_lookup_blocks_skip_admit_and_foreign_permit() {
    let pairs = [
        (ProviderKind::Codex, ProviderKind::Claude, 6_325_242),
        (ProviderKind::Claude, ProviderKind::Codex, 6_325_243),
    ];
    for (owner, foreign, channel) in pairs {
        let gate = Gate::protect(owner.clone(), channel).unwrap();
        let _health = test_health::Clear::new(&gate);
        let held = gate.admit().unwrap();
        let _closing = gate.close().unwrap();
        let found = fence::lookup(&foreign, channel).expect("skip consumers find the owner's gate");
        assert!(Arc::ptr_eq(&found, &gate));
        assert_eq!(
            effect::admit(&foreign, channel).err(),
            Some(Failure::Mode(Mode::Closing))
        );
        assert_eq!(held.validate(&foreign, channel), Err(Failure::StalePermit));
        let reused = effect::scope(Some(held.clone()), async {
            effect::admit(&foreign, channel)
        });
        assert_eq!(reused.await.err(), Some(Failure::Mode(Mode::Closing)));
        assert!(Arc::ptr_eq(&fence::lookup(&owner, channel).unwrap(), &gate));
    }
    // A reservation never installs a second provider's gate beside the owner's.
    let (registry, dir) = (registry(), sandbox());
    let codex = fence::lookup(&ProviderKind::Codex, 6_325_242).unwrap();
    let routing = Some(snapshot(6_325_242));
    let plan = reserve(registry, dir.path(), 6_325_242, routing, Presence::Absent).unwrap();
    let reserved = &plan.reserved[0];
    assert!(reserved.gate.is_none());
    assert_eq!(reserved.hold, Some(held("protect")));
    let found = fence::lookup(&ProviderKind::Claude, 6_325_242).unwrap();
    assert!(Arc::ptr_eq(&found, &codex), "only the owner's gate exists");
}

#[test]
fn x13_bc_off_lookup_and_health_unchanged() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let channel = 6_325_244;
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        assert!(fence::lookup(&provider, channel).is_none());
        assert!(matches!(effect::admit(&provider, channel), Ok(None)));
    }
    assert!(!REGISTRY.used(), "no production path registers or reserves");
    assert!(REGISTRY.health_reasons().is_empty());
}

#[test]
fn x13_bc_empty_boot_plan_creates_no_input_resources() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let channel = 6_325_245;
    for selection in [InputSelection::default(), selection(&[])] {
        let (registry, dir) = (registry(), sandbox());
        let before = opens();
        let plan = registry.reserve_boot(
            dir.path(),
            &selection,
            &config(channel),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        let plan = plan.unwrap();
        assert!(plan.reserved.is_empty() && plan.refused.is_empty());
        assert!(!registry.used());
        assert!(registry.health_reasons().is_empty());
        assert!(fence::lookup(&ProviderKind::Claude, channel).is_none());
        assert_eq!(opens(), before, "no ledger open");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "no WAL, blob or lock"
        );
        let again = registry.reserve_boot(
            dir.path(),
            &selection,
            &config(channel),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(matches!(again, Err(PlanError::Sealed)));
    }
}
