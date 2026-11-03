//! Classification 2: each gated region is a real item or a statement run inside one, with
//! comments excluded. A lexical tripwire over a fixed list: no alias, macro or reachability.

use crate::services::discord::router::source_guard_support::Lexed;

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

/// Serenity and in-tree helper spellings that DELETE or PATCH a message without a teardown
/// token; `queued_card_gate.rs` is the token boundary and is not checked against them.
const RAW_DISCORD_MESSAGE_OPS: &[&str] = &[
    "delete_message(",
    "delete_channel_message(",
    "delete_rollback_channel_message(",
    "edit_message(",
    "edit_channel_message(",
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

/// The A1–A9 regions. A6 is the success arm's own block and A7 the stale-card `if` block,
/// so prose or log text near them never bounds the region.
pub(super) const SITES: [(&str, &str, Span); 9] = [
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
            "} else if let Some(existing) = queued_placeholder_handoff",
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
    (
        "A9 turn-mode dequeue release",
        INTAKE_RS,
        Span::Nested(&[
            "pub(super) async fn handle_text_message(",
            "} else if placeholderless {",
        ]),
    ),
];

const FLUSH_SCOPE: &str = "pub(super) fn run_bot_spawn_recovery_and_flush_restart_reports(";
const FLUSH_CALL: &str = "delete_stale_queued_placeholder_cards(";
const FLUSH_END: &str = "mark_reconcile_complete(";
const FLUSH_SHARED: &str = "&shared_for_tmux2";
const DETACH_CALL: &str = ".detach_by_message(";

/// A `=>` head must open a braced arm; an expression arm would let `block` run on into the
/// next arm's block, so it resolves to no region.
fn nested(lexed: &Lexed, heads: &[&str]) -> Option<Lexed> {
    heads.iter().try_fold(lexed.clone(), |scope, head| {
        if head.ends_with("=>") {
            let at = scope.find(head)? + head.len();
            if !scope.text()[at..].trim_start().starts_with('{') {
                return None;
            }
        }
        scope.block(head)
    })
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
