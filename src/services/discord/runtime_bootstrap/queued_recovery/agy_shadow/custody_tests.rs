//! Boot custody: selection before recovery, two reads of one prefix, evidence for iMessage slots.

use std::fs;
use std::num::NonZeroU64;
use std::path::Path;

use uuid::Uuid;

use super::evidence;
use super::*;
use crate::db::imsg_reply::Obligation;
use crate::services::discord::InflightTurnState;
use crate::services::tui_input::input_key::SYNTHETIC_KEY_BASE;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::agy_spool::replay::AgyOutcome;
use crate::services::tui_o::store::agy_spool::{AgyReservation, AgySpool, ImessageSource};
use crate::services::tui_o::writer::host::agy_host::{AgyEvidenceHold, AgyHoldReason};

const DIRECT: AgyTarget = AgyTarget {
    destination: 41,
    logical_channel: 41,
};
const THREAD: AgyTarget = AgyTarget {
    destination: 43,
    logical_channel: 41,
};
const MESSAGE: u64 = 61;
const EXECUTION: u64 = SYNTHETIC_KEY_BASE + 7;
const GUID: &str = "imessage-guid-fixture";

type Rows = BTreeMap<AgySlot, Result<Option<Obligation>, ()>>;

fn config(value: serde_json::Value) -> crate::config::Config {
    serde_json::from_value(value).unwrap()
}

/// AGY parent 41 selected by the default writer selection.
fn selected() -> BootChannels {
    BootChannels::validate(&config(serde_json::json!({
        "server": {}, "agents": [{"id": "agy", "name": "AGY", "channels": {"agy": {"id": "41"}}}]
    })))
    .unwrap()
}

/// AGY 41 stays registered; the writer list now names only the Claude pane 42.
fn deselected() -> BootChannels {
    BootChannels::validate(&config(serde_json::json!({
        "server": {}, "tui_o": {"writer": {"channels": [42]}},
        "agents": [
            {"id": "agy", "name": "AGY", "channels": {"agy": {"id": "41"}}},
            {"id": "pane", "name": "Pane", "channels": {"claude": {"id": "42", "runtime": "tui"}}}
        ]
    })))
    .unwrap()
}

fn text(id: u64) -> GuildChannel {
    serde_json::from_value(serde_json::json!({"id": id.to_string(), "type": 0, "name": "fixture"}))
        .unwrap()
}

fn thread(id: u64, parent: u64) -> GuildChannel {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "fixture"
    }))
    .unwrap()
}

fn reservation(target: AgyTarget, message: u64, nonce: Uuid) -> AgyReservation<'static> {
    AgyReservation {
        destination: target.destination,
        logical_channel: target.logical_channel,
        message,
        nonce,
        session_key: "fixture",
        writer_generation: NonZeroU64::MIN,
        resume: None,
    }
}

fn source() -> ImessageSource {
    ImessageSource::from_guid(GUID).unwrap()
}

fn imessage_slot() -> AgySlot {
    AgySlot::Imessage(source().digest().to_owned())
}

fn sealed_imessage(root: &Path, nonce: Uuid) {
    AgySpool::reserve_imessage(root, reservation(DIRECT, EXECUTION, nonce), &source())
        .unwrap()
        .done(Uuid::new_v4(), "imessage answer")
        .unwrap();
}

fn owned_gate() -> Arc<OwnershipGate> {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    gate
}

fn inflight(nonce: Uuid, digest: &str) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Antigravity,
        DIRECT.destination,
        None,
        7,
        EXECUTION,
        0,
        "fixture".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.logical_channel_id = Some(DIRECT.logical_channel);
    row.turn_nonce = Some(nonce.to_string());
    row.agy_imessage_source_sha256 = Some(digest.to_owned());
    row
}

fn obligation(parent: u64, queued: Option<u64>, episode: Option<String>) -> Obligation {
    let now = chrono::Utc::now();
    Obligation {
        origin_id: GUID.into(),
        source: "imessage".into(),
        agent_id: "agy".into(),
        channel_id: DIRECT.destination.to_string(),
        person_id: 7,
        request_hash: format!("v3:agy:{parent}:fixture"),
        opened_at: now,
        expires_at: now,
        admission: "admitted".into(),
        admission_kind: None,
        admission_result: None,
        delivery_confirmed: None,
        resolution: "open".into(),
        admitted_turn_id: None,
        queued_message_id: queued.map(|id| id.to_string()),
        episode_key: episode,
        inject_nonce: None,
        last_reply_episode: None,
        last_reply_at: None,
        terminal_seen_at: None,
        terminal_first_line: None,
        active_send_action: None,
        discord_mirror_id: None,
        closed_at: None,
        closed_reason: None,
    }
}

