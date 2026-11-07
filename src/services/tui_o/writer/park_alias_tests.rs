//! A durable empty-to-filled alias can name a parked predecessor, but cannot bypass target checks.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::cutover;
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::adoption::{LegacyView, NoLegacy};
use crate::services::tui_o::writer::host::{
    self, Custody, HostIo, HostParts, ManagedWriterHandle, Readiness,
};
use crate::services::tui_o::writer::pieces::UnitDeriver;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::time::Duration;

const REPLY: &str = "first reply after a parked empty-session alias";

struct AliasIo {
    port: Arc<FakePort>,
    lease: Arc<FakeLease>,
    alarms: Alarms,
    bindings: Arc<FakeBindings>,
    absent: AtomicBool,
    probes: AtomicUsize,
    /// Actor starts: the host asks for one re-post turn per start.
    boots: AtomicUsize,
}

impl HostIo for AliasIo {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Alarms;
    type Bindings = FakeBindings;

    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        std::future::ready(self.port.clone())
    }

    fn lease(&self) -> Self::Lease {
        self.lease.clone()
    }

    fn alarms(&self) -> Alarms {
        self.alarms.clone()
    }

    fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<FakeBindings> {
        self.bindings.clone()
    }

    fn producer_observation(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> impl Future<Output = Option<crate::services::tui_o::writer::ProducerObservation>> + Send
    {
        use crate::services::tui_o::writer::{ProducerObservation, ProducerPresence};
        self.probes.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Some(ProducerObservation {
            presence: if self.absent.load(Ordering::SeqCst) {
                ProducerPresence::Absent
            } else {
                ProducerPresence::Unknown
            },
            incarnation: None,
        }))
    }

    fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        std::future::ready(Ok(ActivationFacts::default()))
    }

    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
        Ok(Custody::Free)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        Arc::new(NoLegacy)
    }

    fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }

    fn relaying(&self, _: u64) -> bool {
        false
    }

    fn repost(&self, _: u64, _: &Arc<FakePort>) -> Option<host::RepostTurn<Self>> {
        self.boots.fetch_add(1, Ordering::SeqCst);
        None
    }
}

fn aliases(confirmed: bool) -> (Harness, SourceId, SourceId, Arc<FakeBindings>) {
    let (h, path, empty, bindings) = started_empty(b"");
    let filled = named(&empty, "A");
    let mut writer = h.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let (mut deriver, mut owed) = (
        UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        VecDeque::new(),
    );
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    append(
        &path,
        b"{\"type\":\"progress\",\"uuid\":\"alias-retained-evidence\"}\n",
    );
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    if confirmed {
        bindings.commit(resume(2, &empty, &filled));
        sources.follow(&mut writer).unwrap();
    }
    assert!(owed.is_empty() && h.port.posts().is_empty());
    assert_eq!(writer.store().cursors().count(), 1);
    assert!(writer.store().cursor(&empty).is_some());
    assert!(writer.store().cursor(&filled).is_none());
    assert_eq!(
        writer.store().binding_checkpoint().unwrap(),
        Some(if confirmed { 2 } else { 1 })
    );
    drop((sources, writer, deriver, owed));
    (h, empty, filled, bindings)
}

fn retained(h: &Harness, source: &SourceId) -> BTreeMap<PathBuf, Vec<u8>> {
    let base = h._runtime.path().join("o_store").join(CHANNEL.to_string());
    let key = source_key(source);
    let mut files = BTreeMap::new();
    for directory in ["cursor", "spool"] {
        for entry in std::fs::read_dir(base.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&key)
            {
                files.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    assert!(files.values().any(|bytes| {
        bytes
            .windows(23)
            .any(|part| part == b"alias-retained-evidence")
    }));
    files
}

fn halts(h: &Harness) -> usize {
    h.alarms
        .raised
        .lock()
        .unwrap()
        .iter()
        .filter(|alarm| {
            matches!(
                alarm,
                WriterAlarm::Halted { .. } | WriterAlarm::HaltedMissingSource { .. }
            )
        })
        .count()
}

fn hosted(h: &Harness, io: &Arc<AliasIo>, ready: &Arc<Readiness>) -> ManagedWriterHandle {
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: io.clone(),
            runtime_root: Some(h._runtime.path().to_path_buf()),
            gate: h.gate.clone(),
            readiness: ready.clone(),
        },
    );
    assert_eq!(handles.len(), 1);
    handles.pop().unwrap()
}

