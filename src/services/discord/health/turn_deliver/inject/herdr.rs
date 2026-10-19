//! Busy-turn injection into a Claude turn on a Herdr pane: the channel's Bound execution names the
//! pane, and the paste and Enter go only through that execution's gate, never to local tmux.

use std::path::Path;
use std::time::Instant;

use super::names::{unconfirmed_name, veto_name};
use super::{InjectAttempt, Input, unconfirmed};
use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::services::claude_tui::busy_inject::{self, Unconfirmed, Veto};
use crate::services::claude_tui::composer_lock::{self, ComposerAdmission};
use crate::services::claude_tui::host_input::{InputRun, run_herdr};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, herdr_endpoints};

/// The Herdr gate refused before the paste; nothing reached the pane.
const HERDR_REFUSED: &str = "herdr_refused";

/// The gated pane of the Herdr-configured channel's Bound Claude execution under `session`, read
/// from the row the host check reads; any other row or lookup has none.
pub(super) async fn bound_pane(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    session: &str,
) -> Option<HerdrTarget> {
    if *provider != ProviderKind::Claude
        || crate::config::session_hosts::herdr_endpoint(channel).is_none()
    {
        return None;
    }
    let hashes = [shared.token_hash.clone()];
    let derive = crate::services::discord::host_key_derivation::derive_hosted_lookup;
    let lookup = derive(
        shared.pg_pool.as_ref()?,
        &hashes,
        provider,
        channel,
        session,
    )
    .await;
    let HostedLookup::Found(found) = lookup else {
        return None;
    };
    let HostedRecord::Known(record) = found.record else {
        return None;
    };
    let owner = &record.owner;
    let ours = owner.provider == provider.as_str()
        && owner.channel_id == channel.to_string()
        && owner.logical_key == session;
    (record.state == HostedState::Bound && ours)
        .then(|| herdr_endpoints().target(&record))
        .flatten()
}

/// One gated paste and Enter into an empty composer under a live turn, never sent twice; only the
/// exact frame in the transcript makes it injected, and a refusal before the paste sends nothing.
pub(super) fn run(
    pane: &HerdrTarget,
    session: &str,
    transcript: &Path,
    turn_id: Option<String>,
    input: &Input,
) -> InjectAttempt {
    let (source, author, name) = (&input.source, &input.author, &input.name);
    let text = busy_inject::frame(source, author, name, &input.nonce, &input.text);
    if input.text.trim().is_empty() || text.len() > busy_inject::MAX_INPUT_BYTES {
        return InjectAttempt::NotSent(veto_name(Veto::InvalidInput));
    }
    let locked = composer_lock::try_with_composer_mutation_lock(session, || {
        if let Err(veto) = judge(pane, session, transcript) {
            return InjectAttempt::NotSent(veto_name(veto));
        }
        let Ok(offset) = std::fs::metadata(transcript).map(|meta| meta.len()) else {
            return InjectAttempt::NotSent(veto_name(Veto::TranscriptUnavailable));
        };
        let plan = [
            TuiInputAction::PasteBuffer(text.clone()),
            TuiInputAction::Enter,
        ];
        match run_herdr(pane, &plan, None) {
            // The server's write ACK is not Claude's receipt; the transcript decides, as on tmux.
            InputRun::Applied => {
                let timing = &busy_inject::TIMING;
                let deadline = Instant::now() + timing.confirm_window;
                let receipt = (transcript, offset, input.nonce.as_str(), text.as_str());
                if let Err(detail) =
                    busy_inject::await_receipt(receipt, deadline, timing.confirm_poll)
                {
                    return unconfirmed(input, session, turn_id, unconfirmed_name(detail));
                }
                tracing::info!(
                    channel_id = input.channel,
                    pane = pane.pane_id(),
                    nonce = %input.nonce,
                    "busy Herdr turn took the human input"
                );
                InjectAttempt::Injected { turn_id }
            }
            InputRun::Refused(refusal) => {
                tracing::info!(channel_id = input.channel, ?refusal, "herdr gate refused");
                InjectAttempt::NotSent(HERDR_REFUSED)
            }
            InputRun::Indeterminate { confirmed: 0, .. } => {
                let detail = unconfirmed_name(Unconfirmed::PasteFailed);
                unconfirmed(input, session, turn_id, detail)
            }
            _ => unconfirmed(
                input,
                session,
                turn_id,
                unconfirmed_name(Unconfirmed::EnterFailed),
            ),
        }
    });
    locked.unwrap_or(InjectAttempt::NotSent(veto_name(Veto::LockContended)))
}

/// The pane's composer is empty and unguarded, no modal shows, and the screen and transcript both
/// show a live turn, so the Enter queues behind it.
fn judge(pane: &HerdrTarget, session: &str, transcript: &Path) -> Result<(), Veto> {
    use crate::services::tmux_common as tc;
    let capture = pane.capture(-80).ok_or(Veto::PaneUnavailable)?;
    if tc::tmux_capture_indicates_claude_tui_interactive_modal(&capture) {
        return Err(Veto::Modal);
    }
    let admission = composer_lock::composer_admission(session, || Some(capture.clone()));
    if admission != ComposerAdmission::Any
        || tc::tmux_capture_indicates_claude_tui_prompt_draft(&capture)
        || !tc::tmux_capture_indicates_claude_tui_exact_empty_composer(&capture)
    {
        return Err(Veto::Draft);
    }
    let turn = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(transcript);
    if !turn.is_busy() || !tc::tmux_capture_indicates_claude_tui_busy(&capture) {
        return Err(Veto::NotBusy);
    }
    Ok(())
}

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod tests;
