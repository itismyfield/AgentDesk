use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom, SourceLink};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_o::writer::suspension::Suspension;
use sha2::Digest;

struct Bindings {
    event: BindingEvent,
    failed: AtomicBool,
    notice: tokio::sync::watch::Sender<u64>,
}

impl BindingEvents for Bindings {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        assert_eq!(channel, CHANNEL);
        if self.failed.load(Ordering::SeqCst) {
            return Err("fixture binding log unavailable".into());
        }
        Ok((self.event.seq > after)
            .then(|| self.event.clone())
            .into_iter()
            .collect())
    }

    fn subscribe(&self, channel: u64) -> tokio::sync::watch::Receiver<u64> {
        assert_eq!(channel, CHANNEL);
        self.notice.subscribe()
    }
}

type HealthWriter = ChannelWriter<FakePort, Arc<FakeLease>, Arc<AlarmRouter>>;

fn fixture() -> (Harness, [SourceId; 2], Arc<Bindings>) {
    let mut ids = Vec::new();
    let harness = Harness::build(|root| {
        (0..2)
            .map(|index| {
                let path = root.join(format!("pending-{index}.jsonl"));
                std::fs::write(&path, b"").unwrap();
                let source_id = source_id_for(&format!("pending-{index}"), &path).unwrap();
                ids.push(source_id.clone());
                InitSource {
                    source_id,
                    delivery_start: 0,
                    prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
                }
            })
            .collect()
    });
    harness.gate.acquired();
    let ids: [SourceId; 2] = ids.try_into().unwrap();
    let mut store = harness.channel();
    let mut rotation = store.rotation().unwrap();
    for source in &ids {
        rotation.links.insert(
            source_key(source),
            SourceLink {
                source: source.clone(),
                seq: 1,
                parent: None,
                committed_at: Utc::now(),
                boundary: Boundary::Pending {
                    candidates: vec![0],
                },
                hot_candidate: None,
            },
        );
    }
    store.write_rotation(&rotation).unwrap();
    store.set_binding_checkpoint(1).unwrap();
    let event = BindingEvent {
        seq: 1,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: "boundary-health".into(),
        execution_nonce: "boundary-health".into(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(ids[0].clone()),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "session_start".into(),
                received_at: Utc::now(),
                reclaims: false,
            },
        },
        committed_at: Utc::now(),
    };
    let bindings = Arc::new(Bindings {
        event,
        failed: AtomicBool::new(false),
        notice: tokio::sync::watch::channel(1).0,
    });
    (harness, ids, bindings)
}

fn writer(harness: &Harness, router: &Arc<AlarmRouter>) -> HealthWriter {
    ChannelWriter::new(
        harness.channel(),
        Arc::clone(&harness.gate),
        Arc::clone(&harness.port),
        Arc::clone(&harness.lease),
        Arc::clone(router),
    )
}

fn resume(
    writer: &mut HealthWriter,
    bindings: &Arc<Bindings>,
    suspension: Suspension,
) -> Sources<Bindings> {
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, Arc::clone(bindings))
        .with_suspension(suspension);
    sources
        .resume(
            writer,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        )
        .unwrap();
    sources
}

fn active(health: &AlarmHealth, kind: &str, channel: u64) -> bool {
    health
        .current_at(std::time::Instant::now())
        .contains(&format!("tui_o:{kind}:{channel}"))
}

fn resolve(harness: &Harness, source: &SourceId) {
    harness
        .store
        .record_boundary_resolved(
            CHANNEL,
            &source_key(source),
            &ResolveFrom::Offset(0),
            "boundary-health-test",
        )
        .unwrap();
}

#[test]
fn c07_reopen_clears_only_the_last_pending_and_preserves_binding_log_health() {
    let (harness, ids, bindings) = fixture();
    let health = Arc::new(AlarmHealth::default());
    let router = Arc::new(AlarmRouter::new(None, None, Arc::clone(&health)));
    router.raise(
        CHANNEL + 1,
        WriterAlarm::BoundaryPending {
            source: ids[0].clone(),
        },
    );
    let mut current = writer(&harness, &router);
    resume(&mut current, &bindings, Suspension::default());
    assert!(active(&health, "boundary_pending", CHANNEL));
    drop(current);

    resolve(&harness, &ids[0]);
    let mut current = writer(&harness, &router);
    resume(&mut current, &bindings, Suspension::default());
    let rotation = current.store().rotation().unwrap();
    assert_eq!(
        rotation.link(&ids[0]).unwrap().boundary,
        Boundary::Owed { from: 0 }
    );
    assert!(matches!(
        rotation.link(&ids[1]).unwrap().boundary,
        Boundary::Pending { .. }
    ));
    assert!(active(&health, "boundary_pending", CHANNEL));
    drop(current);

    resolve(&harness, &ids[1]);
    bindings.failed.store(true, Ordering::SeqCst);
    let mut current = writer(&harness, &router);
    resume(&mut current, &bindings, Suspension::default());
    assert!(!active(&health, "boundary_pending", CHANNEL));
    assert!(active(&health, "binding_log_unavailable", CHANNEL));
    assert!(active(&health, "boundary_pending", CHANNEL + 1));
    let pending = current
        .store()
        .rotation()
        .unwrap()
        .links
        .values()
        .filter(|link| matches!(link.boundary, Boundary::Pending { .. }))
        .count();
    assert_eq!(pending, 0);
}

#[test]
fn c07_capture_excludes_suspended_pending_but_counts_retired_pending_across_resume() {
    let (harness, ids, bindings) = fixture();
    harness.channel().set_retired(&ids[1], true).unwrap();
    let health = Arc::new(AlarmHealth::default());
    let router = Arc::new(AlarmRouter::new(None, None, Arc::clone(&health)));
    let suspension = Suspension::default();
    suspension.extend("boundary-health".into(), [ids[0].clone()]);
    let mut current = writer(&harness, &router);
    let mut sources = resume(&mut current, &bindings, suspension.clone());
    assert!(current.store().cursor(&ids[1]).unwrap().retired);
    assert!(active(&health, "boundary_pending", CHANNEL));

    suspension.extend("boundary-health".into(), [ids[1].clone()]);
    sources
        .capture(
            &mut current,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        )
        .unwrap();
    assert!(!active(&health, "boundary_pending", CHANNEL));
    drop((sources, current));

    router.raise(
        CHANNEL,
        WriterAlarm::BoundaryPending {
            source: ids[0].clone(),
        },
    );
    let mut current = writer(&harness, &router);
    resume(&mut current, &bindings, suspension);
    assert!(!active(&health, "boundary_pending", CHANNEL));
    assert!(
        current
            .store()
            .rotation()
            .unwrap()
            .links
            .values()
            .all(|link| matches!(link.boundary, Boundary::Pending { .. }))
    );
}
