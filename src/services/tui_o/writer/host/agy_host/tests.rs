use super::*;
use crate::services::tui_o::store::agy_spool::{AgyReservation, AgySpool, Failure};
use std::io::Write;
use uuid::Uuid;

const TARGET: AgyTarget = AgyTarget {
    destination: 43,
    logical_channel: 41,
};
const MESSAGE: u64 = 61;

fn reserve(root: &Path, message: u64) -> AgySpool {
    AgySpool::reserve(
        root,
        AgyReservation {
            destination: TARGET.destination,
            logical_channel: TARGET.logical_channel,
            message,
            nonce: Uuid::new_v4(),
            session_key: "fixture",
            writer_generation: NonZeroU64::MIN,
            resume: None,
        },
    )
    .unwrap()
}

fn owned_gate() -> Arc<OwnershipGate> {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    gate
}

fn start(root: &Path, gate: &Arc<OwnershipGate>) -> AgyShadowHost {
    AgyShadowHost::start(root.to_owned(), TARGET, Arc::clone(gate))
        .unwrap()
        .unwrap()
}

#[test]
fn agy_shadow_replays_only_committed_prefix_and_canonical_units() {
    let root = tempfile::tempdir().unwrap();
    let mut spool = reserve(root.path(), MESSAGE);
    let conversation = Uuid::new_v4();
    spool.init(conversation).unwrap();
    spool.delta("preview").unwrap();
    let gate = owned_gate();
    let host = start(root.path(), &gate);
    let first = host
        .adopt(&BTreeMap::new(), true, &BTreeMap::new())
        .unwrap();
    assert_eq!(first.permit, None);
    let pending = first.custody.turns;
    assert_eq!(pending[0].outcome, AgyOutcome::Pending);
    let previous = BTreeMap::from([(AgySlot::Discord(MESSAGE), pending[0].frontier.clone())]);

    let body = "🧪\n".repeat(1_500);
    spool.done(conversation, &body).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&pending[0].frontier.path)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    let second = host.adopt(&previous, true, &BTreeMap::new()).unwrap();
    assert_eq!(second.permit.unwrap().target, TARGET);
    let adopted = second.custody.turns;
    let AgyOutcome::Sealed(seal) = &adopted[0].outcome else {
        panic!("missing seal")
    };
    assert_eq!(seal.body, body);
    assert_eq!(seal.conversation_id, conversation);
    assert_ne!(seal.reply, seal.footer);
    assert_eq!(seal.reply.channel_id, TARGET.destination);
    assert_eq!(
        seal.reply.provider,
        crate::services::tui_o::shadow::ShadowProvider::Antigravity
    );
    assert!(adopted[0].frontier.last_seq > pending[0].frontier.last_seq);
    assert_eq!(host.body_owner(), Some(AgyBodyOwner::Legacy));
}

#[test]
fn agy_shadow_rejects_parent_conflict_prefix_shrink_and_segment_replacement() {
    let root = tempfile::tempdir().unwrap();
    let mut spool = reserve(root.path(), MESSAGE);
    spool.fail(Failure::Child).unwrap();
    let slot = AgySlot::Discord(MESSAGE);
    let found = replay(root.path(), TARGET, &slot, None).unwrap();
    assert_eq!(found.outcome, AgyOutcome::Failed(Failure::Child));
    let wrong_parent = AgyTarget {
        logical_channel: 55,
        ..TARGET
    };
    assert!(replay(root.path(), wrong_parent, &slot, None).is_err());
    let segment = &found.frontier.path;
    let original = fs::read(segment).unwrap();
    fs::write(segment, &original[..original.len() - 1]).unwrap();
    assert!(replay(root.path(), TARGET, &slot, Some(&found.frontier)).is_err());
    fs::write(segment, &original).unwrap();
    let replacement = segment.with_extension("replacement");
    fs::write(&replacement, &original).unwrap();
    fs::rename(replacement, segment).unwrap();
    assert!(replay(root.path(), TARGET, &slot, Some(&found.frontier)).is_err());
}

#[test]
fn agy_shadow_restart_resolves_one_owner_and_fences_retired_child() {
    let root = tempfile::tempdir().unwrap();
    let gate = owned_gate();
    let first = start(root.path(), &gate);
    let no_evidence = BTreeMap::new();
    assert!(
        first
            .adopt(&BTreeMap::new(), false, &no_evidence)
            .unwrap()
            .permit
            .is_none()
    );
    let first_permit = first
        .adopt(&BTreeMap::new(), true, &no_evidence)
        .unwrap()
        .permit
        .unwrap();
    assert_eq!(first.body_owner(), Some(AgyBodyOwner::Legacy));

    gate.uncertain();
    assert!(first.restore(&BTreeMap::new()).is_err());
    assert_eq!(first.body_owner(), None);
    gate.reacquired().unwrap();
    let next = start(root.path(), &gate);
    let permit = next
        .adopt(&BTreeMap::new(), true, &no_evidence)
        .unwrap()
        .permit
        .unwrap();
    assert_ne!(first_permit, permit);
    assert!(
        !next
            .retire_child(first_permit, &BTreeMap::new(), |_| true)
            .unwrap()
    );
    assert_eq!(next.body_owner(), Some(AgyBodyOwner::Legacy));

    let mut spool = reserve(root.path(), MESSAGE);
    assert!(
        !next
            .retire_child(permit, &BTreeMap::new(), |_| true)
            .unwrap()
    );
    spool.done(Uuid::new_v4(), "settled").unwrap();
    let retired = next
        .retire_child(permit, &BTreeMap::new(), |turn| turn.key.id == MESSAGE)
        .unwrap();
    assert!(retired);
    assert_eq!(next.body_owner(), Some(AgyBodyOwner::Retired));

    reserve(root.path(), MESSAGE + 1)
        .done(Uuid::new_v4(), "late")
        .unwrap();
    assert!(next.restore(&BTreeMap::new()).is_err());
    assert!(replay(root.path(), TARGET, &AgySlot::Discord(MESSAGE + 1), None).is_err());
    let restarted = start(root.path(), &owned_gate());
    assert_eq!(restarted.body_owner(), Some(AgyBodyOwner::Retired));
    assert!(
        restarted
            .adopt(&BTreeMap::new(), true, &no_evidence)
            .is_err()
    );
}

#[test]
fn agy_shadow_missing_known_spool_is_debt_after_restart() {
    let root = tempfile::tempdir().unwrap();
    reserve(root.path(), MESSAGE).fail(Failure::Spawn).unwrap();
    let gate = owned_gate();
    let host = start(root.path(), &gate);
    let turns = host.restore(&BTreeMap::new()).unwrap();
    let prior = BTreeMap::from([(AgySlot::Discord(MESSAGE), turns[0].frontier.clone())]);
    fs::remove_file(&turns[0].frontier.path).unwrap();
    gate.acquired();
    let adoption = start(root.path(), &gate)
        .adopt(&prior, true, &BTreeMap::new())
        .unwrap();
    assert!(adoption.permit.is_none());
    assert_eq!(
        adoption.custody.held,
        vec![AgyHeldSlot {
            name: MESSAGE.to_string(),
            reason: AgyHoldReason::NoCheckpoint,
        }]
    );
}