fn queue_row() -> Rows {
    BTreeMap::from([(
        imessage_slot(),
        Ok(Some(obligation(
            DIRECT.logical_channel,
            Some(EXECUTION),
            None,
        ))),
    )])
}

/// One boot pass: first read, evidence, then the second read over the first read's frontiers.
fn observe_once(
    root: &Path,
    gate: &Arc<OwnershipGate>,
    custody: Custody,
    inflight: &Result<Option<InflightTurnState>, String>,
    rows: &Rows,
) -> AgyAdoption {
    let (host, first) = first_read(root.to_owned(), custody, Arc::clone(gate))
        .unwrap()
        .unwrap();
    let target = custody.target();
    let bindings = evidence::journal_bindings(root, target.destination, &first.turns);
    let found = evidence::assemble(target, &first.turns, inflight, rows, &bindings);
    let settled = evidence::prior_legacy_settled(inflight, &first.turns, &found);
    second_read(&host, custody, &first, settled, &found).unwrap()
}

fn evidence_held(adoption: &AgyAdoption) -> Vec<AgyHoldReason> {
    adoption
        .custody
        .held
        .iter()
        .filter(|held| held.name == imessage_slot().dir_name())
        .map(|held| held.reason.clone())
        .collect()
}

// T7
#[test]
fn observe_holds_queue_only_pg_row() {
    let root = tempfile::tempdir().unwrap();
    sealed_imessage(root.path(), Uuid::new_v4());
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &Ok(None),
        &queue_row(),
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::PgOnly)]
    );
}

// T8
#[test]
fn observe_holds_direct_episode_pg_row() {
    let root = tempfile::tempdir().unwrap();
    sealed_imessage(root.path(), Uuid::new_v4());
    let episode = crate::services::imsg_reply::episode::turn(
        &ProviderKind::Antigravity,
        DIRECT.destination,
        EXECUTION,
    );
    let rows = BTreeMap::from([(
        imessage_slot(),
        Ok(Some(obligation(
            DIRECT.logical_channel,
            None,
            Some(episode),
        ))),
    )]);
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &Ok(None),
        &rows,
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::PgOnly)]
    );
}

// T9
#[test]
fn adopt_holds_same_id_with_other_nonce() {
    let root = tempfile::tempdir().unwrap();
    sealed_imessage(root.path(), Uuid::new_v4());
    let other = Ok(Some(inflight(Uuid::new_v4(), source().digest())));
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &other,
        &queue_row(),
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::Conflict)]
    );
}

// T10
#[test]
fn adopt_holds_prefix_replaced_between_reads() {
    let root = tempfile::tempdir().unwrap();
    AgySpool::reserve(root.path(), reservation(DIRECT, MESSAGE, Uuid::new_v4()))
        .unwrap()
        .done(Uuid::new_v4(), "discord answer")
        .unwrap();
    let custody = Custody::Selected(DIRECT);
    let (host, first) = first_read(root.path().to_owned(), custody, owned_gate())
        .unwrap()
        .unwrap();
    let segment = first.turns[0].frontier.path.clone();
    let replacement = segment.with_extension("replacement");
    fs::copy(&segment, &replacement).unwrap();
    fs::rename(&replacement, &segment).unwrap();

    let adoption = second_read(&host, custody, &first, true, &BTreeMap::new()).unwrap();
    assert_eq!(adoption.permit, None);
    assert!(matches!(
        adoption.custody.held.as_slice(),
        [held] if held.name == MESSAGE.to_string()
            && matches!(held.reason, AgyHoldReason::Corrupt(_))
    ));
}

// T11
#[test]
fn observe_admits_bound_imessage_slot() {
    let root = tempfile::tempdir().unwrap();
    let nonce = Uuid::new_v4();
    sealed_imessage(root.path(), nonce);
    let bound = Ok(Some(inflight(nonce, source().digest())));
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &bound,
        &queue_row(),
    );
    assert!(adoption.custody.held.is_empty());
    assert_eq!(adoption.permit.unwrap().target, DIRECT);
}

// T12
#[test]
fn observe_holds_pg_and_inflight_conflict() {
    let root = tempfile::tempdir().unwrap();
    let nonce = Uuid::new_v4();
    sealed_imessage(root.path(), nonce);
    let bound = Ok(Some(inflight(nonce, source().digest())));
    let other_parent = BTreeMap::from([(
        imessage_slot(),
        Ok(Some(obligation(99, Some(EXECUTION), None))),
    )]);
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &bound,
        &other_parent,
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::Conflict)]
    );
}

