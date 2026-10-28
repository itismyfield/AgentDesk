use super::*;
use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const OLD_NONCE: &str = "11111111111141118111111111111111";
const SUCCESSOR: &str = "22222222222242228222222222222222";
const OTHER_NONCE: &str = "33333333333343338333333333333333";
const READY: &str = include_str!("../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
const RECEIPT_STEPS: [Step; 6] = [
    Step::Write,
    Step::FileSync,
    Step::Rename,
    Step::ParentSync,
    Step::MigrationSync,
    Step::RootSync,
];

fn assert_publication_synced(publication: &Arc<Mutex<PublicationTrace>>, phase: &Phase) {
    let trace = publication
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(trace.phase.as_ref(), Some(phase), "current receipt phase");
    assert_eq!(
        trace.operations,
        RECEIPT_STEPS.map(|step| (step, true)),
        "effect requires every receipt operation to succeed in order"
    );
}

struct Rig {
    root: tempfile::TempDir,
    intent: Intent,
    store: Store,
}
impl Rig {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let owner = Owner {
            runtime_root: root.path().canonicalize().unwrap(),
            provider: "claude".into(),
            token_hash: "isolated-token".into(),
            channel: 7503,
            logical_key: "isolated-old-pane".into(),
        };
        let source = root.path().join(format!("{SID}.jsonl"));
        fs::write(&source, "{}\n").unwrap();
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(&source).unwrap();
        let intent = Intent {
            owner: owner.clone(),
            canonical_row: 37,
            hosted_raw: None,
            revision: Revision {
                clear_generation: 9,
                cached_sid: Some(SID.into()),
                raw_sid: Some(SID.into()),
                cwd: root.path().into(),
            },
            source: SourceFence {
                source: SourceId {
                    session_id: SID.into(),
                    path: source,
                    dev: m.dev(),
                    ino: m.ino(),
                },
                prefix_eof: 3,
                prefix_sha256: {
                    use sha2::{Digest, Sha256};
                    format!("{:x}", Sha256::digest(b"{}\n"))
                },
            },
            old: OldIncarnation {
                nonce: OLD_NONCE.into(),
                physical_pane: [
                    "77".into(),
                    "$1".into(),
                    "@2".into(),
                    "%3".into(),
                    "88".into(),
                ],
                provider_pid: 99,
                starttime: Some(100),
                macos_lstart_hash: None,
            },
            request_key: "original-request-attachments-fifo-head".into(),
        };
        let store = Store::open(root.path(), &owner).unwrap();
        Self {
            root,
            intent,
            store,
        }
    }
    fn host(&self) -> FakeHost {
        FakeHost::new(&self.intent)
    }
}

