//! Classification 2: admission order inside the real entry items, comments excluded. A
//! lexical tripwire; it proves no reachability or await pairing.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const INTAKE_TURN: &str = include_str!("../../router/message_handler/intake_turn.rs");
pub(super) const HEADLESS_TURN: &str =
    include_str!("../../router/message_handler/headless_turn.rs");
pub(super) const RESTORE: &str = include_str!("../../recovery_engine/restore_inflight.rs");

/// Entry item, registration call and durable create call of each original turn entry.
pub(super) const ENTRIES: [(&str, &str, &str, &str); 2] = [
    (
        INTAKE_TURN,
        "pub(super) async fn handle_text_message(",
        "register_or_requeue(",
        "save_intake_inflight(",
    ),
    (
        HEADLESS_TURN,
        "async fn start_reserved_headless_turn_admitted(",
        "register_headless_original(",
        "save_inflight_state_create_new(",
    ),
];
const PRODUCER: &str = "spawn_blocking(move ||";
const HELD: &str = "let _original_registration = producer_registration;";
const CLONED: &str = "producer_registration = original_registration.clone()";
const RESTORE_ENTRY: &str = "pub(in crate::services::discord) async fn restore_inflight_states(";
const READERS: [(&str, &str); 2] = [
    (
        "spawn_observed_tmux_watcher(",
        "reregister_active_turn_from_inflight(",
    ),
    ("read_restored_output(", "mailbox_recovery_kickoff("),
];

/// Registration → durable create → producer inside the entry, no transcript work between
/// registration and create, and the producer holds the cloned registration.
pub(super) fn admission_order_gaps(
    source: &str,
    entry: &str,
    call: &str,
    create_call: &str,
) -> Vec<String> {
    let Some(body) = Lexed::new(source).block(entry) else {
        return vec![format!("missing {entry}")];
    };
    let register = body.find(call);
    let create = register.and_then(|at| body.find_after(at, create_call));
    let producer = create.and_then(|at| body.find_after(at, PRODUCER));
    let (Some(register), Some(create), Some(producer)) = (register, create, producer) else {
        return vec![format!(
            "{entry}: {call} → {create_call} → {PRODUCER} order broken"
        )];
    };
    let mut gaps = Vec::new();
    if body
        .slice(register..create)
        .text()
        .contains("transcript_turns")
    {
        gaps.push(format!(
            "{entry}: transcript work before the durable create"
        ));
    }
    if !body.slice(producer..body.text().len()).contains(HELD) {
        gaps.push(format!("{entry}: producer does not hold the registration"));
    }
    if !body.contains(CLONED) {
        gaps.push(format!("{entry}: producer registration is not cloned"));
    }
    gaps
}

/// Inside restore, each reader starts only after its registration step.
pub(super) fn restore_reader_gaps(restore: &str) -> Vec<String> {
    let Some(body) = Lexed::new(restore).block(RESTORE_ENTRY) else {
        return vec![format!("missing {RESTORE_ENTRY}")];
    };
    READERS
        .into_iter()
        .filter(|(reader, registration)| match body.find(reader) {
            Some(at) => !body.slice(0..at).contains(registration),
            None => true,
        })
        .map(|(reader, registration)| format!("{reader} must follow {registration}"))
        .collect()
}

#[test]
fn admission_order_guards_reject_commented_and_reordered_steps() {
    for (source, entry, call, create) in ENTRIES {
        for needle in [entry, call, create, PRODUCER, HELD, CLONED] {
            for forgery in CommentForgery::ALL {
                let forged = forge(source, needle, forgery);
                assert!(
                    !admission_order_gaps(&forged, entry, call, create).is_empty(),
                    "{forgery:?} commented `{needle}` still passes"
                );
            }
        }
        let late_register = swap(source, call, create);
        assert!(!admission_order_gaps(&late_register, entry, call, create).is_empty());
    }
    for needle in [RESTORE_ENTRY]
        .into_iter()
        .chain(READERS.map(|pair| pair.0))
        .chain(READERS.map(|pair| pair.1))
    {
        for forgery in CommentForgery::ALL {
            let forged = forge(RESTORE, needle, forgery);
            assert!(
                !restore_reader_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let late_kickoff = swap(
        RESTORE,
        "mailbox_recovery_kickoff(",
        "read_restored_output(",
    );
    assert!(!restore_reader_gaps(&late_kickoff).is_empty());
}

#[test]
fn admission_order_guard_counts_coded_transcript_work_but_not_comments() {
    let (source, entry, call, create_call) = ENTRIES[0];
    let create = "let row = save_intake_inflight(";
    let prose = source.replacen(create, &format!("// transcript_turns\n        {create}"), 1);
    assert_ne!(prose, source);
    assert_eq!(
        admission_order_gaps(&prose, entry, call, create_call),
        Vec::<String>::new()
    );
    let coded = source.replacen(
        create,
        &format!("let _ = transcript_turns;\n        {create}"),
        1,
    );
    assert_eq!(
        admission_order_gaps(&coded, entry, call, create_call).len(),
        1
    );
}