async fn until(h: &Harness, predicate: impl Fn() -> bool) {
    past(h, 0, predicate).await;
}

/// Polls until `predicate` holds or more than `seen` halts were raised.
async fn past(h: &Harness, seen: usize, predicate: impl Fn() -> bool) {
    for _ in 0..120 {
        if predicate() || halts(h) > seen {
            return;
        }
        polls(1).await;
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "alias host made no progress");
}

#[tokio::test(start_paused = true)]
async fn parked_confirmed_empty_session_old_alias_posts_fresh_reply_once_across_restart() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::parked_confirmed_empty_session_old_alias_posts_fresh_reply_once_across_restart"
    )) {
        return;
    }
    let (h, empty, filled, bindings) = aliases(true);
    let original = retained(&h, &empty);
    let cursor = h.channel().cursor(&empty).unwrap().clone();
    let serial = h.channel().ledger().next_serial();
    std::fs::remove_file(&empty.path).unwrap();
    let _selected = cutover::test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let io = Arc::new(AliasIo {
        port: h.port.clone(),
        lease: h.lease.clone(),
        alarms: h.alarms.clone(),
        bindings: bindings.clone(),
        absent: AtomicBool::new(true),
        probes: AtomicUsize::new(0),
        boots: AtomicUsize::new(0),
    });
    let ready = Arc::new(Readiness::default());
    let first = hosted(&h, &io, &ready);
    until(&h, || ready.accepts(CHANNEL)).await;
    assert!(
        ready.accepts(CHANNEL),
        "the missing confirmed alias must park"
    );
    assert!(io.probes.load(Ordering::SeqCst) > 0);
    let parked = ready
        .undelivered_after(CHANNEL, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!((parked.uncaptured, parked.binding_pending), (0, 0));
    assert!(
        parked.captured.is_empty(),
        "the missing nonretired source is parked"
    );
    assert_eq!(h.channel().cursor(&empty).unwrap(), &cursor);
    assert_eq!(retained(&h, &empty), original);
    let (path, fresh) = transcript(&empty.path, "b.jsonl", "B", &row("alias-fresh", REPLY));
    bindings.commit(bound(
        3,
        Some(&filled),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Startup,
        None,
    ));
    until(&h, || {
        h.channel().binding_checkpoint().unwrap() == Some(3) && h.port.posts().len() == 1
    })
    .await;
    assert_eq!(
        h.channel().binding_checkpoint().unwrap(),
        Some(3),
        "a parked confirmed old alias must not reopen its missing file"
    );
    let store = h.channel();
    assert!(store.cursor(&fresh).is_some());
    assert_eq!(store.cursors().count(), 2);
    assert!(matches!(
        store
            .ledger()
            .piece(serial)
            .and_then(|piece| piece.outcome.as_ref()),
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(store.ledger().next_serial(), serial + 1);
    assert_eq!(h.port.posts(), [REPLY]);
    drop(store);
    for _ in 0..5 {
        polls(2).await;
        assert!(first.live() && ready.accepts(CHANNEL));
        assert_eq!(
            halts(&h),
            0,
            "a successful old alias must not restart its host"
        );
        assert_eq!(h.channel().cursor(&empty).unwrap(), &cursor);
        assert_eq!(retained(&h, &empty), original);
    }
    let debt = ready
        .undelivered_after(CHANNEL, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        (
            debt.owed,
            debt.prepared,
            debt.unsealed,
            debt.uncaptured,
            debt.binding_pending,
            debt.verified_work,
            debt.posting
        ),
        (0, 0, 0, 0, 0, 0, 0)
    );
    assert!(
        debt.captured.iter().any(|(source, through)| source == &path
            && *through >= std::fs::metadata(&path).unwrap().len())
    );
    first.stop_and_join().await.unwrap();
    io.absent.store(false, Ordering::SeqCst);
    let restarted_ready = Arc::new(Readiness::default());
    let restarted = hosted(&h, &io, &restarted_ready);
    until(&h, || restarted_ready.accepts(CHANNEL)).await;
    assert!(restarted_ready.accepts(CHANNEL));
    let debt = restarted_ready
        .undelivered_after(CHANNEL, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        (debt.owed, debt.uncaptured, debt.binding_pending),
        (0, 0, 0)
    );
    polls(5).await;
    assert_eq!(
        h.port.posts(),
        [REPLY],
        "restart must not repost the first reply"
    );
    assert_eq!(h.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(h.channel().cursor(&empty).unwrap(), &cursor);
    assert_eq!(retained(&h, &empty), original);
    assert_eq!(halts(&h), 0);
    restarted.stop_and_join().await.unwrap();
}

fn missing_engine(
    h: &Harness,
    source: &SourceId,
    bindings: Arc<FakeBindings>,
) -> (Sources<FakeBindings>, Writer) {
    std::fs::remove_file(&source.path).unwrap();
    let mut writer = h.writer();
    let mut sources =
        Sources::new(CHANNEL, ShadowProvider::Claude, bindings).with_producer_absent(true);
    sources
        .resume(
            &mut writer,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        )
        .unwrap();
    assert!(sources.is_parked(source));
    (sources, writer)
}

#[test]
fn parked_unconfirmed_empty_session_old_alias_still_requires_prefix_validation() {
    let (h, empty, filled, bindings) = aliases(false);
    let (_, fresh) = transcript(&empty.path, "b.jsonl", "B", b"");
    let (mut sources, mut writer) = missing_engine(&h, &empty, bindings.clone());
    bindings.commit(bound(
        2,
        Some(&filled),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Startup,
        None,
    ));
    assert!(matches!(
        sources.follow(&mut writer),
        Err(WriterAlarm::HaltedMissingSource { ref source, .. }) if *source == empty
    ));
    assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(1));
    assert!(writer.store().cursor(&fresh).is_none());
}

#[test]
fn parked_empty_session_alias_as_new_or_parent_still_requires_prefix_validation() {
    for parent in [false, true] {
        let (h, empty, filled, bindings) = aliases(true);
        let (_, fresh) = transcript(&empty.path, "b.jsonl", "B", b"");
        let (mut sources, mut writer) = missing_engine(&h, &empty, bindings.clone());
        let target = if parent {
            fresh.clone()
        } else {
            filled.clone()
        };
        bindings.commit(bound(
            3,
            Some(&filled),
            BindingTarget::Source(target),
            BindingCause::Startup,
            parent.then_some(&filled),
        ));
        assert!(
            matches!(sources.follow(&mut writer), Err(WriterAlarm::HaltedMissingSource { ref source, ref detail }) if *source == empty && detail.contains("source reopen")),
            "parent={parent}"
        );
        assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(2));
        assert!(writer.store().cursor(&fresh).is_none());
    }
}

#[test]
fn unparked_confirmed_empty_session_old_alias_still_requires_prefix_validation() {
    let (h, empty, filled, bindings) = aliases(true);
    let (_, fresh) = transcript(&empty.path, "b.jsonl", "B", b"");
    let mut writer = h.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    sources
        .resume(
            &mut writer,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        )
        .unwrap();
    assert!(!sources.is_parked(&empty));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&empty.path)
        .unwrap();
    file.write_all(b"!").unwrap();
    drop(file);
    bindings.commit(bound(
        3,
        Some(&filled),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Startup,
        None,
    ));
    assert!(matches!(
        sources.follow(&mut writer),
        Err(WriterAlarm::Halted { ref detail }) if detail.contains("bytes before the cursor changed")
    ));
    assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(2));
    assert!(writer.store().cursor(&fresh).is_none());
}

