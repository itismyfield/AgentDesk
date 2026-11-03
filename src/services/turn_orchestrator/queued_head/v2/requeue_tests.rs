use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7676,
        "source_message_ids":[id],"queued_generation":5,"text":format!("original {id}")}))
    .unwrap()
}

fn attempt(nonce: &str) -> AttemptIdentity {
    AttemptIdentity {
        nonce: nonce.into(),
        provider: "claude".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: "requeue-pane".into(),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::PreSource {
            expected_session_id: None,
            expected_transcript: None,
            binding_watermark: None,
        },
    }
}

fn input(id: u64, followers: &[u64], nonce: &str, phase: HeadPhase) -> InputAttempt {
    let payload = PreparedPayload {
        frame: "frame".into(),
        digest: "digest".into(),
        attribution: json!(null),
        followers: followers.iter().map(|id| item(*id)).collect(),
    };
    InputAttempt {
        identity: sources::identity(&item(id)),
        original: item(id),
        phase,
        attempt: Some(attempt(nonce)),
        payload: Some(payload),
        confirmation_deadline_ms: None,
        resolved_source: None,
        payload_window: None,
        resolved_target: None,
        preparation_hold: None,
        legacy_attempt: None,
    }
}

fn retained(
    input: InputAttempt,
    reason: &str,
    disposition: Option<RetainedDisposition>,
) -> RetainedInput {
    RetainedInput {
        input,
        reason: match reason {
            "" => RetainedReason::Unconfirmed,
            reason => RetainedReason::Rejected {
                reason: reason.into(),
            },
        },
        disposition,
        notice: NoticeIntent {
            kind: "retained_retry_required".into(),
            enqueued: true,
        },
    }
}

/// A receipt bound to `id` at `at`, carrying the caller context `id` names.
fn bind(id: u64, at: BindAt) -> AdmissionBind {
    AdmissionBind {
        receipt: format!("tk:{id}:receipt"),
        at,
        context: Some(AdmissionContext {
            original: Some(json!(format!("caller {id}"))),
            ..Default::default()
        }),
    }
}

fn attempt_bind(id: u64, nonce: &str) -> AdmissionBind {
    let identity = sources::identity(&item(id));
    bind(
        id,
        BindAt::Attempt {
            identity,
            attempt: attempt(nonce),
        },
    )
}

fn owner(channel: u64, snapshot: SnapshotV2) -> Owner {
    let file = crate::services::turn_orchestrator::queued_head::pending_queue_file_path(
        &ProviderKind::Claude,
        "v2-requeue",
        ChannelId::new(channel),
    )
    .unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();
    Owner::adopt(file, format!("requeue-{channel}")).unwrap()
}

fn snapshot(channel: u64, items: Vec<u64>, retained: Vec<RetainedInput>) -> SnapshotV2 {
    let mut binds: Vec<AdmissionBind> = Vec::new();
    let mut snapshot = SnapshotV2 {
        version: 2,
        items: items.iter().map(|id| item(*id)).collect(),
        head_injection: None,
        retained_inputs: retained,
        terminal_identities: vec![],
        namespace: Some(format!("requeue-{channel}")),
        admission_receipts: vec![],
        admission_sequence: 0,
        admission_binds: Vec::new(),
    };
    for (index, id) in items.iter().enumerate() {
        let item = snapshot.queued_digest(index).unwrap();
        binds.push(bind(*id, BindAt::Queued { index, item }));
    }
    for entry in &snapshot.retained_inputs {
        let nonce = &entry.input.attempt.as_ref().unwrap().nonce;
        for member in entry.input.members() {
            binds.push(attempt_bind(member.message_id, nonce));
        }
    }
    snapshot.admission_binds = binds;
    snapshot
}

/// Queued ids in order, and each queued receipt with the caller context it still carries.
fn queue(owner: &Owner) -> (Vec<u64>, Vec<(usize, String, Option<serde_json::Value>)>) {
    let ids = owner.snapshot.items.iter().map(|i| i.message_id).collect();
    let mut binds: Vec<_> = (owner.snapshot.admission_binds.iter())
        .filter_map(|b| match &b.at {
            BindAt::Queued { index, item } => {
                assert_eq!(owner.snapshot.queued_digest(*index).as_ref(), Some(item));
                let context = b.context.as_ref().and_then(|c| c.original.clone());
                Some((*index, b.receipt.clone(), context))
            }
            _ => None,
        })
        .collect();
    binds.sort_by_key(|(index, ..)| *index);
    (ids, binds)
}