// T13
#[test]
fn recovery_only_restores_deselected_parent_once() {
    let snapshot = deselected();
    assert!(!snapshot.agy_selected(41));
    assert!(snapshot.configured_binding(41));
    let custody = custody(&snapshot, 41, true, Some(&text(41))).unwrap();
    assert_eq!(custody, Custody::Recovery(DIRECT));

    let root = tempfile::tempdir().unwrap();
    AgySpool::reserve(root.path(), reservation(DIRECT, MESSAGE, Uuid::new_v4()))
        .unwrap()
        .done(Uuid::new_v4(), "existing answer")
        .unwrap();
    let gate = owned_gate();
    for _ in 0..2 {
        let adoption = observe_once(root.path(), &gate, custody, &Ok(None), &Rows::new());
        assert_eq!(adoption.permit, None);
        assert!(adoption.custody.held.is_empty());
        assert_eq!(adoption.custody.turns.len(), 1);
        assert!(matches!(
            adoption.custody.turns[0].outcome,
            AgyOutcome::Sealed(_)
        ));
    }
    assert_eq!(snapshot.agy_target(41), None);
    assert!(!snapshot.agy_selected(41));
}

// T13c
#[test]
fn recovery_refuses_a_selected_pane_destination() {
    let snapshot = deselected();
    assert!(snapshot.hosts_output(42));
    assert!(custody(&snapshot, 42, true, Some(&text(42))).is_err());
}

// T13c (held pane child)
#[test]
fn recovery_refuses_a_held_pane_child_even_when_nothing_hosts_it() {
    let snapshot = deselected();
    let child = serde_json::json!({"channel": 43, "policy_parent": 41, "sources": [],
        "initial_anchor": 0, "build_digest": "fixture", "at": "2026-11-25T00:00:00Z"});
    snapshot.record_child_fixture(&serde_json::from_value(child).unwrap());
    assert!(matches!(snapshot.child(43), Some(Err(_))));
    assert!(!snapshot.hosts_output(43));
    assert!(!snapshot.hosts_output(41));

    assert!(custody(&snapshot, 43, true, Some(&thread(43, 41))).is_err());
    assert!(snapshot.agy_recovery_target(&thread(43, 41)).is_err());
    assert_eq!(snapshot.agy_target(43), None);
}

// T13s
#[test]
fn selected_parent_thread_keeps_selection_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut pending =
        AgySpool::reserve(root.path(), reservation(THREAD, MESSAGE, Uuid::new_v4())).unwrap();
    let gate = owned_gate();

    let snapshot = selected();
    assert_eq!(snapshot.agy_target(43), None);
    let custody = custody(&snapshot, 43, true, Some(&thread(43, 41))).unwrap();
    assert_eq!(custody, Custody::Selected(THREAD));
    assert_eq!(snapshot.agy_target(43), Some(THREAD));
    let held = observe_once(root.path(), &gate, custody, &Ok(None), &Rows::new());
    assert_eq!(held.permit, None);
    assert!(matches!(
        held.custody.held.as_slice(),
        [slot] if matches!(slot.reason, AgyHoldReason::Pending(_))
    ));

    pending.done(Uuid::new_v4(), "settled").unwrap();
    let restarted = selected();
    let custody = super::custody(&restarted, 43, true, Some(&thread(43, 41))).unwrap();
    assert_eq!(custody, Custody::Selected(THREAD));
    let adoption = observe_once(root.path(), &gate, custody, &Ok(None), &Rows::new());
    assert_eq!(adoption.custody.turns.len(), 1);
    assert_eq!(adoption.custody.turns[0].key.id, MESSAGE);
    assert_eq!(adoption.permit.unwrap().target, THREAD);
}

// T13x (cache conflict)
#[test]
fn cached_child_contradicted_by_fresh_read_is_held_not_recovered() {
    let snapshot = selected();
    assert_eq!(
        custody(&snapshot, 43, true, Some(&thread(43, 41))),
        Ok(Custody::Selected(THREAD))
    );
    assert_eq!(snapshot.agy_target(43), Some(THREAD));
    let moved = thread(43, 50);
    assert!(snapshot.agy_recovery_target(&moved).is_ok());

    assert_eq!(
        custody(&snapshot, 43, true, Some(&moved)),
        Err("agy thread relationship changed")
    );
    assert_eq!(snapshot.agy_target(43), None);
}

// T14
#[test]
fn recovery_only_restores_deselected_child_via_checked_parent() {
    let snapshot = deselected();
    let custody = custody(&snapshot, 43, true, Some(&thread(43, 41))).unwrap();
    assert_eq!(custody, Custody::Recovery(THREAD));
    assert_eq!(snapshot.agy_target(43), None);
    assert!(custody_rejects_unchecked_thread(&snapshot));

    let root = tempfile::tempdir().unwrap();
    AgySpool::reserve(root.path(), reservation(THREAD, MESSAGE, Uuid::new_v4()))
        .unwrap()
        .done(Uuid::new_v4(), "existing answer")
        .unwrap();
    let gate = owned_gate();
    for _ in 0..2 {
        let adoption = observe_once(root.path(), &gate, custody, &Ok(None), &Rows::new());
        assert_eq!(adoption.permit, None);
        assert_eq!(adoption.custody.turns.len(), 1);
    }
    let wrong_parent = Custody::Recovery(AgyTarget {
        logical_channel: 99,
        ..THREAD
    });
    let adoption = observe_once(root.path(), &gate, wrong_parent, &Ok(None), &Rows::new());
    assert!(matches!(
        adoption.custody.held.as_slice(),
        [held] if matches!(held.reason, AgyHoldReason::Corrupt(_))
    ));
}