#[tokio::test(start_paused = true)]
async fn parked_unconfirmed_old_alias_holds_its_claim_until_its_file_returns() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::parked_unconfirmed_old_alias_holds_its_claim_until_its_file_returns"
    )) {
        return;
    }
    let (h, empty, filled, bindings) = aliases(false);
    let original = retained(&h, &empty);
    let cursor = h.channel().cursor(&empty).unwrap().clone();
    let (_, fresh) = transcript(&empty.path, "b.jsonl", "B", &row("alias-held", REPLY));
    bindings.commit(bound(
        2,
        Some(&filled),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Startup,
        None,
    ));
    let away = empty.path.with_extension("away");
    std::fs::rename(&empty.path, &away).unwrap();
    let _selected = cutover::test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let io = Arc::new(AliasIo {
        port: h.port.clone(),
        lease: h.lease.clone(),
        alarms: h.alarms.clone(),
        bindings: bindings.clone(),
        absent: AtomicBool::new(false),
        probes: AtomicUsize::new(0),
        boots: AtomicUsize::new(0),
    });
    let ready = Arc::new(Readiness::default());
    let host = hosted(&h, &io, &ready);
    let generation = ready.hosted_by(CHANNEL);
    assert!(generation.is_some());
    until(&h, || false).await;
    // The unconfirmed alias cannot be validated while its file is gone: hold, never re-host.
    for _ in 0..5 {
        polls(2).await;
        let at = h.channel().binding_checkpoint().unwrap();
        assert_eq!(
            (ready.hosted_by(CHANNEL), at),
            (generation, Some(1)),
            "a missing unconfirmed alias must keep its claim at the same checkpoint"
        );
        assert_eq!((io.boots.load(Ordering::SeqCst), halts(&h)), (1, 1));
        assert!(host.live() && !ready.accepts(CHANNEL));
        assert!(h.channel().cursor(&fresh).is_none() && h.port.posts().is_empty());
        assert_eq!(h.channel().cursor(&empty).unwrap(), &cursor);
        assert_eq!(retained(&h, &empty), original);
    }
    assert!(matches!(
        h.alarms.raised.lock().unwrap().last(),
        Some(WriterAlarm::HaltedMissingSource { source, .. }) if *source == empty
    ));
    std::fs::rename(&away, &empty.path).unwrap();
    past(&h, 1, || {
        h.port.posts().len() == 1 && ready.accepts(CHANNEL)
    })
    .await;
    for _ in 0..5 {
        polls(2).await;
        assert_eq!(h.port.posts(), [REPLY]);
        assert_eq!(h.channel().binding_checkpoint().unwrap(), Some(2));
        assert_eq!((io.boots.load(Ordering::SeqCst), halts(&h)), (2, 1));
        assert_eq!(ready.hosted_by(CHANNEL), generation);
        assert!(host.live() && ready.accepts(CHANNEL));
    }
    assert!(h.channel().cursor(&fresh).is_some());
    host.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_source_cleared_in_after_ready_and_lost_releases_the_absent_claim() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_source_cleared_in_after_ready_and_lost_releases_the_absent_claim"
    )) {
        return;
    }
    let (h, empty, filled, bindings) = aliases(true);
    std::fs::remove_file(&empty.path).unwrap();
    let _selected = cutover::test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let io = Arc::new(AliasIo {
        port: h.port.clone(),
        lease: h.lease.clone(),
        alarms: h.alarms.clone(),
        bindings: bindings.clone(),
        absent: AtomicBool::new(true),
        probes: AtomicUsize::new(0),
        boots: AtomicUsize::new(0),
    });
    let ready = Arc::new(Readiness::default());
    let host = hosted(&h, &io, &ready);
    until(&h, || ready.accepts(CHANNEL)).await;
    assert!(ready.accepts(CHANNEL));
    // The fresh source is opened by a bind the ready actor follows, so no start cursor names it.
    let (path, fresh) = transcript(&empty.path, "b.jsonl", "B", &row("cleared", REPLY));
    bindings.commit(bound(
        3,
        Some(&filled),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Clear,
        None,
    ));
    until(&h, || h.port.posts().len() == 1).await;
    assert_eq!(h.port.posts(), [REPLY]);
    std::fs::remove_file(&path).unwrap();
    until(&h, || false).await;
    past(&h, 1, || !host.live()).await;
    assert!(matches!(
        h.alarms.raised.lock().unwrap().last(),
        Some(WriterAlarm::HaltedMissingSource { source, .. }) if *source == fresh
    ));
    assert_eq!(
        (ready.hosted_by(CHANNEL), io.boots.load(Ordering::SeqCst)),
        (None, 1),
        "an Absent producer's runtime loss returns the claim for sessionless recovery"
    );
    assert_eq!(h.channel().binding_checkpoint().unwrap(), Some(3));
}

