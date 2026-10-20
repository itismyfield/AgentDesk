//! Classification 2 for the text-command dispatcher guards: matching is comment-free and
//! as written, scoped to the dispatcher and alias blocks. A lexical tripwire; it does
//! not prove reachability, alias resolution or that a count pairs with an await.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const TEXT_COMMANDS: &str = include_str!("../text_commands.rs");
pub(super) const INTAKE_GATE: &str = include_str!("../../router/intake_gate.rs");

const DISPATCHER: &str =
    "pub(in crate::services::discord) async fn handle_text_command_with_uploads(";
const SELECTION_ORDER: [&str; 10] = [
    "let classified = TextCommandId::from_str(cmd);",
    "if classified.is_some() && risk.is_high_risk()",
    "let is_owner = check_owner(msg.author.id, &data.shared).await;",
    "let high_risk_enabled = super::high_risk_enabled_via_env();",
    "(is_owner, high_risk_enabled)",
    "let selected = match select_text_command(classified, is_owner, high_risk_enabled)",
    "TextCommandSelection::Dispatch(id) => id,",
    "denied.denial_message(cmd)",
    "return Ok(true);",
    "match selected {",
];
const DISPATCH_ARMS: [&str; 2] = [
    "TextCommandId::Vc => {\n            super::handle_vc_text_command",
    "TextCommandId::Cc | TextCommandId::Skill => {",
];
const ALIAS_MATCH: &str = "match skill.as_str() {";
const ALIAS_ACTIONS: [&str; 8] = [
    "\"Use `!clear` instead.\"",
    "cancel_text_stop_token_mailbox(",
    "super::CommandRisk::Mutating,",
    "handle_text_command(ctx, msg, data, channel_id, \"!pwd\")",
    "handle_text_command(ctx, msg, data, channel_id, \"!health\")",
    "handle_text_command(ctx, msg, data, channel_id, \"!status\")",
    "handle_text_command(ctx, msg, data, channel_id, \"!inflight\")",
    "handle_text_command(ctx, msg, data, channel_id, \"!help\")",
];

/// Selection runs classify → permission → dispatch in order and the outer match over
/// the selected id stays exhaustive.
pub(super) fn dispatch_gaps(source: &str) -> Vec<String> {
    let Some(body) = Lexed::new(source).block(DISPATCHER) else {
        return vec![format!("missing dispatcher {DISPATCHER}")];
    };
    let mut gaps = Vec::new();
    let mut from = 0;
    for anchor in SELECTION_ORDER {
        match body.find_after(from, anchor) {
            Some(at) => from = at + anchor.len(),
            None => gaps.push(format!("missing or out of order: {anchor}")),
        }
    }
    let rest = body.slice(from..body.text().len());
    if body.text().contains("Ok(false)") {
        gaps.push("dispatcher returns Ok(false)".to_string());
    }
    // Nested subcommand wildcards are legitimate; only the outer match is forbidden.
    if rest
        .text()
        .lines()
        .any(|line| line.starts_with("        _ =>"))
    {
        gaps.push("outer dispatch match has a wildcard".to_string());
    }
    for arm in DISPATCH_ARMS {
        if !rest.contains(arm) {
            gaps.push(format!("missing dispatch arm: {arm}"));
        }
    }
    gaps
}

/// The dispatcher's seven skill aliases equal the intake gate's alias list, and each
/// alias keeps its canonical action.
pub(super) fn skill_alias_gaps(text_commands: &str, intake_gate: &str) -> Vec<String> {
    let Some(aliases) = Lexed::new(text_commands)
        .block(DISPATCHER)
        .and_then(|body| body.block(ALIAS_MATCH))
    else {
        return vec![format!("missing {ALIAS_MATCH} in the dispatcher")];
    };
    let names: std::collections::BTreeSet<_> = aliases
        .text()
        .lines()
        .filter_map(|line| line.strip_prefix("                \""))
        .filter_map(|line| line.split_once("\" => {").map(|pair| pair.0))
        .collect();
    let intake = Lexed::new(intake_gate);
    let intake_names: std::collections::BTreeSet<_> = intake
        .find("let is_skill_command =")
        .and_then(|start| {
            let end = intake.find_after(start, "if !is_skill_command {")?;
            let list = &intake.text()[start..end];
            Some(list.split_once("skill_name,\n")?.1)
        })
        .into_iter()
        .flat_map(|list| list.split('"').skip(1).step_by(2))
        .collect();
    let mut gaps = Vec::new();
    if names != intake_names || names.len() != 7 {
        gaps.push(format!(
            "aliases {names:?} differ from intake {intake_names:?}"
        ));
    }
    for action in ALIAS_ACTIONS {
        if !aliases.contains(action) {
            gaps.push(format!("alias lost its canonical action: {action}"));
        }
    }
    gaps
}

#[test]
fn dispatch_guards_reject_commented_selection_and_arms() {
    assert_eq!(dispatch_gaps(TEXT_COMMANDS), Vec::<String>::new());
    let needles = [DISPATCHER]
        .into_iter()
        .chain(SELECTION_ORDER)
        .chain(DISPATCH_ARMS);
    for needle in needles {
        for forgery in CommentForgery::ALL {
            let forged = forge(TEXT_COMMANDS, needle, forgery);
            assert!(forged.contains(needle));
            assert!(
                !dispatch_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let late_classify = swap(TEXT_COMMANDS, SELECTION_ORDER[0], SELECTION_ORDER[2]);
    assert!(!dispatch_gaps(&late_classify).is_empty());
}

#[test]
fn dispatch_guard_counts_ok_false_in_code_but_not_in_comments() {
    let open = "match selected {";
    let prose = format!("{open} /* outer /* Ok(false) */ */ // Ok(false)");
    assert_eq!(
        dispatch_gaps(&TEXT_COMMANDS.replacen(open, &prose, 1)),
        Vec::<String>::new()
    );
    let coded = TEXT_COMMANDS.replacen(
        open,
        "if false { return Ok(false); }\n    match selected {",
        1,
    );
    assert_eq!(dispatch_gaps(&coded), ["dispatcher returns Ok(false)"]);
}

#[test]
fn skill_alias_guard_rejects_commented_aliases_and_actions() {
    assert_eq!(
        skill_alias_gaps(TEXT_COMMANDS, INTAKE_GATE),
        Vec::<String>::new()
    );
    for forgery in CommentForgery::ALL {
        let intake = forge(INTAKE_GATE, "\"pwd\" | ", forgery);
        assert!(
            !skill_alias_gaps(TEXT_COMMANDS, &intake).is_empty(),
            "{forgery:?}"
        );
        for needle in [ALIAS_MATCH, DISPATCHER].into_iter().chain(ALIAS_ACTIONS) {
            let forged = forge(TEXT_COMMANDS, needle, forgery);
            assert!(forged.contains(needle));
            assert!(
                !skill_alias_gaps(&forged, INTAKE_GATE).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
}