struct FakeHost {
    evidence: Intent,
    hosted: Hosted,
    token_live: bool,
    o_ready: bool,
    drained: bool,
    source_exclusive: bool,
    probes: VecDeque<OldProbe>,
    default_probe: OldProbe,
    calls: Vec<String>,
    cas: Cas,
    created: Create,
    bad_at: Option<Effect>,
    store_dir: PathBuf,
    publication: Option<Arc<Mutex<PublicationTrace>>>,
    control_race: Option<OldProbe>,
    prepare_race: Option<OldProbe>,
    pending_race: Option<OldProbe>,
    prepare_source_change: bool,
}
impl FakeHost {
    fn new(intent: &Intent) -> Self {
        Self {
            evidence: intent.clone(),
            hosted: Hosted::Legacy,
            token_live: true,
            o_ready: true,
            drained: true,
            source_exclusive: true,
            probes: VecDeque::from([
                OldProbe::Alive(Box::new(intent.old.clone()), READY.into()),
                OldProbe::Alive(Box::new(intent.old.clone()), READY.into()),
            ]),
            default_probe: OldProbe::AbsentNoReplacement,
            calls: Vec::new(),
            cas: Cas::Written,
            created: Create::Acknowledged,
            bad_at: None,
            store_dir: PathBuf::new(),
            publication: None,
            control_race: None,
            prepare_race: None,
            pending_race: None,
            prepare_source_change: false,
        }
    }
    fn count(&self, kind: &str) -> usize {
        self.calls.iter().filter(|s| s.as_str() == kind).count()
    }
    fn assert_zero(&self) {
        assert_eq!(self.count("exit"), 0);
        assert_eq!(self.count("prepare"), 0);
        assert_eq!(self.count("pending"), 0);
        assert_eq!(self.count("create"), 0);
    }
    fn block_after_exit(&mut self, probe: OldProbe) {
        self.default_probe = probe;
    }
}
impl Host for FakeHost {
    fn observe(&mut self, _intent: &Intent, effect: Effect) -> Result<Observation, String> {
        let i = &self.evidence;
        Ok(Observation {
            owner: i.owner.clone(),
            canonical_row: i.canonical_row,
            hosted_raw: i.hosted_raw.clone(),
            revision: i.revision.clone(),
            source: i.source.clone(),
            request_key: i.request_key.clone(),
            marker_nonce: match &self.hosted {
                Hosted::Pending(nonce) | Hosted::Bound(nonce) => nonce.clone(),
                _ => i.old.nonce.clone(),
            },
            source_exclusive: self.source_exclusive,
            token_live: self.token_live && self.bad_at != Some(effect),
            o_ready: self.o_ready,
            prior_responsibility_drained: self.drained,
            hosted: self.hosted.clone(),
        })
    }
    fn probe_old(&mut self, _intent: &Intent) -> Result<OldProbe, String> {
        self.calls.push("probe".into());
        Ok(self
            .probes
            .pop_front()
            .unwrap_or_else(|| self.default_probe.clone()))
    }
    fn control(
        &mut self,
        intent: &Intent,
        control: MigrationControl,
        prewrite: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        assert_eq!(control, MigrationControl::Exit);
        if let Some(replacement) = self.control_race.take() {
            self.probes.push_front(replacement);
        }
        prewrite(self)?;
        assert_publication_synced(
            self.publication
                .as_ref()
                .expect("store publication observer"),
            &Phase::ExitMayHaveBeenSent,
        );
        let bytes = fs::read(self.store_dir.join("receipt.json"));
        assert!(
            bytes.is_ok(),
            "exit requires durable receipt before transport write"
        );
        let receipt: Receipt = serde_json::from_slice(&bytes.unwrap()).unwrap();
        assert_eq!(receipt.intent, *intent);
        assert_eq!(
            receipt.state,
            Phase::ExitMayHaveBeenSent,
            "exit must have durable attempt evidence"
        );
        self.calls.push("exit".into());
        Ok(())
    }
    fn prepare(&mut self, intent: &Intent, nonce: &str) -> Result<(), String> {
        assert_ne!(nonce, intent.old.nonce);
        assert_eq!(intent.source.source.session_id, SID);
        assert_eq!(
            self.default_probe,
            OldProbe::AbsentNoReplacement,
            "prepare before confirmed old death"
        );
        self.calls.push("prepare".into());
        if let Some(probe) = self.prepare_race.take() {
            self.default_probe = probe;
        }
        if self.prepare_source_change {
            fs::write(&intent.source.source.path, "[]\n").unwrap();
        }
        Ok(())
    }
    fn install_pending(&mut self, _intent: &Intent, nonce: &str) -> Result<Cas, String> {
        self.calls.push("pending".into());
        if self.cas == Cas::Written {
            self.hosted = Hosted::Pending(nonce.into());
        }
        if let Some(probe) = self.pending_race.take() {
            self.default_probe = probe;
        }
        Ok(self.cas)
    }
    fn create(&mut self, _intent: &Intent, nonce: &str) -> Result<Create, String> {
        assert_eq!(self.hosted, Hosted::Pending(nonce.into()));
        self.calls.push("create".into());
        Ok(self.created)
    }
}
fn run(rig: &Rig, host: &mut FakeHost) -> Outcome {
    host.store_dir = rig.store.dir.clone();
    host.publication = Some(rig.store.publication.clone());
    drive(&rig.store, &rig.intent, host)
}
fn held(outcome: Outcome) {
    assert!(matches!(outcome, Outcome::Hold(_)), "{outcome:?}");
}

#[test]
fn exact_idle_old_exits_once_and_death_precedes_every_new_effect() {
    let rig = Rig::new();
    let mut host = rig.host();
    let Outcome::Created(nonce) = run(&rig, &mut host) else {
        panic!("expected create")
    };
    assert_ne!(nonce, rig.intent.old.nonce);
    assert_eq!(host.count("exit"), 1);
    assert_eq!(host.count("prepare"), 1);
    assert_eq!(host.count("pending"), 1);
    assert_eq!(host.count("create"), 1);
    assert_eq!(
        rig.store.read().unwrap().unwrap().state,
        Phase::Pending {
            nonce: nonce.clone()
        }
    );
    assert_eq!(run(&rig, &mut host), Outcome::ExistingPending(nonce));
    assert_eq!(host.count("exit"), 1);
    assert_eq!(host.count("create"), 1);
}