#[tokio::test(start_paused = true)]
async fn a_new_bound_source_missing_before_first_open_holds_unknown_claim() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_new_bound_source_missing_before_first_open_holds_unknown_claim"
    )) {
        return;
    }
    let (h, empty, _, bindings) = aliases(false);
    let (path, fresh) = transcript(&empty.path, "b.jsonl", "B", &row("first-open", REPLY));
    bindings.commit(bound(
        2,
        Some(&empty),
        BindingTarget::Source(fresh.clone()),
        BindingCause::Startup,
        None,
    ));
    let away = path.with_extension("away");
    std::fs::rename(&path, &away).unwrap();
    let _selected = cutover::test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let io = Arc::new(AliasIo {
        port: h.port.clone(),
        lease: h.lease.clone(),
        alarms: h.alarms.clone(),
        bindings: bindings.clone(),
        absent: AtomicBool::new(false),
        probes: AtomicUsize::new(0),
        boots: AtomicUsize::new(0),
    });
    let ready = Arc::new(Readiness::default());
    let host = hosted(&h, &io, &ready);
    let generation = ready.hosted_by(CHANNEL);
    assert!(generation.is_some());
    until(&h, || false).await;
    // An Unknown producer's bound source gone before its first open holds the claim.
    for _ in 0..5 {
        polls(2).await;
        let at = h.channel().binding_checkpoint().unwrap();
        assert_eq!(
            (ready.hosted_by(CHANNEL), at),
            (generation, Some(1)),
            "a new source missing on its first open must keep its claim"
        );
        assert_eq!((io.boots.load(Ordering::SeqCst), halts(&h)), (1, 1));
        assert!(host.live() && !ready.accepts(CHANNEL));
        assert!(h.port.posts().is_empty());
    }
    assert!(matches!(
        h.alarms.raised.lock().unwrap().last(),
        Some(WriterAlarm::HaltedMissingSource { source, .. }) if *source == fresh
    ));
    std::fs::rename(&away, &path).unwrap();
    past(&h, 1, || {
        h.port.posts().len() == 1 && ready.accepts(CHANNEL)
    })
    .await;
    for _ in 0..5 {
        polls(2).await;
        assert_eq!(h.port.posts(), [REPLY]);
        assert_eq!(h.channel().binding_checkpoint().unwrap(), Some(2));
        assert_eq!((io.boots.load(Ordering::SeqCst), halts(&h)), (2, 1));
        assert_eq!(ready.hosted_by(CHANNEL), generation);
        assert!(host.live() && ready.accepts(CHANNEL));
    }
    host.stop_and_join().await.unwrap();
}
