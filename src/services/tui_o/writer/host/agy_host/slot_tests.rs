//! Typed Discord and iMessage turn slots, read beside each other in one destination.

use super::*;
use crate::services::tui_input::input_key::SYNTHETIC_KEY_BASE;
use crate::services::tui_o::store::agy_spool::{
    AgyReservation, AgySpool, CHECKPOINT, ImessageSource, SEGMENT,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const TARGET: AgyTarget = AgyTarget {
    destination: 43,
    logical_channel: 41,
};
const MESSAGE: u64 = 61;
const EXECUTION: u64 = SYNTHETIC_KEY_BASE + 7;

fn reservation(message: u64) -> AgyReservation<'static> {
    AgyReservation {
        destination: TARGET.destination,
        logical_channel: TARGET.logical_channel,
        message,
        nonce: Uuid::new_v4(),
        session_key: "fixture",
        writer_generation: NonZeroU64::MIN,
        resume: None,
    }
}

fn discord(root: &Path, message: u64) -> AgySpool {
    AgySpool::reserve(root, reservation(message)).unwrap()
}

fn imessage(root: &Path, guid: &str, execution: u64) -> (AgySpool, AgySlot) {
    let source = ImessageSource::from_guid(guid).unwrap();
    let slot = AgySlot::Imessage(source.digest().to_owned());
    let spool = AgySpool::reserve_imessage(root, reservation(execution), &source).unwrap();
    (spool, slot)
}

fn owned_gate() -> Arc<OwnershipGate> {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    gate
}

fn start(root: &Path) -> AgyShadowHost {
    AgyShadowHost::start(root.to_owned(), TARGET, owned_gate())
        .unwrap()
        .unwrap()
}

fn version(root: &Path) -> PathBuf {
    version_dir(root, TARGET.destination).unwrap()
}