#[test]
fn busy_draft_modal_and_unknown_old_never_send_any_control() {
    let cases = [
        "✳ Architecting…\n────────────────────\n❯ \n────────────────────\n",
        "────────────────────\n❯ a person's draft\n────────────────────\n",
        "Do you want to proceed?\n1. Yes\n2. No\n",
        "capture unavailable",
    ];
    for capture in cases {
        let rig = Rig::new();
        let mut host = rig.host();
        host.probes = VecDeque::from([OldProbe::Alive(
            Box::new(rig.intent.old.clone()),
            capture.into(),
        )]);
        held(run(&rig, &mut host));
        host.assert_zero();
        assert!(rig.store.read().unwrap().is_none());
    }
    for probe in [
        OldProbe::Unknown,
        OldProbe::Replaced,
        OldProbe::AbsentNoReplacement,
    ] {
        let rig = Rig::new();
        let mut host = rig.host();
        host.probes = VecDeque::from([probe]);
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn owner_source_row_revision_request_and_responsibility_are_not_name_only_authority() {
    for case in 0..14 {
        let rig = Rig::new();
        let mut host = rig.host();
        match case {
            0 => host.evidence.owner.runtime_root.push("other-root"),
            1 => host.evidence.owner.channel += 1,
            2 => host.evidence.owner.token_hash.push('x'),
            3 => host.evidence.owner.provider = "codex".into(),
            4 => host.evidence.owner.logical_key.push('x'),
            5 => host.evidence.canonical_row += 1,
            6 => host.evidence.source.source.ino += 1,
            7 => host.evidence.revision.clear_generation += 1,
            8 => host.evidence.request_key.push('x'),
            9 => host.token_live = false,
            10 => host.o_ready = false,
            11 => host.drained = false,
            12 => host.evidence.old.nonce.push('x'),
            13 => host.source_exclusive = false,
            _ => unreachable!(),
        }
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn nonce_pid_start_and_each_physical_pane_field_are_exact() {
    for case in 0..9 {
        let rig = Rig::new();
        let mut host = rig.host();
        let mut old = rig.intent.old.clone();
        match case {
            0 => old.nonce.push('x'),
            1 => old.provider_pid += 1,
            2 => old.starttime = Some(101),
            3 => old.macos_lstart_hash = Some(77),
            n => old.physical_pane[n - 4].push('x'),
        }
        host.probes = VecDeque::from([OldProbe::Alive(Box::new(old), READY.into())]);
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn last_prewrite_probe_rejects_a_replaced_incarnation_after_receipt() {
    let rig = Rig::new();
    let mut host = rig.host();
    let mut replacement = rig.intent.old.clone();
    replacement.starttime = Some(101);
    host.probes[1] = OldProbe::Alive(Box::new(replacement), READY.into());
    held(run(&rig, &mut host));
    host.assert_zero();
    assert_eq!(
        rig.store.read().unwrap().unwrap().state,
        Phase::ExitMayHaveBeenSent
    );
    host.probes.clear();
    host.default_probe = OldProbe::Alive(Box::new(rig.intent.old.clone()), READY.into());
    assert_eq!(run(&rig, &mut host), Outcome::WaitingForOldExit);
    assert_eq!(
        host.count("exit"),
        0,
        "possibly sent means no retry even if the last check held"
    );
}

#[test]
fn alive_replaced_or_unknown_after_exit_never_prepare_or_create_and_never_kill() {
    for probe in [
        OldProbe::Alive(Box::new(Rig::new().intent.old), READY.into()),
        OldProbe::Replaced,
        OldProbe::Unknown,
    ] {
        let rig = Rig::new();
        let mut host = rig.host();
        host.block_after_exit(probe);
        let out = run(&rig, &mut host);
        assert!(matches!(out, Outcome::WaitingForOldExit | Outcome::Hold(_)));
        assert_eq!(host.count("exit"), 1);
        for effect in ["prepare", "pending", "create", "kill"] {
            assert_eq!(host.count(effect), 0);
        }
        for _ in 0..3 {
            let _ = run(&rig, &mut host);
        }
        assert_eq!(host.count("exit"), 1);
        assert_eq!(host.count("create"), 0);
    }
}

#[test]
fn receipt_before_send_survives_restart_without_resending_exit() {
    let rig = Rig::new();
    let mut host = rig.host();
    rig.store
        .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
        .unwrap();
    host.probes.clear();
    host.default_probe = OldProbe::Alive(Box::new(rig.intent.old.clone()), READY.into());
    assert_eq!(run(&rig, &mut host), Outcome::WaitingForOldExit);
    host.assert_zero();
    host.default_probe = OldProbe::AbsentNoReplacement;
    assert!(matches!(run(&rig, &mut host), Outcome::Created(_)));
    assert_eq!(host.count("exit"), 0);
    assert_eq!(host.count("create"), 1);
}

#[test]
fn old_absent_restart_reuses_successor_nonce_and_pending_never_creates_twice() {
    let rig = Rig::new();
    let mut host = rig.host();
    let nonce = SUCCESSOR.to_string();
    rig.store
        .publish(
            &rig.intent,
            Phase::OldAbsent {
                nonce: nonce.clone(),
            },
        )
        .unwrap();
    host.probes.clear();
    assert_eq!(run(&rig, &mut host), Outcome::Created(nonce.clone()));
    assert_eq!(
        run(&rig, &mut host),
        Outcome::ExistingPending(nonce.clone())
    );
    assert_eq!(host.count("exit"), 0);
    assert_eq!(host.count("create"), 1);
    host.hosted = Hosted::Bound(nonce.clone());
    assert_eq!(run(&rig, &mut host), Outcome::AlreadyBound(nonce));
    assert_eq!(host.count("create"), 1);
}

#[test]
fn pending_cas_stale_unknown_and_create_ambiguity_are_not_fresh_retries() {
    for cas in [Cas::Stale, Cas::Unknown] {
        let rig = Rig::new();
        let mut host = rig.host();
        host.cas = cas;
        held(run(&rig, &mut host));
        assert_eq!(host.count("create"), 0);
        assert_eq!(host.count("exit"), 1);
    }
    for created in [Create::Unknown, Create::NotSent] {
        let rig = Rig::new();
        let mut host = rig.host();
        host.created = created;
        held(run(&rig, &mut host));
        assert!(matches!(run(&rig, &mut host), Outcome::ExistingPending(_)));
        assert_eq!(host.count("create"), 1);
        assert_eq!(host.count("exit"), 1);
    }
    let rig = Rig::new();
    let mut host = rig.host();
    host.hosted = Hosted::Unknown;
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn pending_receipt_with_legacy_row_is_uncertain_and_does_not_send_or_create() {
    let rig = Rig::new();
    let mut host = rig.host();
    rig.store
        .publish(
            &rig.intent,
            Phase::Pending {
                nonce: SUCCESSOR.into(),
            },
        )
        .unwrap();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn every_durability_failure_prevents_control_even_if_rename_is_visible() {
    for step in RECEIPT_STEPS {
        let mut rig = Rig::new();
        let mut host = rig.host();
        rig.store.fail = Some(step);
        held(run(&rig, &mut host));
        host.assert_zero();
        let trace = rig
            .store
            .publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let failed = RECEIPT_STEPS
            .iter()
            .position(|candidate| *candidate == step)
            .unwrap();
        assert_eq!(trace.operations.len(), failed + 1);
        assert_eq!(trace.operations.last(), Some(&(step, false)));
        drop(trace);
        rig.store.fail = None;
        if matches!(
            step,
            Step::ParentSync | Step::MigrationSync | Step::RootSync
        ) {
            host.probes.clear();
            host.default_probe = OldProbe::Alive(Box::new(rig.intent.old.clone()), READY.into());
            assert_eq!(run(&rig, &mut host), Outcome::WaitingForOldExit);
            assert_eq!(host.count("exit"), 0);
        }
    }
}

#[test]
fn corrupt_unknown_schema_symlink_and_wrong_owner_receipts_are_holds() {
    for bytes in ["{", "{\"schema\":9}", "null"] {
        let rig = Rig::new();
        let mut host = rig.host();
        fs::write(rig.store.dir.join("receipt.json"), bytes).unwrap();
        held(run(&rig, &mut host));
        host.assert_zero();
    }
    let rig = Rig::new();
    let mut host = rig.host();
    let mut old = rig.intent.clone();
    old.owner.channel += 1;
    rig.store.publish(&old, Phase::ExitMayHaveBeenSent).unwrap();
    held(run(&rig, &mut host));
    host.assert_zero();
    let rig = Rig::new();
    let mut host = rig.host();
    std::os::unix::fs::symlink(
        rig.root.path().join(format!("{SID}.jsonl")),
        rig.store.dir.join("receipt.json"),
    )
    .unwrap();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn clear_and_resume_invalidate_without_waiting_for_long_handoff_lock() {
    for cause in [Invalidation::Clear, Invalidation::Resume] {
        let rig = Rig::new();
        let mut host = rig.host();
        rig.store
            .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
            .unwrap();
        let long = rig.store.lock("handoff.lock").unwrap();
        let mut committed = false;
        rig.store
            .with_selector_change(cause.clone(), || {
                assert_publication_synced(
                    &rig.store.publication,
                    &Phase::Invalidated {
                        cause: cause.clone(),
                    },
                );
                committed = true;
                Ok(())
            })
            .unwrap();
        assert!(
            committed,
            "selector commit must follow every successful sync"
        );
        drop(long);
        assert_eq!(
            rig.store.read().unwrap().unwrap().state,
            Phase::Invalidated { cause }
        );
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn short_effect_gate_excludes_clear_during_a_write_but_not_during_exit_wait() {
    let rig = Rig::new();
    rig.store
        .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
        .unwrap();
    let gate = rig.store.lock("effect.lock").unwrap();
    assert!(
        rig.store
            .with_selector_change(Invalidation::Clear, || Ok(()))
            .is_err()
    );
    assert_eq!(
        rig.store.read().unwrap().unwrap().state,
        Phase::ExitMayHaveBeenSent
    );
    drop(gate);
    let mut host = rig.host();
    host.probes.clear();
    host.default_probe = OldProbe::Alive(Box::new(rig.intent.old.clone()), READY.into());
    assert_eq!(run(&rig, &mut host), Outcome::WaitingForOldExit);
    rig.store
        .with_selector_change(Invalidation::Clear, || Ok(()))
        .unwrap();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn token_change_before_prepare_pending_and_create_blocks_that_effect() {
    for effect in [
        Effect::Exit,
        Effect::Prepare,
        Effect::Pending,
        Effect::Create,
    ] {
        let rig = Rig::new();
        let mut host = rig.host();
        host.bad_at = Some(effect);
        held(run(&rig, &mut host));
        let name = match effect {
            Effect::Exit => "exit",
            Effect::Prepare => "prepare",
            Effect::Pending => "pending",
            Effect::Create => "create",
        };
        assert_eq!(host.count(name), 0, "{effect:?}");
    }
}

#[test]
fn clear_aba_and_source_prefix_or_inode_change_are_not_the_same_readset() {
    for case in 0..5 {
        let rig = Rig::new();
        let mut host = rig.host();
        rig.store
            .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
            .unwrap();
        host.probes.clear();
        match case {
            0 => host.evidence.revision.clear_generation += 2,
            1 => host.evidence.revision.cwd.push("new-project"),
            2 => host.evidence.source.prefix_sha256.replace_range(0..1, "b"),
            3 => host.evidence.source.prefix_eof -= 1,
            4 => host.evidence.source.source.ino += 1,
            _ => unreachable!(),
        }
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn root_and_lock_symlinks_and_incomplete_intents_are_refused() {
    let rig = Rig::new();
    let mut owner = rig.intent.owner.clone();
    owner.runtime_root.push("other");
    assert!(Store::open(rig.root.path(), &owner).is_err());
    std::os::unix::fs::symlink(
        rig.root.path().join(format!("{SID}.jsonl")),
        rig.store.dir.join("handoff.lock"),
    )
    .unwrap();
    let mut host = rig.host();
    held(run(&rig, &mut host));
    host.assert_zero();
    for case in 0..5 {
        let mut rig = Rig::new();
        let mut host = rig.host();
        match case {
            0 => rig.intent.old.starttime = None,
            1 => rig.intent.old.provider_pid = 0,
            2 => rig.intent.source.source.session_id = "bad-selector".into(),
            3 => rig.intent.source.source.path = "relative".into(),
            4 => rig.intent.source.prefix_sha256.clear(),
            _ => unreachable!(),
        }
        held(run(&rig, &mut host));
        host.assert_zero();
    }
}

#[test]
fn os_lock_excludes_an_independent_process() {
    const ENV: &str = "AGENTDESK_TEST_I7503_HANDOFF_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        let root = PathBuf::from(path);
        let owner: Owner =
            serde_json::from_slice(&fs::read(root.join("owner.json")).unwrap()).unwrap();
        let store = Store::open(&root, &owner).unwrap();
        let _lock = store.lock("handoff.lock").unwrap();
        fs::write(root.join("child-ready"), "locked").unwrap();
        let start = Instant::now();
        while !root.join("child-release").exists() {
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        return;
    }
    let rig = Rig::new();
    fs::write(
        rig.root.path().join("owner.json"),
        serde_json::to_vec(&rig.intent.owner).unwrap(),
    )
    .unwrap();
    let name = format!(
        "{}::os_lock_excludes_an_independent_process",
        module_path!().split_once("::").unwrap().1
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(ENV, rig.root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !rig.root.path().join("child-ready").exists() {
        if start.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not acquire real lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut host = rig.host();
    let outcome = run(&rig, &mut host);
    let invalidation = rig
        .store
        .with_selector_change(Invalidation::Clear, || Ok(()));
    fs::write(rig.root.path().join("child-release"), "release").unwrap();
    assert!(child.wait().unwrap().success());
    held(outcome);
    host.assert_zero();
    invalidation.unwrap();
    assert!(rig.store.lock("handoff.lock").is_ok());
}

#[test]
fn canonical_pending_and_bound_need_the_receipts_exact_successor_nonce() {
    for bound in [false, true] {
        for receipt in [
            None,
            Some(Phase::ExitMayHaveBeenSent),
            Some(Phase::OldAbsent {
                nonce: SUCCESSOR.into(),
            }),
            Some(Phase::Pending {
                nonce: SUCCESSOR.into(),
            }),
        ] {
            let rig = Rig::new();
            let mut host = rig.host();
            if let Some(state) = receipt {
                rig.store.publish(&rig.intent, state).unwrap();
            }
            host.hosted = if bound {
                Hosted::Bound(OTHER_NONCE.into())
            } else {
                Hosted::Pending(OTHER_NONCE.into())
            };
            held(run(&rig, &mut host));
            host.assert_zero();
        }
        for receipt in [
            Phase::OldAbsent {
                nonce: SUCCESSOR.into(),
            },
            Phase::Pending {
                nonce: SUCCESSOR.into(),
            },
        ] {
            let rig = Rig::new();
            let mut host = rig.host();
            rig.store.publish(&rig.intent, receipt).unwrap();
            host.hosted = if bound {
                Hosted::Bound(SUCCESSOR.into())
            } else {
                Hosted::Pending(SUCCESSOR.into())
            };
            assert_eq!(
                run(&rig, &mut host),
                if bound {
                    Outcome::AlreadyBound(SUCCESSOR.into())
                } else {
                    Outcome::ExistingPending(SUCCESSOR.into())
                }
            );
            host.assert_zero();
        }
    }
}

#[test]
fn engine_reads_actual_prefix_identity_length_and_complete_jsonl_before_effects() {
    for case in 0..7 {
        let rig = Rig::new();
        let mut host = rig.host();
        let path = &rig.intent.source.source.path;
        match case {
            0 => {
                fs::write(path, "[]\n").unwrap();
            }
            1 => {
                fs::write(path, "").unwrap();
            }
            2 => {
                fs::write(path, "{}\n{\"torn\":").unwrap();
            }
            3 => {
                fs::write(path, "{}\ninvalid\n").unwrap();
            }
            4 => {
                let replacement = path.with_extension("new");
                fs::write(&replacement, "{}\n").unwrap();
                fs::rename(replacement, path).unwrap();
            }
            5 => {
                fs::remove_file(path).unwrap();
            }
            6 => {
                fs::remove_file(path).unwrap();
                std::os::unix::fs::symlink(rig.root.path().join("missing"), path).unwrap();
            }
            _ => unreachable!(),
        }
        held(run(&rig, &mut host));
        host.assert_zero();
        assert!(rig.store.read().unwrap().is_none());
    }
    let rig = Rig::new();
    let mut host = rig.host();
    OpenOptions::new()
        .append(true)
        .open(&rig.intent.source.source.path)
        .unwrap()
        .write_all(b"{\"type\":\"file-history-snapshot\"}\n")
        .unwrap();
    assert!(
        matches!(run(&rig, &mut host), Outcome::Created(_)),
        "complete metadata append preserves original prefix"
    );
    let mut rig = Rig::new();
    let mut host = rig.host();
    rig.intent.source.prefix_eof = 2;
    host.evidence = rig.intent.clone();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn actual_prefix_change_after_prepare_blocks_pending_and_create() {
    let rig = Rig::new();
    let mut host = rig.host();
    host.prepare_source_change = true;
    held(run(&rig, &mut host));
    assert_eq!(host.count("prepare"), 1);
    assert_eq!(host.count("pending"), 0);
    assert_eq!(host.count("create"), 0);
}

#[test]
fn replacement_after_prepare_or_pending_is_reprobed_before_next_effect() {
    for after_pending in [false, true] {
        let rig = Rig::new();
        let mut host = rig.host();
        if after_pending {
            host.pending_race = Some(OldProbe::Replaced);
        } else {
            host.prepare_race = Some(OldProbe::Replaced);
        }
        held(run(&rig, &mut host));
        assert_eq!(host.count("prepare"), 1);
        assert_eq!(host.count("pending"), usize::from(after_pending));
        assert_eq!(host.count("create"), 0);
    }
}

#[test]
fn transport_prewrite_callback_catches_identity_change_after_engine_preflight() {
    let rig = Rig::new();
    let mut host = rig.host();
    let mut changed = rig.intent.old.clone();
    changed.provider_pid += 1;
    host.control_race = Some(OldProbe::Alive(Box::new(changed), READY.into()));
    held(run(&rig, &mut host));
    host.assert_zero();
    assert_eq!(
        rig.store.read().unwrap().unwrap().state,
        Phase::ExitMayHaveBeenSent
    );
}

#[test]
fn selector_commit_with_no_receipt_is_fenced_and_clear_aba_stays_invalid() {
    let rig = Rig::new();
    let mut host = rig.host();
    rig.store
        .with_selector_change(Invalidation::Clear, || {
            held(run(&rig, &mut host));
            host.assert_zero();
            host.evidence.revision.clear_generation += 1;
            Ok(())
        })
        .unwrap();
    assert!(rig.store.read().unwrap().is_none());
    held(run(&rig, &mut host));
    host.assert_zero();
    host.evidence.revision.cached_sid = None;
    host.evidence.revision.cached_sid = Some(SID.into());
    host.evidence.revision.clear_generation += 1;
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn failed_invalidation_durability_never_enters_the_selector_commit() {
    for step in RECEIPT_STEPS {
        let mut rig = Rig::new();
        rig.store
            .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
            .unwrap();
        rig.store.fail = Some(step);
        let mut committed = false;
        assert!(
            rig.store
                .with_selector_change(Invalidation::Resume, || {
                    committed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!committed);
        let trace = rig
            .store
            .publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            trace.phase,
            Some(Phase::Invalidated {
                cause: Invalidation::Resume
            })
        );
        let failed = RECEIPT_STEPS
            .iter()
            .position(|candidate| *candidate == step)
            .unwrap();
        assert_eq!(trace.operations.len(), failed + 1);
        assert_eq!(trace.operations.last(), Some(&(step, false)));
    }
}

#[test]
fn new_receipts_and_locks_are_private_without_chmod_of_existing_directories() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let rig = Rig::new();
    let mut host = rig.host();
    assert_eq!(fs::metadata(&rig.store.dir).unwrap().mode() & 0o777, 0o700);
    fs::set_permissions(&rig.store.dir, fs::Permissions::from_mode(0o755)).unwrap();
    let reopened = Store::open(rig.root.path(), &rig.intent.owner).unwrap();
    assert_eq!(fs::metadata(&reopened.dir).unwrap().mode() & 0o777, 0o755);
    assert!(matches!(run(&rig, &mut host), Outcome::Created(_)));
    for name in ["receipt.json", "effect.lock", "handoff.lock"] {
        assert_eq!(
            fs::metadata(rig.store.dir.join(name)).unwrap().mode() & 0o777,
            0o600
        );
    }
    let other = Owner {
        channel: 7504,
        ..rig.intent.owner.clone()
    };
    let other_store = Store::open(rig.root.path(), &other).unwrap();
    held(drive(&other_store, &rig.intent, &mut host));
}

#[test]
fn malformed_or_reused_successor_nonce_is_held_even_when_the_canonical_nonce_matches() {
    for nonce in [
        "not-a-nonce",
        OLD_NONCE,
        "11111111-1111-4111-8111-111111111111",
    ] {
        for pending in [false, true] {
            for state in [
                Phase::OldAbsent {
                    nonce: nonce.into(),
                },
                Phase::Pending {
                    nonce: nonce.into(),
                },
            ] {
                let rig = Rig::new();
                let mut host = rig.host();
                rig.store.publish(&rig.intent, state).unwrap();
                host.hosted = if pending {
                    Hosted::Pending(nonce.into())
                } else {
                    Hosted::Bound(nonce.into())
                };
                held(run(&rig, &mut host));
                host.assert_zero();
            }
        }
    }
    let mut rig = Rig::new();
    let mut host = rig.host();
    rig.intent.old.nonce = "not-a-nonce".into();
    host.evidence = rig.intent.clone();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn stable_parent_alias_preserves_exact_source_and_retarget_or_leaf_symlinks_are_held() {
    use std::os::unix::fs::symlink;
    let mut rig = Rig::new();
    let original = rig.intent.source.source.path.clone();
    let alias = rig.root.path().join("source-alias");
    symlink(original.parent().unwrap(), &alias).unwrap();
    rig.intent.source.source.path = alias.join(original.file_name().unwrap());
    let exact = rig.intent.source.source.path.clone();
    let mut host = rig.host();
    assert!(matches!(run(&rig, &mut host), Outcome::Created(_)));
    assert_eq!(
        rig.store.read().unwrap().unwrap().intent.source.source.path,
        exact
    );

    let mut rig = Rig::new();
    let original = rig.intent.source.source.path.clone();
    let alias = rig.root.path().join("source-alias");
    symlink(original.parent().unwrap(), &alias).unwrap();
    rig.intent.source.source.path = alias.join(original.file_name().unwrap());
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join(original.file_name().unwrap()), "{}\n").unwrap();
    fs::remove_file(&alias).unwrap();
    symlink(other.path(), &alias).unwrap();
    let mut host = rig.host();
    held(run(&rig, &mut host));
    host.assert_zero();

    let mut rig = Rig::new();
    let link = rig.root.path().join("leaf-link.jsonl");
    symlink(&rig.intent.source.source.path, &link).unwrap();
    rig.intent.source.source.path = link;
    let mut host = rig.host();
    held(run(&rig, &mut host));
    host.assert_zero();
}

#[test]
fn one_logical_execution_shares_locks_but_never_shares_receipt_authority() {
    let rig = Rig::new();
    let mut other = rig.intent.owner.clone();
    other.token_hash = "another-token".into();
    let store = Store::open(rig.root.path(), &other).unwrap();
    assert_eq!(
        rig.store.dir, store.dir,
        "handoff scope is root plus logical execution"
    );
    let mut intent = rig.intent.clone();
    intent.owner = other;
    intent.canonical_row += 1;
    let mut host = FakeHost::new(&intent);
    host.store_dir = store.dir.clone();
    assert!(
        validate(
            &intent,
            &host.observe(&intent, Effect::Exit).unwrap(),
            &Hosted::Legacy
        )
        .is_ok()
    );
    let lock = rig.store.lock("handoff.lock").unwrap();
    held(drive(&store, &intent, &mut host));
    host.assert_zero();
    drop(lock);
    let lock = rig.store.lock("effect.lock").unwrap();
    let mut committed = false;
    assert!(
        store
            .with_selector_change(Invalidation::Clear, || {
                committed = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!committed);
    drop(lock);
    rig.store
        .publish(&rig.intent, Phase::ExitMayHaveBeenSent)
        .unwrap();
    let before = fs::read(rig.store.dir.join("receipt.json")).unwrap();
    assert!(
        store
            .with_selector_change(Invalidation::Resume, || {
                committed = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!committed);
    assert_eq!(
        fs::read(rig.store.dir.join("receipt.json")).unwrap(),
        before
    );
    held(drive(&store, &intent, &mut host));
    host.assert_zero();
}
