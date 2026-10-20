//! Classification 2: bans read comment-free text, counts and order read as-written code.
//! A lexical tripwire per file; no alias or macro resolution, no reachability.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const INTAKE_GATE: &str = include_str!("../../intake_gate.rs");
pub(super) const GATEWAY: &str = include_str!("../../../gateway.rs");
pub(super) const DISCORD_MOD: &str = include_str!("../../../mod.rs");
pub(super) const QUEUE_KICKOFF: &str = include_str!("../../../queue_dispatch/kickoff.rs");
pub(super) const SKILL: &str = include_str!("../../../commands/skill.rs");
pub(super) const TEXT_COMMANDS: &str = include_str!("../../../commands/text_commands.rs");
pub(super) const WORKER_BODY: &str = include_str!("../../message_handler/intake_turn.rs");
pub(super) const WORKER_ENTRY: &str =
    include_str!("../../message_handler/intake_turn/worker_entry.rs");
pub(super) const MESSAGE_HANDLER: &str = include_str!("../../message_handler.rs");

pub(super) type Sources<'a> = [(&'static str, &'a str)];

pub(super) const PRODUCERS: [(&str, &str); 6] = [
    ("intake_gate", INTAKE_GATE),
    ("gateway", GATEWAY),
    ("discord_mod", DISCORD_MOD),
    ("queue_kickoff", QUEUE_KICKOFF),
    ("skill", SKILL),
    ("text_commands", TEXT_COMMANDS),
];
pub(super) const DISPATCHERS: [(&str, &str); 3] = [
    ("intake_gate", INTAKE_GATE),
    ("skill", SKILL),
    ("text_commands", TEXT_COMMANDS),
];
pub(super) const QUEUED: [(&str, &str, &str); 2] = [
    ("gateway", GATEWAY, "drain_dispatched_queue_markers("),
    (
        "queue_kickoff",
        QUEUE_KICKOFF,
        "start_and_drain_kickoff_markers(",
    ),
];
const CENTRAL_DISPATCH: [(&str, &str, usize); 3] = [
    ("intake_gate", "dispatch_text_intake(", 1),
    ("skill", "dispatch_skill_intake(", 2),
    ("text_commands", "dispatch_skill_intake(", 1),
];
const WORKER_BANS: [&str; 4] = [
    "dispatch_text_intake(",
    "admit_text_intake(",
    "try_route_intake(",
    "IntakeSubmission {",
];
const BODY: &str = "handle_text_message(";

/// No producer names the body directly, and each dispatcher keeps its central calls.
pub(super) fn producer_gaps(producers: &Sources, dispatchers: &Sources) -> Vec<String> {
    let mut gaps = Vec::new();
    for (name, source) in producers {
        if Lexed::new(source).text().contains(BODY) {
            gaps.push(format!("{name} bypasses central intake admission"));
        }
    }
    for (name, source) in dispatchers {
        let source = Lexed::new(source);
        if source.text().contains("finish_admitted_local(") {
            gaps.push(format!("{name} bypasses dispatch convenience path"));
        }
        for (owner, call, expected) in CENTRAL_DISPATCH {
            if owner == *name && source.count(call) != expected {
                gaps.push(format!("{name} must call {call} {expected} time(s)"));
            }
        }
    }
    gaps
}

/// Only the worker's post-claim seam calls the body, once per file.
pub(super) fn worker_gaps(body: &str, entry: &str, message_handler: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    for (name, source) in [("intake_turn", body), ("worker_entry", entry)] {
        let source = Lexed::new(source);
        for banned in WORKER_BANS {
            if source.text().contains(banned) {
                gaps.push(format!("{name} re-enters admission via {banned}"));
            }
        }
    }
    for (name, source) in [
        ("intake_turn", body),
        ("worker_entry", entry),
        ("message_handler", message_handler),
    ] {
        if Lexed::new(source).count(BODY) != 1 {
            gaps.push(format!("{name} must name {BODY} exactly once"));
        }
    }
    gaps
}

