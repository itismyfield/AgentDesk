//! Classification 2: each gated region is a real item or a statement run inside one, with
//! comments excluded. A lexical tripwire over a fixed list: no alias, macro or reachability.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const MOD_RS: &str = include_str!("../../../mod.rs");
pub(super) const KICKOFF_RS: &str = include_str!("../../../queue_dispatch/kickoff.rs");
pub(super) const GATEWAY_RS: &str = include_str!("../../../gateway.rs");
pub(super) const INTAKE_RS: &str = include_str!("../../../router/message_handler/intake_turn.rs");
pub(super) const HANDOFF_RS: &str =
    include_str!("../../../router/message_handler/intake_turn/placeholder_handoff.rs");
pub(super) const BOOTSTRAP_RS: &str =
    include_str!("../../../runtime_bootstrap/queued_placeholders.rs");
pub(super) const FLUSH_RS: &str = include_str!("../../../runtime_bootstrap/recovery_flush.rs");
pub(super) const CONTROLLER_RS: &str = include_str!("../../../placeholder_controller.rs");
pub(super) const GATE_RS: &str = include_str!("../../queued_card_gate.rs");

/// Serenity and in-tree helper spellings that DELETE or PATCH a message without a teardown
/// token; `queued_card_gate.rs` is the token boundary and is not checked against them.
const RAW_DISCORD_MESSAGE_OPS: &[&str] = &[
    "delete_message(",
    "delete_channel_message(",
    "delete_rollback_channel_message(",
    "edit_message(",
    "edit_channel_message(",
    "edit_channel_message_with_components(",
    "edit_outbound_message(",
    "edit_outbound_message_classified(",
];
const GATE: &str = "queued_card_gate::";

/// Where a gated region sits: whole items, one block reached head by head, or the
/// statements from `start` up to `end` inside the block reached through `scope`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Span {
    Items(&'static [&'static str]),
    Nested(&'static [&'static str]),
    Statements {
        scope: &'static [&'static str],
        start: &'static str,
        end: &'static str,
    },
}

/// The A1–A8 regions; A6/A7 used to end at a comment and a log literal and now end
/// with the arm and `if` block that hold the gate call.
pub(super) const SITES: [(&str, &str, Span); 8] = [
    (
        "A1/A2 queue-exit drain",
        MOD_RS,
        Span::Items(&[
            "async fn queue_exit_drain_queued_placeholders(",
            "async fn apply_queue_exit_feedback(",
        ]),
    ),
    (
        "A3 deferred drain",
        MOD_RS,
        Span::Items(&["async fn drain_pending_queue_exit_placeholder_clears_with("]),
    ),
    (
        "A5 kickoff drain",
        KICKOFF_RS,
        Span::Statements {
            scope: &["async fn kickoff_queue_channel_inner("],
            start: "let drained_cards = gateway::drain_merged_queued_placeholders(",
            end: "let dispatch_result =",
        },
    ),
    (
        "A4 merged drain helper",
        GATEWAY_RS,
        Span::Items(&["pub(super) async fn drain_merged_queued_placeholders("]),
    ),
    (
        "A4 dispatch call site",
        GATEWAY_RS,
        Span::Statements {
            scope: &[
                "impl TurnGateway for DiscordGateway",
                "fn dispatch_queued_turn<'a>(",
            ],
            start: "let drained = drain_merged_queued_placeholders(",
            end: "router::finish_admitted_queued_intake",
        },
    ),
    (
        "A6 dequeue hand-off",
        INTAKE_RS,
        Span::Nested(&[
            "pub(super) async fn handle_text_message(",
            "if let Some(existing) = queued_placeholder_handoff",
            "Ok(fresh_msg_id) =>",
        ]),
    ),
    (
        "A7 busy-notice reuse",
        HANDOFF_RS,
        Span::Nested(&[
            "pub(super) async fn reuse_bound_busy_notice(",
            "if let Some(stale_queued) = queued_placeholder_handoff",
        ]),
    ),
    (
        "A8 stale-card delete",
        BOOTSTRAP_RS,
        Span::Items(&["async fn delete_stale_queued_placeholder_cards_with("]),
    ),
];

const FLUSH_SCOPE: &str = "pub(super) fn run_bot_spawn_recovery_and_flush_restart_reports(";
const FLUSH_CALL: &str = "delete_stale_queued_placeholder_cards(";
const FLUSH_END: &str = "mark_reconcile_complete(";
const FLUSH_SHARED: &str = "&shared_for_tmux2";
const DETACH_CALL: &str = ".detach_by_message(";
const EXIT_BODY: &str = "async fn teardown_exit_body(";
const WARN: &str = "tracing::warn!";
const DISCARD: &str = "let _ =";

