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

    fn producer_absent(&self, _: u64, _: ShadowProvider) -> impl Future<Output = bool> + Send {
        self.probes.fetch_add(1, Ordering::SeqCst);
        std::future::ready(self.absent.load(Ordering::SeqCst))
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
    for _ in 0..120 {
        if predicate() || halts(h) != 0 {
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
        Err(WriterAlarm::Halted { .. })
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
            matches!(sources.follow(&mut writer), Err(WriterAlarm::Halted { ref detail }) if detail.contains("source reopen")),
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
