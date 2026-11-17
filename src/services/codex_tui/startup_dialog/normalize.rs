//! A bounded trust-menu action never submits a prompt or changes launch evidence.
use std::path::Path;
use std::time::Duration;

use crate::services::codex_tui::host_input::trust::TrustTransport;
use crate::services::tui_o::channel_policy::output_disposition::{Output, Phase, Reason};
use crate::services::tui_prompt_dedupe::binding_context::{self as bc, BindingContext};

pub(crate) fn repair_once(
    output: &Output,
    context: &BindingContext,
    cwd: &Path,
    transport: &mut dyn TrustTransport,
) -> Result<String, Reason> {
    current(output, context)?;
    if !output.take_repair() {
        return Err(Reason::TrustUnavailable);
    }
    let root = crate::config::runtime_root().ok_or(Reason::TrustUnavailable)?;
    let cwd = cwd.canonicalize().map_err(|_| Reason::TrustUnavailable)?;
    if crate::services::codex::normalization_trust_override(&root, &cwd).is_none() {
        return Err(Reason::TrustUnavailable);
    }
    let session = &context.tmux_session;
    current(output, context)?;
    let before = transport
        .observe(session, budget(output)?)
        .ok_or(Reason::TrustUnavailable)?;
    if before.cwd.canonicalize().ok().as_ref() != Some(&cwd) {
        return Err(Reason::Invalidated);
    }
    if !selected_trust(&before.text) {
        return Err(Reason::TrustUnavailable);
    }
    // Locks protect identity checks, never host I/O; adoption revalidates after this action.
    current(output, context)?;
    if !transport.enter(session, budget(output)?) {
        return Err(Reason::TrustUnavailable);
    }
    current(output, context)?;
    let after = transport
        .observe(session, budget(output)?)
        .ok_or(Reason::TrustUnavailable)?;
    if after.cwd.canonicalize().ok().as_ref() != Some(&cwd) {
        return Err(Reason::Invalidated);
    }
    current(output, context)?;
    Ok(after.text)
}

fn budget(output: &Output) -> Result<Duration, Reason> {
    // Each existing host executor may spend two seconds reaping each of two commands.
    output
        .remaining()
        .checked_sub(Duration::from_secs(4))
        .filter(|budget| !budget.is_zero())
        .map(|budget| budget.min(Duration::from_millis(200)))
        .ok_or(Reason::Deadline)
}

fn current(output: &Output, context: &BindingContext) -> Result<(), Reason> {
    use crate::services::{tmux_common as tc, tui_prompt_dedupe as dedupe};
    if output.remaining().is_zero() {
        output.expire();
        return Err(Reason::Deadline);
    }
    if output.phase() != Phase::Normalizing {
        return Err(Reason::Invalidated);
    }
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        if context.provider != "codex"
            || context.channel_id != Some(output.id().channel)
            || context.execution_nonce.is_empty()
            || crate::services::tui_o::channel_policy::first_prompt_digest(&context.execution_nonce)
                != output.id().execution
            || bc::execution_context("codex", &context.execution_nonce).as_ref() != Ok(context)
            || context.owner_runtime_root != tc::current_tmux_owner_marker()
            || bc::observe_spawn_nonce_marker(authority.session())
                != bc::SpawnNonceMarker::Known(context.execution_nonce.clone())
            || dedupe::owner_channel_for_tmux_session(authority.session()) != context.channel_id
        {
            return Err(Reason::Invalidated);
        }
        Ok(())
    })
}

fn selected_trust(pane: &str) -> bool {
    if super::classify(pane).map(|(reason, _)| reason) != Some("folder trust") {
        return false;
    }
    let plain = crate::services::tui_input::ansi::strip_ansi_escape_sequences(pane);
    plain
        .lines()
        .rev()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(['›', '❯', '>']))
        .and_then(super::option)
        .is_some_and(|choice| {
            matches!(
                choice.to_ascii_lowercase().as_str(),
                "yes, proceed" | "trust and continue"
            )
        })
}

#[cfg(unix)]
#[path = "normalize_tests.rs"]
mod tests;