fn nested(lexed: &Lexed, heads: &[&str]) -> Option<Lexed> {
    heads
        .iter()
        .try_fold(lexed.clone(), |scope, head| scope.block(head))
}

fn statements(scope: &Lexed, start: &str, end: &str) -> Option<Lexed> {
    let from = scope.find(start)?;
    let to = scope.find_after(from + start.len(), end)?;
    Some(scope.slice(from..to))
}

fn resolve(source: &str, span: Span) -> Option<Vec<Lexed>> {
    let lexed = Lexed::new(source);
    match span {
        Span::Items(heads) => heads.iter().map(|head| lexed.block(head)).collect(),
        Span::Nested(heads) => nested(&lexed, heads).map(|block| vec![block]),
        Span::Statements { scope, start, end } => {
            statements(&nested(&lexed, scope)?, start, end).map(|run| vec![run])
        }
    }
}

/// The region routes through the gate AND holds no raw destructive call; a region with
/// both a gate call and a raw call fails. A missing region is a gap, never a pass.
pub(super) fn gated_region_gaps(label: &str, source: &str, span: Span) -> Vec<String> {
    let Some(parts) = resolve(source, span) else {
        return vec![format!("{label}: region missing")];
    };
    let mut gaps = Vec::new();
    if !parts.iter().any(|part| part.contains(GATE)) {
        gaps.push(format!("{label}: not gated"));
    }
    for op in RAW_DISCORD_MESSAGE_OPS {
        if parts.iter().any(|part| part.text().contains(op)) {
            gaps.push(format!("{label}: raw `{op}` inside a gated region"));
        }
    }
    gaps
}

/// The A8 bootstrap caller does not name the gate itself: it must hand `shared` to the
/// helper that re-gates each card, and make no raw call there.
pub(super) fn bootstrap_caller_gaps(flush: &str) -> Vec<String> {
    let lexed = Lexed::new(flush);
    let Some(call) = lexed
        .block(FLUSH_SCOPE)
        .and_then(|scope| statements(&scope, FLUSH_CALL, FLUSH_END))
    else {
        return vec!["A8 caller: region missing".to_string()];
    };
    let mut gaps = Vec::new();
    if !call.contains(FLUSH_SHARED) {
        gaps.push("A8 caller: no `shared`".to_string());
    }
    for op in RAW_DISCORD_MESSAGE_OPS {
        if call.text().contains(op) {
            gaps.push(format!("A8 caller: raw `{op}`"));
        }
    }
    gaps
}

/// `placeholder_controller.rs` itself holds no `detach_by_message` call site.
pub(super) fn controller_gaps(controller: &str) -> Vec<String> {
    if Lexed::new(controller).text().contains(DETACH_CALL) {
        vec!["placeholder_controller.rs calls detach_by_message".to_string()]
    } else {
        Vec::new()
    }
}

pub(super) fn gate_wiring_gaps() -> Vec<String> {
    let mut gaps: Vec<String> = SITES
        .into_iter()
        .flat_map(|(label, source, span)| gated_region_gaps(label, source, span))
        .collect();
    gaps.extend(bootstrap_caller_gaps(FLUSH_RS));
    gaps.extend(controller_gaps(CONTROLLER_RS));
    gaps
}

/// Inside the real teardown item: one warning that carries both errors, and no error
/// discarded with `let _ =`. The warning runs from the macro to the next code `;`.
pub(super) fn exit_body_warning_gaps(gate: &str) -> Vec<String> {
    let Some(body) = Lexed::new(gate).block(EXIT_BODY) else {
        return vec![format!("missing {EXIT_BODY}")];
    };
    let mut gaps = Vec::new();
    let warns = body.find_all(WARN);
    match warns[..] {
        [at] => match body.find_after(at, ";") {
            Some(end) => {
                let warning = body.slice(at..end);
                for field in ["edit_error", "delete_error"] {
                    if !warning.contains(field) {
                        gaps.push(format!("the warning omits {field}"));
                    }
                }
            }
            None => gaps.push("the warning never ends".to_string()),
        },
        _ => gaps.push(format!("{} warnings; want 1", warns.len())),
    }
    if body.text().contains(DISCARD) {
        gaps.push("an error is discarded with `let _ =`".to_string());
    }
    gaps
}

fn span_needles(span: Span) -> Vec<&'static str> {
    match span {
        Span::Items(heads) | Span::Nested(heads) => heads.to_vec(),
        Span::Statements { scope, start, end } => {
            scope.iter().copied().chain([start, end]).collect()
        }
    }
}

