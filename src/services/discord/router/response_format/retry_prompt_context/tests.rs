use super::*;

#[test]
fn six_requeues_and_restarts_render_one_wip_with_exact_raw_bytes() {
    let raw = "quoted bytes\r\n[WIP 리마인더] typed by the user\n";
    let warning = "dirty worktree";
    let expected = format!(
        "{}\n\n{raw}",
        crate::services::discord::router::response_format::format_turn_end_wip_warning_injection(
            warning
        )
        .unwrap()
    );
    let mut context = RetryPromptContext::raw(Some(raw.into()));
    let mut reply = None;
    for attempt in 0..7 {
        context.selected.wip = Some(warning.into());
        reply = context.render();
        if attempt < 6 {
            context = RetryPromptContext::restore(context.requeue());
        }
    }
    assert_eq!(
        reply.as_deref(),
        Some(expected.as_str()),
        "WIP_REPEAT_BYTE_CONTRACT"
    );
}

#[test]
fn same_bytes_keep_raw_and_legacy_provenance_across_restart() {
    let bytes = "[WIP 리마인더] user-written text";
    let raw = RetryPromptContext::restore(RetryPromptContext::raw(Some(bytes.into())).encode());
    let legacy = RetryPromptContext::restore(Some(bytes.into()));
    for (mut context, may_select) in [(raw, true), (legacy, false)] {
        assert_eq!(
            context.may_select(),
            may_select,
            "RAW_LEGACY_DURABLE_DISCRIMINANT"
        );
        context = RetryPromptContext::restore(context.requeue());
        assert_eq!(
            context.may_select(),
            may_select,
            "RAW_LEGACY_DURABLE_DISCRIMINANT"
        );
        assert_eq!(context.raw.as_deref(), Some(bytes));
        if context.may_select() {
            context.selected.wip = Some("new warning".into());
            assert_ne!(context.render().as_deref(), Some(bytes));
        } else {
            context.selected.wip = Some("must not decorate".into());
            assert_eq!(context.render().as_deref(), Some(bytes));
        }
    }
}

#[test]
fn empty_raw_wire_keeps_plain_text_eligibility_but_actual_context_does_not() {
    use crate::services::discord::router::response_format::build_race_requeued_intervention;
    use poise::serenity_prelude::{MessageId, UserId};
    let mut item = build_race_requeued_intervention(
        UserId::new(77),
        MessageId::new(731_440),
        "human input",
        true,
        None,
        false,
        false,
        Vec::new(),
    );
    assert!(crate::services::turn_orchestrator::prefix::eligible(&item));
    item.reply_context = RetryPromptContext::raw(None).encode();
    assert!(
        crate::services::turn_orchestrator::prefix::eligible(&item),
        "EMPTY_RAW_MUST_KEEP_BUSY_INJECT_ELIGIBILITY"
    );
    for mut metadata_only in [
        RetryPromptContext::raw(None),
        RetryPromptContext::restore(None),
    ] {
        metadata_only.expected_prompt = Some("receipt snapshot".into());
        item.reply_context = metadata_only.encode();
        assert!(
            crate::services::turn_orchestrator::prefix::eligible(&item),
            "RECEIPT_METADATA_MUST_NOT_CHANGE_PAYLOAD_ELIGIBILITY"
        );
    }
    for context in [
        RetryPromptContext::raw(Some("actual reply".into())),
        RetryPromptContext {
            selected: Preludes {
                wip: Some("selected".into()),
                ..Preludes::default()
            },
            ..RetryPromptContext::raw(None)
        },
        RetryPromptContext::restore(Some("legacy rendered".into())),
    ] {
        item.reply_context = context.encode();
        assert!(
            !crate::services::turn_orchestrator::prefix::eligible(&item),
            "ACTUAL_CONTEXT_MUST_NOT_GAIN_BUSY_INJECT_ELIGIBILITY"
        );
    }
    item.reply_context = RetryPromptContext::raw(None).encode();
    item.has_reply_boundary = true;
    assert!(!crate::services::turn_orchestrator::prefix::eligible(&item));
}

#[test]
fn fresh_user_wire_like_bytes_are_preserved_inside_raw_envelope() {
    let fake = RetryPromptContext::raw(Some("looks internal".into()))
        .encode()
        .unwrap();
    let raw = RetryPromptContext::raw(Some(fake.clone()));
    let restored = RetryPromptContext::restore(raw.encode());
    assert_eq!(restored.raw.as_deref(), Some(fake.as_str()));
    assert_eq!(restored.render().as_deref(), Some(fake.as_str()));
    let unknown = "\u{1e}agentdesk.retry-prompt.v2\n{}";
    assert_eq!(
        RetryPromptContext::restore(Some(unknown.into()))
            .render()
            .as_deref(),
        Some(unknown)
    );
}
