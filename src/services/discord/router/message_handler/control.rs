use super::*;

/// Compatibility entry point for a legacy command boundary. Denial only.
pub(in crate::services::discord::router) async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    _text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    rate_limit_wait(shared, channel_id).await;
    let _ = channel_id
        .say(
            &ctx.http,
            "Unknown or unavailable text command. Use `!help` for supported commands.",
        )
        .await;
    Ok(())
}

/// Handle text-based commands (!start, !meeting, !stop, !clear, etc.).
/// Consumes every command, including an unexpected unhandled dispatcher result.
pub(in crate::services::discord::router) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
    preloaded_uploads: &[crate::services::cluster::attachment_transfer::uploads::Upload],
    admitted_attachment_permit: &mut Option<super::super::LocalAdmissionPermit>,
) -> Result<bool, Error> {
    let handled = super::super::super::commands::handle_text_command_with_uploads(
        ctx,
        msg,
        data,
        channel_id,
        text,
        preloaded_uploads,
        admitted_attachment_permit,
    )
    .await?;
    if !handled {
        handle_shell_command_raw(ctx, channel_id, text, &data.shared).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod command_boundary_tests {
    use crate::services::discord::router::source_guard_support::{Lexed, forge};
    use crate::test_support::CommentForgery;

    const CONTROL: &str = include_str!("control.rs");
    const WRAPPER: &str = "pub(in crate::services::discord::router) async fn handle_text_command(";
    const LEGACY: &str =
        "pub(in crate::services::discord::router) async fn handle_shell_command_raw(";
    const CONSUME: [&str; 2] = [
        "let handled = super::super::super::commands::handle_text_command_with_uploads(",
        "if !handled {\n        handle_shell_command_raw(ctx, channel_id, text, &data.shared).await?;\n    }\n    Ok(true)",
    ];
    const DENIAL: [&str; 4] = [
        "_text: &str",
        "rate_limit_wait(shared, channel_id).await;",
        "&ctx.http,\n            \"Unknown or unavailable text command. Use `!help` for supported commands.\",",
        ".say(",
    ];
    const EXECUTION: [&str; 5] = [
        "shell_command_builder",
        "spawn_blocking",
        "Command::",
        ".spawn()",
        ".output()",
    ];

    /// Classification 2: both checks read the named function only, comments excluded,
    /// so this file's own test literals and prose never satisfy them. A lexical tripwire.
    fn wrapper_gaps(source: &str) -> Vec<String> {
        let Some(wrapper) = Lexed::new(source).block(WRAPPER) else {
            return vec![format!("missing {WRAPPER}")];
        };
        CONSUME
            .into_iter()
            .filter(|step| !wrapper.contains(step))
            .map(|step| format!("wrapper lost: {step}"))
            .collect()
    }

    fn legacy_gaps(source: &str) -> Vec<String> {
        let Some(legacy) = Lexed::new(source).block(LEGACY) else {
            return vec![format!("missing {LEGACY}")];
        };
        let mut gaps: Vec<String> = EXECUTION
            .into_iter()
            .filter(|banned| legacy.text().contains(banned))
            .map(|banned| format!("legacy boundary constructs execution: {banned}"))
            .collect();
        gaps.extend(
            DENIAL
                .into_iter()
                .filter(|kept| !legacy.contains(kept))
                .map(|kept| format!("legacy denial lost: {kept}")),
        );
        if legacy.count(".say(") != 1 || !legacy.text().trim_end().ends_with("Ok(())\n}") {
            gaps.push("legacy boundary must say once and return Ok(())".to_string());
        }
        gaps
    }

    #[test]
    fn wrapper_consumes_unhandled_dispatch_results_with_denial() {
        assert_eq!(wrapper_gaps(CONTROL), Vec::<String>::new());
    }

    #[test]
    fn legacy_boundary_is_only_a_fixed_denial() {
        assert_eq!(legacy_gaps(CONTROL), Vec::<String>::new());
    }

    #[test]
    fn command_boundary_guards_reject_commented_code_and_count_coded_execution() {
        type Judge = fn(&str) -> Vec<String>;
        let cases: [(Judge, &[&str]); 2] = [
            (wrapper_gaps, &[WRAPPER, CONSUME[0], CONSUME[1]]),
            (
                legacy_gaps,
                &[LEGACY, DENIAL[0], DENIAL[1], DENIAL[2], DENIAL[3]],
            ),
        ];
        for (judge, needles) in cases {
            for needle in needles {
                for forgery in CommentForgery::ALL {
                    let forged = forge(CONTROL, needle, forgery);
                    assert!(forged.contains(needle));
                    assert!(!judge(&forged).is_empty(), "{forgery:?} `{needle}` passes");
                }
            }
        }
        let rate = DENIAL[1];
        for banned in EXECUTION {
            let prose = CONTROL.replacen(rate, &format!("{rate} // {banned}\n/* {banned} */"), 1);
            assert_eq!(legacy_gaps(&prose), Vec::<String>::new());
            let coded = CONTROL.replacen(rate, &format!("{rate} let _ = \"{banned}\";"), 1);
            assert_eq!(legacy_gaps(&coded).len(), 1, "{banned}");
        }
        let text = "\"Unknown or unavailable text command. Use `!help` for supported commands.\"";
        let quoted = CONTROL.replacen(text, &format!("r#\"{text}\"#"), 1);
        assert_ne!(quoted, CONTROL);
        assert!(
            !legacy_gaps(&quoted).is_empty(),
            "a raw-string copy of the denial passes"
        );
    }
}