#[test]
fn gated_region_guards_reject_commented_heads_and_gate_calls() {
    for (label, source, span) in SITES {
        assert_eq!(gated_region_gaps(label, source, span), Vec::<String>::new());
        for needle in span_needles(span).into_iter().chain([GATE]) {
            for forgery in CommentForgery::ALL {
                let forged = forge(source, needle, forgery);
                assert!(
                    !gated_region_gaps(label, &forged, span).is_empty(),
                    "{label}: {forgery:?} commented `{needle}` still passes"
                );
            }
        }
        if let Span::Statements { start, end, .. } = span {
            let reversed = swap(source, start, end);
            assert!(!gated_region_gaps(label, &reversed, span).is_empty());
        }
    }
    for needle in [FLUSH_SCOPE, FLUSH_CALL, FLUSH_END, FLUSH_SHARED] {
        for forgery in CommentForgery::ALL {
            let forged = forge(FLUSH_RS, needle, forgery);
            assert!(
                !bootstrap_caller_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let reversed = swap(FLUSH_RS, FLUSH_CALL, FLUSH_END);
    assert!(!bootstrap_caller_gaps(&reversed).is_empty());
}

#[test]
fn gated_region_guards_count_coded_raw_calls_but_not_comments() {
    let (label, source, span) = SITES[5];
    let anchor = "let stale_disposition = match queued_card_gate::release_or_rekey(";
    let with = |line: &str| source.replacen(anchor, &format!("{line}\n{anchor}"), 1);
    assert_eq!(
        gated_region_gaps(label, &with("// delete_channel_message("), span),
        Vec::<String>::new()
    );
    let coded = with("let _ = delete_channel_message(http, channel_id, existing);");
    assert_eq!(gated_region_gaps(label, &coded, span).len(), 1);

    let call = "delete_stale_queued_placeholder_cards(\n";
    let flush = |line: &str| FLUSH_RS.replacen(call, &format!("{call}{line}\n"), 1);
    assert_eq!(
        bootstrap_caller_gaps(&flush("// http.delete_message(")),
        Vec::<String>::new()
    );
    assert_eq!(
        bootstrap_caller_gaps(&flush("http.delete_message(1);")).len(),
        1
    );

    let prose = format!("{CONTROLLER_RS}// slot.detach_by_message(\n");
    assert_eq!(controller_gaps(&prose), Vec::<String>::new());
    let coded = format!("{CONTROLLER_RS}fn _probe(c: C) {{ c.detach_by_message(1); }}\n");
    assert_eq!(controller_gaps(&coded).len(), 1);
}

#[test]
fn gated_region_bounds_follow_code_not_comments_or_log_text() {
    let a6 = "// Fresh anchor is live; tear down the buried queued card.";
    let a7 = "routed the queued card through the #5035 gate";
    let reworded = [
        (5, INTAKE_RS.replacen(a6, "// The fresh anchor is up.", 1)),
        (6, HANDOFF_RS.replacen(a7, "routed the queued card", 1)),
    ];
    for (index, source) in reworded {
        let (label, original, span) = SITES[index];
        assert_ne!(source, original, "{label}: the old boundary text moved");
        assert_eq!(
            gated_region_gaps(label, &source, span),
            Vec::<String>::new()
        );
    }
    let (label, source, span) = SITES[1];
    let definition = "pub(in crate::services::discord) async fn drain_pending_queue_exit_placeholder_clears_with(";
    let decoy = "const _DECOY: &str = r#\"async fn drain_pending_queue_exit_placeholder_clears_with() { queued_card_gate::x() }\"#;\n";
    let gate_hidden = forge(source, GATE, CommentForgery::Line);
    let decoyed = gate_hidden.replacen(definition, &format!("{decoy}{definition}"), 1);
    assert_ne!(decoyed, gate_hidden);
    assert!(!gated_region_gaps(label, &decoyed, span).is_empty());
}

#[test]
fn exit_body_warning_guard_rejects_commented_fields_and_counts_coded_warnings() {
    assert_eq!(exit_body_warning_gaps(GATE_RS), Vec::<String>::new());
    for needle in [EXIT_BODY, WARN, "edit_error", "delete_error"] {
        for forgery in CommentForgery::ALL {
            let forged = forge(GATE_RS, needle, forgery);
            assert!(
                !exit_body_warning_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let anchor = "    let outcome = match edit_channel_message(";
    let with = |line: &str| GATE_RS.replacen(anchor, &format!("    {line}\n{anchor}"), 1);
    assert_eq!(
        exit_body_warning_gaps(&with("// tracing::warn!(\"x\"); let _ = 1;")),
        Vec::<String>::new()
    );
    assert_eq!(
        exit_body_warning_gaps(&with("tracing::warn!(\"x\");")).len(),
        1
    );
    assert_eq!(exit_body_warning_gaps(&with("let _ = 1;")).len(), 1);
}