/// Rewrites the first record's data and republishes a checkpoint that matches the new bytes.
fn rewrite_open(slot_dir: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let segment = slot_dir.join(SEGMENT);
    let text = fs::read_to_string(&segment).unwrap();
    let mut records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    edit(&mut records[0]["data"]);
    let mut bytes = Vec::new();
    for record in &records {
        bytes.extend(serde_json::to_vec(record).unwrap());
        bytes.push(b'\n');
    }
    fs::write(&segment, &bytes).unwrap();
    let checkpoint = serde_json::json!({
        "segment": SEGMENT, "committed_bytes": bytes.len(), "last_seq": records.len() - 1,
        "prefix_hash": format!("{:x}", Sha256::digest(&bytes)),
    });
    fs::write(
        slot_dir.join(CHECKPOINT),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();
}

fn held(custody: &AgyCustody, name: &str) -> Vec<AgyHoldReason> {
    custody
        .held
        .iter()
        .filter(|held| held.name == name)
        .map(|held| held.reason.clone())
        .collect()
}

// T1
#[test]
fn agy_reader_seals_imessage_slot_beside_discord_slot() {
    let root = tempfile::tempdir().unwrap();
    discord(root.path(), MESSAGE)
        .done(Uuid::new_v4(), "discord answer")
        .unwrap();
    let (mut spool, _) = imessage(root.path(), "guid-a", EXECUTION);
    spool.done(Uuid::new_v4(), "imessage answer").unwrap();

    let turns = start(root.path()).restore(&BTreeMap::new()).unwrap();
    assert_eq!(turns.len(), 2);
    let message = turns.iter().find(|turn| turn.key.id == EXECUTION).unwrap();
    let AgyOutcome::Sealed(seal) = &message.outcome else {
        panic!("iMessage slot was not sealed")
    };
    assert_eq!(seal.body, "imessage answer");
    assert!(turns.iter().any(|turn| turn.key.id == MESSAGE));
}

// T2
#[test]
fn agy_reader_holds_imessage_open_digest_differing_from_slot() {
    let root = tempfile::tempdir().unwrap();
    let (mut spool, slot) = imessage(root.path(), "guid-a", EXECUTION);
    spool.done(Uuid::new_v4(), "answer").unwrap();
    let other = format!("imsg-{}", "0".repeat(64));
    fs::rename(
        version(root.path()).join(slot.dir_name()),
        version(root.path()).join(&other),
    )
    .unwrap();

    let host = start(root.path());
    assert!(host.restore(&BTreeMap::new()).is_err());
    let custody = host.custody(&BTreeMap::new()).unwrap();
    assert!(matches!(
        held(&custody, &other).as_slice(),
        [AgyHoldReason::Corrupt(_)]
    ));
}

// T3a
#[test]
fn agy_reader_rejects_discord_open_with_http_ingress() {
    let root = tempfile::tempdir().unwrap();
    discord(root.path(), MESSAGE)
        .done(Uuid::new_v4(), "answer")
        .unwrap();
    rewrite_open(&version(root.path()).join(MESSAGE.to_string()), |open| {
        open["ingress"] = serde_json::json!("imessage_http");
    });
    assert!(start(root.path()).restore(&BTreeMap::new()).is_err());
}

// T3b
#[test]
fn agy_reader_rejects_discord_open_with_only_imessage_sha() {
    let root = tempfile::tempdir().unwrap();
    discord(root.path(), MESSAGE)
        .done(Uuid::new_v4(), "answer")
        .unwrap();
    rewrite_open(&version(root.path()).join(MESSAGE.to_string()), |open| {
        open["original_input_sha256"] = serde_json::json!("0".repeat(64));
    });
    assert!(start(root.path()).restore(&BTreeMap::new()).is_err());
}

// T4
#[test]
fn agy_reader_holds_two_imessage_slots_naming_one_execution() {
    let root = tempfile::tempdir().unwrap();
    let (mut first, first_slot) = imessage(root.path(), "guid-a", EXECUTION);
    first.done(Uuid::new_v4(), "one").unwrap();
    let (mut second, second_slot) = imessage(root.path(), "guid-b", EXECUTION);
    second.done(Uuid::new_v4(), "two").unwrap();

    let host = start(root.path());
    assert!(host.restore(&BTreeMap::new()).is_err());
    let custody = host.custody(&BTreeMap::new()).unwrap();
    for slot in [first_slot, second_slot] {
        assert_eq!(held(&custody, &slot.dir_name()), [AgyHoldReason::Duplicate]);
    }
}

// T5
#[test]
fn agy_reader_holds_malformed_imsg_slot_name() {
    let root = tempfile::tempdir().unwrap();
    discord(root.path(), MESSAGE)
        .done(Uuid::new_v4(), "answer")
        .unwrap();
    fs::create_dir(version(root.path()).join("imsg-XYZ")).unwrap();
    fs::write(version(root.path()).join("retired.json.tmp"), b"41").unwrap();

    let host = start(root.path());
    assert!(host.restore(&BTreeMap::new()).is_err());
    let custody = host.custody(&BTreeMap::new()).unwrap();
    assert_eq!(custody.held.len(), 1);
    assert_eq!(held(&custody, "imsg-XYZ"), [AgyHoldReason::Malformed]);
    assert_eq!(custody.turns.len(), 1);
}

// T6
#[test]
fn agy_reader_frontier_pins_imessage_slot() {
    let root = tempfile::tempdir().unwrap();
    let (mut spool, slot) = imessage(root.path(), "guid-a", EXECUTION);
    spool.done(Uuid::new_v4(), "answer").unwrap();
    let host = start(root.path());
    let first = host.custody(&BTreeMap::new()).unwrap();
    assert!(first.held.is_empty());
    let previous = first.frontiers();
    let frontier = previous[&slot].clone();

    let moved_volume = AgyFrontier {
        dev: frontier.dev ^ 1,
        ..frontier.clone()
    };
    assert!(replay(root.path(), TARGET, &slot, Some(&moved_volume)).is_err());

    let replacement = frontier.path.with_extension("replacement");
    fs::copy(&frontier.path, &replacement).unwrap();
    fs::rename(&replacement, &frontier.path).unwrap();
    let replaced = host.custody(&previous).unwrap();
    assert!(matches!(
        held(&replaced, &slot.dir_name()).as_slice(),
        [AgyHoldReason::Corrupt(_)]
    ));

    fs::remove_dir_all(version(root.path()).join(slot.dir_name())).unwrap();
    let missing = host.custody(&previous).unwrap();
    assert_eq!(held(&missing, &slot.dir_name()), [AgyHoldReason::Frontier]);
}

// T15
#[test]
fn adopt_names_mkdir_only_slot() {
    let root = tempfile::tempdir().unwrap();
    discord(root.path(), MESSAGE)
        .done(Uuid::new_v4(), "answer")
        .unwrap();
    fs::create_dir(version(root.path()).join((MESSAGE + 1).to_string())).unwrap();

    let adoption = start(root.path())
        .adopt(&BTreeMap::new(), true, &BTreeMap::new())
        .unwrap();
    assert_eq!(adoption.permit, None);
    assert_eq!(
        adoption.custody.held,
        vec![AgyHeldSlot {
            name: (MESSAGE + 1).to_string(),
            reason: AgyHoldReason::NoCheckpoint,
        }]
    );
    assert_eq!(adoption.custody.turns.len(), 1);
    assert_eq!(adoption.custody.turns[0].key.id, MESSAGE);
}

// T16
#[test]
fn adopt_names_pending_slot_without_rerun() {
    let root = tempfile::tempdir().unwrap();
    let spool = discord(root.path(), MESSAGE);
    drop(spool);
    let segment = version(root.path()).join(MESSAGE.to_string()).join(SEGMENT);
    let before = fs::read(&segment).unwrap();

    let host = start(root.path());
    let adoption = host
        .adopt(&BTreeMap::new(), true, &BTreeMap::new())
        .unwrap();
    assert_eq!(adoption.permit, None);
    let key = adoption.custody.turns[0].key.clone();
    assert_eq!(
        adoption.custody.held,
        vec![AgyHeldSlot {
            name: MESSAGE.to_string(),
            reason: AgyHoldReason::Pending(key),
        }]
    );
    let again = host
        .adopt(&adoption.custody.frontiers(), true, &BTreeMap::new())
        .unwrap();
    assert_eq!(again.permit, None);
    assert_eq!(fs::read(&segment).unwrap(), before);
    assert_eq!(fs::read_dir(version(root.path())).unwrap().count(), 1);
}
