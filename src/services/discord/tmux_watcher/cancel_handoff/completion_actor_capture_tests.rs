use super::*;
use crate::services::discord::serenity::{MessageId, UserId};
use crate::services::discord::turn_end_summary::tests::{Isolation, row};

#[tokio::test(flavor = "current_thread")]
async fn split_capture_preserves_cleanup_predicate_and_only_extends_nonbody_summary_witnesses() {
    let _isolation = Isolation::new();
    for (index, case) in [
        "no_original",
        "no_mailbox",
        "idle",
        "user_none",
        "owner_mismatch",
        "nonce_mismatch",
        "valid_managed",
        "id_mismatch",
        "legacy_none",
        "legacy_empty",
    ]
    .into_iter()
    .enumerate()
    {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(586_530 + index as u64);
        let mut original = row(
            channel.get(),
            if case == "user_none" { 0 } else { 11 },
            "original",
        );
        let nonce = match case {
            "nonce_mismatch" => Some("successor".to_string()),
            "legacy_none" | "legacy_empty" => None,
            _ => Some("original".to_string()),
        };
        if case == "legacy_none" {
            original.turn_nonce = None;
        }
        if case == "legacy_empty" {
            original.turn_nonce = Some(String::new());
        }
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(nonce));
        if case != "no_mailbox" {
            let mailbox = shared.mailbox(channel);
            if case != "idle" {
                let owner = if case == "owner_mismatch" { 2 } else { 1 };
                let id = match case {
                    "user_none" => None,
                    "id_mismatch" => Some(MessageId::new(12)),
                    _ => Some(MessageId::new(11)),
                };
                assert!(
                    mailbox
                        .recovery_kickoff(actor.clone(), UserId::new(owner), id)
                        .await
                        .activated_turn()
                );
            }
        }
        let (completion, summary) = capture_actors(
            &shared,
            channel,
            (case != "no_original").then_some(&original),
        )
        .await;
        let expected_completion = case == "valid_managed";
        let expected_summary = matches!(
            case,
            "valid_managed" | "user_none" | "legacy_none" | "legacy_empty"
        );
        assert_eq!(
            completion.is_some(),
            expected_completion,
            "original completion cleanup predicate changed: {case}"
        );
        assert_eq!(
            summary.is_some(),
            expected_summary,
            "summary actor provenance changed: {case}"
        );
        for witness in completion.iter().chain(summary.iter()) {
            assert!(Weak::ptr_eq(witness, &Arc::downgrade(&actor)));
        }
        assert!(!actor.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "capture is read-only and cannot release or admit a turn"
        );
        if let Some(mailbox) = shared.mailbox_peek(channel) {
            let snapshot = mailbox.snapshot().await;
            assert_eq!(snapshot.cancel_token.is_some(), case != "idle");
            if let Some(active) = snapshot.cancel_token {
                assert!(Arc::ptr_eq(&active, &actor));
            }
        }
    }
}

fn arguments_of<'a>(source: &'a str, call: &str) -> &'a str {
    assert_eq!(
        source.matches(call).count(),
        1,
        "one actual production call is required"
    );
    let args = source.split_once(call).unwrap().1;
    let mut depth = 1;
    for (offset, byte) in args.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return &args[..offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated production call");
}

#[test]
fn production_watcher_keeps_cleanup_and_summary_witnesses_on_their_separate_call_sites() {
    let source = include_str!("../../tmux_watcher.rs");
    let producer = arguments_of(
        source,
        "complete_watcher_terminal_footer_or_status_panel_with_sniffer(",
    );
    assert!(
        producer.trim_end().ends_with("summary_actor.as_ref(),"),
        "actual watcher summary must receive the original summary witness"
    );
    let cleanup = arguments_of(source, "cancel_handoff::completion::finish_after_receipt(");
    assert!(cleanup.contains("completion_actor.as_ref(),"));
    assert!(
        !cleanup.contains("summary_actor"),
        "summary witness never authorizes existing cleanup"
    );
}