/// Only a clearable proven-unsent input requeues, once, behind any head with each ticket and caller
/// context; unconfirmed, opaque, legacy, permanent and settled inputs stay retained.
#[test]
fn only_a_proven_unsent_retained_input_is_requeued_once_with_its_tickets() {
    let _root = TestRuntimeRootGuard::new();
    let known = Some(RetainedDisposition::KnownNotSent);
    let ask = "known_not_sent: vetoed: NotBusy";
    let reserved = HeadPhase::Reserved;
    let stays: [(&str, RetainedInput); 6] = [
        (
            "unconfirmed",
            retained(input(10, &[], "u", reserved), "", None),
        ),
        (
            "opaque",
            retained(
                input(10, &[], "o", reserved),
                ask,
                Some(RetainedDisposition::OpaqueUnsent),
            ),
        ),
        (
            "no disposition",
            retained(input(10, &[], "n", reserved), ask, None),
        ),
        (
            "unknown phase",
            retained(input(10, &[], "p", HeadPhase::Unknown), ask, known.clone()),
        ),
        (
            "permanent",
            retained(
                input(10, &[], "f", reserved),
                "known_not_sent: vetoed: frame_mismatch",
                known.clone(),
            ),
        ),
        (
            "settled",
            retained(input(10, &[], "s", reserved), ask, known.clone()),
        ),
    ];
    for (n, (case, entry)) in stays.into_iter().enumerate() {
        let channel = 767_601 + n as u64;
        let mut seeded = snapshot(channel, vec![20], vec![entry]);
        if case == "settled" {
            seeded.terminal_identities.push(TerminalIdentity {
                identity: sources::identity(&item(10)),
                settled_at_ms: 1,
            });
        }
        let mut owner = owner(channel, seeded);
        let before = queue(&owner);
        assert!(owner.requeue_known_not_sent().unwrap().is_empty(), "{case}");
        assert_eq!(owner.snapshot.retained_inputs.len(), 1, "{case}");
        assert_eq!(queue(&owner), before, "{case}");
    }
    // A proven-unsent input with a follower, behind an attempted head with its own binds.
    let channel = 767_610;
    let seeded = snapshot(
        channel,
        vec![1, 20],
        vec![retained(input(10, &[11], "k", reserved), ask, known)],
    );
    let mut owner = owner(channel, seeded);
    // The head unit is the first original alone; the tail stays queued behind it.
    let unit = |item: &PendingQueueItem| {
        (item.message_id == 1).then(|| (String::new(), item.text.clone()))
    };
    owner.reserve_unit(attempt("head"), unit).unwrap();
    let moved = owner.requeue_known_not_sent().unwrap();
    assert_eq!(moved.len(), 1);
    assert!(owner.snapshot.retained_inputs.is_empty());
    let (ids, binds) = queue(&owner);
    assert_eq!(ids, [1, 10, 11, 20]);
    let caller = |id: u64| {
        (
            format!("tk:{id}:receipt"),
            Some(json!(format!("caller {id}"))),
        )
    };
    let expected: Vec<_> = [(0, 1), (1, 10), (2, 11), (3, 20)]
        .into_iter()
        .map(|(index, id)| {
            let (receipt, context) = caller(id);
            (index, receipt, context)
        })
        .collect();
    assert_eq!(
        binds, expected,
        "every member keeps its ticket and caller context"
    );
    let head = |b: &AdmissionBind| matches!(&b.at, BindAt::Attempt { attempt: a, .. } if a.nonce == "head");
    assert!(
        owner.snapshot.admission_binds.iter().any(head),
        "the head keeps its attempt bind"
    );
    // A second boot finds nothing more to requeue.
    assert!(owner.requeue_known_not_sent().unwrap().is_empty());
    assert_eq!(queue(&owner).0, [1, 10, 11, 20]);
}

/// A clear or a cancel that meets a proven-unsent retained input takes it as the untried input it
/// is, in the same commit, so no later requeue brings it back.
#[test]
fn a_clear_or_cancel_takes_a_proven_unsent_input_before_it_could_return() {
    let _root = TestRuntimeRootGuard::new();
    let known = Some(RetainedDisposition::KnownNotSent);
    let ask = "known_not_sent: vetoed: NotBusy";
    let entry = || {
        retained(
            input(10, &[11], "k", HeadPhase::Reserved),
            ask,
            known.clone(),
        )
    };
    let mut cleared = owner(767_620, snapshot(767_620, vec![20], vec![entry()]));
    let result = clear(&mut cleared);
    let mut discarded: Vec<u64> = result
        .discarded_message_ids
        .iter()
        .map(|m| m.get())
        .collect();
    discarded.sort();
    assert_eq!(discarded, [10, 11, 20]);
    assert!(cleared.snapshot.retained_inputs.is_empty());
    assert!(cleared.requeue_known_not_sent().unwrap().is_empty());
    assert!(cleared.snapshot.items.is_empty());
    let mut cancelled = owner(767_621, snapshot(767_621, vec![20], vec![entry()]));
    let result = cancel(&mut cancelled, MessageId::new(10));
    assert_eq!(result.removed.map(|r| r.message_id.get()), Some(10));
    assert!(cancelled.snapshot.retained_inputs.is_empty());
    assert!(cancelled.requeue_known_not_sent().unwrap().is_empty());
    assert_eq!(queue(&cancelled).0, [11, 20]);
}