/// A thread is never recovered without a checked parent.
fn custody_rejects_unchecked_thread(snapshot: &BootChannels) -> bool {
    let orphan: GuildChannel =
        serde_json::from_value(serde_json::json!({"id": "44", "type": 11, "name": "fixture"}))
            .unwrap();
    custody(snapshot, 44, true, Some(&orphan)).is_err()
}

// T17
#[test]
fn observe_mixed_slots_hold_then_admit_after_evidence() {
    let root = tempfile::tempdir().unwrap();
    AgySpool::reserve(root.path(), reservation(DIRECT, MESSAGE, Uuid::new_v4()))
        .unwrap()
        .done(Uuid::new_v4(), "discord answer")
        .unwrap();
    let nonce = Uuid::new_v4();
    sealed_imessage(root.path(), nonce);
    let gate = owned_gate();

    let held = observe_once(
        root.path(),
        &gate,
        Custody::Selected(DIRECT),
        &Ok(None),
        &queue_row(),
    );
    assert_eq!(held.permit, None);
    assert_eq!(held.custody.held.len(), 1);
    assert_eq!(
        evidence_held(&held),
        [AgyHoldReason::Evidence(AgyEvidenceHold::PgOnly)]
    );
    assert!(held.custody.turns.iter().any(|turn| turn.key.id == MESSAGE));

    let bound = Ok(Some(inflight(nonce, source().digest())));
    let adoption = observe_once(
        root.path(),
        &gate,
        Custody::Selected(DIRECT),
        &bound,
        &queue_row(),
    );
    assert!(adoption.custody.held.is_empty());
    assert_eq!(adoption.custody.turns.len(), 2);
    assert_eq!(adoption.permit.unwrap().target, DIRECT);
}

/// A delivery journal written by slice 4 for the iMessage execution, with its binding.
fn journal(root: &Path, nonce: Uuid, version: u32) {
    let dir = root.join(format!("o_store/{}/agy-delivery", DIRECT.destination));
    fs::create_dir_all(&dir).unwrap();
    let fingerprint = [0_u8; 32];
    let state = serde_json::json!({
        "version": version,
        "key": {"destination": DIRECT.destination, "id": EXECUTION, "nonce": nonce},
        "revision": 1,
        "fingerprint": fingerprint,
        "last_seq": 3,
        "receipts": [],
        "footer": null,
        "attempt": null,
        "ended": null,
        "binding": {"logical_channel": DIRECT.logical_channel,
            "original": {"imessage": {"digest": source().digest()}}},
    });
    fs::write(
        dir.join(format!("{EXECUTION}.json")),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
}

// D5
#[test]
fn observe_accepts_journal_binding_after_inflight_clear() {
    let root = tempfile::tempdir().unwrap();
    let nonce = Uuid::new_v4();
    sealed_imessage(root.path(), nonce);
    journal(root.path(), nonce, 1);
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &Ok(None),
        &queue_row(),
    );
    assert!(adoption.custody.held.is_empty());
    assert_eq!(adoption.permit.unwrap().target, DIRECT);
}

// D6
#[test]
fn observe_holds_journal_binding_with_other_nonce() {
    let root = tempfile::tempdir().unwrap();
    sealed_imessage(root.path(), Uuid::new_v4());
    journal(root.path(), Uuid::new_v4(), 1);
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &Ok(None),
        &queue_row(),
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::Conflict)]
    );
}

// A journal of a version this binary does not know is unreadable, never binding evidence.
#[test]
fn observe_holds_journal_of_unknown_version() {
    let root = tempfile::tempdir().unwrap();
    let nonce = Uuid::new_v4();
    sealed_imessage(root.path(), nonce);
    journal(root.path(), nonce, 2);
    let read = crate::services::tui_o::store::agy_delivery::read_binding;
    assert!(read(root.path(), DIRECT.destination, EXECUTION).is_err());
    let adoption = observe_once(
        root.path(),
        &owned_gate(),
        Custody::Selected(DIRECT),
        &Ok(None),
        &queue_row(),
    );
    assert_eq!(adoption.permit, None);
    assert_eq!(
        evidence_held(&adoption),
        [AgyHoldReason::Evidence(AgyEvidenceHold::Unverifiable)]
    );
}
