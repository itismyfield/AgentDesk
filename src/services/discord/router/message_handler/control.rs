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
    preloaded_uploads: &[String],
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
    #[test]
    fn wrapper_consumes_unhandled_dispatch_results_with_denial() {
        let body = include_str!("control.rs")
            .split_once("pub(in crate::services::discord::router) async fn handle_text_command(")
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        assert!(body.contains(
            "let handled = super::super::super::commands::handle_text_command_with_uploads("
        ));
        assert!(body.contains("if !handled {\n        handle_shell_command_raw(ctx, channel_id, text, &data.shared).await?;\n    }\n    Ok(true)"));
    }

    #[test]
    fn legacy_boundary_is_only_a_fixed_denial() {
        let body = include_str!("control.rs")
            .split_once(
                "pub(in crate::services::discord::router) async fn handle_shell_command_raw(",
            )
            .expect("legacy entry exists")
            .1
            .split_once("\n}")
            .expect("legacy entry ends before wrapper and tests")
            .0;
        for forbidden in [
            "shell_command_builder",
            "spawn_blocking",
            "Command::",
            ".spawn()",
            ".output()",
        ] {
            assert!(
                !body.contains(forbidden),
                "legacy boundary must not construct execution"
            );
        }
        assert!(body.contains("_text: &str"));
        assert!(body.contains("rate_limit_wait(shared, channel_id).await;"));
        assert!(
            body.contains(
                "Unknown or unavailable text command. Use `!help` for supported commands."
            )
        );
        assert_eq!(body.matches(".say(").count(), 1);
        assert!(body.trim_end().ends_with("Ok(())"));
    }
}