/// Queued entrypoints admit, then promote their markers once, then finish once.
pub(super) fn queued_promotion_gaps(queued: &[(&str, &str, &str)]) -> Vec<String> {
    let mut gaps = Vec::new();
    for (name, source, promotion) in queued {
        let source = Lexed::new(source);
        let finish = "finish_admitted_queued_intake(";
        let order = [
            source.find("admit_queued_intake("),
            source.find(promotion),
            source.find(finish),
        ];
        match order {
            [Some(admit), Some(promote), Some(done)] if admit < promote && promote < done => {}
            _ => gaps.push(format!("{name} must admit, promote markers, then finish")),
        }
        if source.count(promotion) != 1 || source.count(finish) != 1 {
            gaps.push(format!("{name} must promote and finish exactly once"));
        }
    }
    gaps
}

#[test]
fn central_admission_guards_reject_commented_calls() {
    assert_eq!(
        producer_gaps(&PRODUCERS, &DISPATCHERS),
        Vec::<String>::new()
    );
    for (index, (name, call, _)) in CENTRAL_DISPATCH.into_iter().enumerate() {
        for forgery in CommentForgery::ALL {
            let mut dispatchers = DISPATCHERS;
            let forged = forge(dispatchers[index].1, call, forgery);
            dispatchers[index].1 = &forged;
            assert!(
                !producer_gaps(&PRODUCERS, &dispatchers).is_empty(),
                "{forgery:?} commented {call} in {name} still passes"
            );
        }
    }
    let body = BODY;
    for (index, (name, source)) in PRODUCERS.into_iter().enumerate() {
        let mut producers = PRODUCERS;
        let prose = format!("// {body}\n/* outer /* {body} */ */\n{source}");
        producers[index].1 = &prose;
        assert_eq!(
            producer_gaps(&producers, &DISPATCHERS),
            Vec::<String>::new()
        );
        let coded = format!("{source}\nfn forged() {{ {body}); }}\n");
        producers[index].1 = &coded;
        let gaps = producer_gaps(&producers, &DISPATCHERS);
        assert_eq!(gaps, [format!("{name} bypasses central intake admission")]);
    }
}

#[test]
fn worker_bypass_guard_rejects_commented_bodies_and_counts_coded_reentry() {
    let actual = [WORKER_BODY, WORKER_ENTRY, MESSAGE_HANDLER];
    assert_eq!(
        worker_gaps(actual[0], actual[1], actual[2]),
        Vec::<String>::new()
    );
    for index in 0..actual.len() {
        for forgery in CommentForgery::ALL {
            let mut sources = actual;
            let forged = forge(sources[index], BODY, forgery);
            sources[index] = &forged;
            assert!(forged.contains(BODY));
            assert_eq!(worker_gaps(sources[0], sources[1], sources[2]).len(), 1);
        }
    }
    for banned in WORKER_BANS {
        let prose = format!("// {banned}\n/* outer /* {banned} */ */\n{WORKER_ENTRY}");
        assert_eq!(
            worker_gaps(WORKER_BODY, &prose, MESSAGE_HANDLER),
            Vec::<String>::new()
        );
        let coded = format!("{WORKER_ENTRY}\nfn forged() {{ {banned} }}\n");
        assert_eq!(worker_gaps(WORKER_BODY, &coded, MESSAGE_HANDLER).len(), 1);
    }
}

#[test]
fn queued_promotion_guard_rejects_commented_and_reordered_steps() {
    assert_eq!(queued_promotion_gaps(&QUEUED), Vec::<String>::new());
    for (index, (name, source, promotion)) in QUEUED.into_iter().enumerate() {
        let steps = [
            "admit_queued_intake(",
            promotion,
            "finish_admitted_queued_intake(",
        ];
        for step in steps {
            for forgery in CommentForgery::ALL {
                let mut queued = QUEUED;
                let forged = forge(source, step, forgery);
                queued[index].1 = &forged;
                assert!(
                    !queued_promotion_gaps(&queued).is_empty(),
                    "{forgery:?} commented {step} in {name} still passes"
                );
            }
        }
        let mut queued = QUEUED;
        let reordered = swap(source, promotion, "finish_admitted_queued_intake(");
        queued[index].1 = &reordered;
        assert!(
            !queued_promotion_gaps(&queued).is_empty(),
            "{name} reorder passes"
        );
    }
}
