//! A Herdr reader's typed terminal is admitted from this token's own turn start, the pane's
//! canonical Source and the descriptor its replay reads; it never becomes a pinned range.

use super::*;
use crate::services::provider::CancelToken;

/// What a Herdr reader's typed terminal frame claims; admission trusts none of it on its own.
pub(super) struct HerdrFrame {
    pub(super) provider: ProviderKind,
    pub(super) result: String,
    pub(super) session_id: Option<String>,
    pub(super) path: String,
    pub(super) logical: String,
    pub(super) turn_nonce: String,
    pub(super) source_start: u64,
    pub(super) end: u64,
    pub(super) file: (u64, u64),
    pub(super) kind: NativeTerminalKind,
}

/// A turn's first terminal as the provider's own reader policy accepts it.
struct Replayed {
    kind: NativeTerminalKind,
    start: u64,
    end: u64,
}

impl HerdrFrame {
    /// Commits the kind only when this token's own turn replays to exactly this end; it reads the
    /// marker, binding log and transcript, so it runs off the scheduler. Failure never yields a Done.
    pub(super) fn admit(
        self,
        (local, baseline): (&mut InflightTurnState, &mut InflightTurnState),
        expected: &InflightTurnIdentity,
        can_deliver_directly: bool,
        actor: &std::sync::Arc<CancelToken>,
    ) -> Result<NativeTerminalKind, GuardedSaveOutcome> {
        let refused = GuardedSaveOutcome::AuthorityPinned;
        let state = actor.herdr_interrupt_state().ok_or(refused)?;
        let start = state.turn_start.get().ok_or(refused)?.clone();
        let owner = &state.owner;
        if !can_deliver_directly
            || owner.provider != self.provider.as_str()
            || owner.channel_id != local.channel_id.to_string()
            || owner.logical_key != self.logical
            || actor.tmux_session_name().as_deref() != Some(self.logical.as_str())
            || actor.turn_nonce() != Some(self.turn_nonce.as_str())
        {
            return Err(refused);
        }
        let root = inflight_runtime_root().ok_or(refused)?;
        let path = inflight_state_path(&root, &self.provider, local.channel_id);
        let _lock = lock_inflight_state_path(&path).map_err(|_| GuardedSaveOutcome::IoError)?;
        let mut fresh = read_inflight_state_for_guarded_write(
            &path,
            &self.provider,
            local.channel_id,
            expected,
            "turn_bridge::herdr_terminal_admission",
        )?;
        let channel = local.channel_id;
        // The Source is judged and the kind committed under one source authority, so no clear or
        // rebind lands between them.
        crate::services::tmux_common::with_tmux_source_authority(&self.logical, |_| {
            // Herdr exists only on unix; elsewhere no pane is Herdr's.
            #[cfg(unix)]
            let herdr = crate::services::discord::turn_bridge::herdr_marked(&self.logical);
            #[cfg(not(unix))]
            let herdr = false;
            if !herdr {
                return Err(refused);
            }
            let source =
                canonical_source(channel, &self.logical, &self.provider, &start).ok_or(refused)?;
            #[cfg(test)]
            test_hooks::after_source_check(channel);
            let mut file = std::fs::File::open(&source.path).map_err(|_| refused)?;
            let meta = file.metadata().map_err(|_| refused)?;
            let opened = opened_identity(&meta);
            let named = std::fs::canonicalize(&self.path).ok();
            if opened != Some((source.dev, source.ino))
                || opened != Some(self.file)
                || start.file.is_some_and(|file| Some(file) != opened)
                || named != std::fs::canonicalize(&source.path).ok()
            {
                return Err(refused);
            }
            let replayed = replay(&self.provider, &mut file, &start, meta.len(), actor);
            let replayed = replayed.ok_or(refused)?;
            if replayed.start != self.source_start
                || replayed.end != self.end
                || replayed.kind != self.kind
            {
                return Err(refused);
            }
            if fresh.turn_nonce.as_deref() != Some(self.turn_nonce.as_str())
                || fresh.rebind_origin
                || fresh.restart_mode.is_some()
                || fresh.terminal_delivery_committed
                || fresh.tui_terminal_kind.is_some()
                || !StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
            {
                return Err(GuardedSaveOutcome::from_durable_authority(&fresh));
            }
            fresh.tui_terminal_kind = Some(self.kind);
            let persisted = persist_under_lock_with_snapshot(
                &root,
                &path,
                &fresh,
                "inflight::runtime_stamp::admit_herdr_terminal",
            )
            .map_err(|_| GuardedSaveOutcome::IoError)?
            .ok_or(refused)?;
            baseline.clone_from(&persisted);
            local.tui_terminal_kind = persisted.tui_terminal_kind;
            local.save_generation = persisted.save_generation;
            Ok(self.kind)
        })
    }
}

/// The pane's latest logged Source when this token's execution logged it; a later Pending, refusal
/// or another execution's record leaves none.
fn canonical_source(
    channel: u64,
    logical: &str,
    provider: &ProviderKind,
    start: &crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart,
) -> Option<crate::services::tui_prompt_dedupe::binding_events::SourceId> {
    use crate::services::tui_prompt_dedupe::binding_events::{BindingTarget, binding_events_since};
    let events = binding_events_since(channel, 0).ok()?;
    let event = events.into_iter().rev().find(|event| {
        event.tmux_session == logical && !matches!(event.new, BindingTarget::Rejected { .. })
    })?;
    if event.execution_nonce.as_deref() != Some(start.execution_nonce.as_str())
        || event.provider != provider.as_str()
    {
        return None;
    }
    let source = match event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
        _ => return None,
    };
    let canonical = |path: &std::path::Path| std::fs::canonicalize(path).ok();
    (canonical(&start.source)? == canonical(&source.path)?).then_some(source)
}

fn opened_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// This token's turn replayed from its own recorded start by the provider's reader policy.
fn replay(
    provider: &ProviderKind,
    file: &mut std::fs::File,
    start: &crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart,
    len: u64,
    actor: &std::sync::Arc<CancelToken>,
) -> Option<Replayed> {
    match provider {
        ProviderKind::Codex => {
            let decoder =
                crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::replay_herdr_turn;
            let terminal = decoder(file, start.offset, len, actor)?;
            Some(Replayed {
                kind: terminal.kind,
                start: start.offset,
                end: terminal.end,
            })
        }
        #[cfg(unix)]
        ProviderKind::Claude => {
            let replay = crate::services::claude::herdr_turn::replay_herdr_turn;
            let (start, kind, end) = replay(file, start.submitted_at?, len)?;
            Some(Replayed { kind, start, end })
        }
        _ => None,
    }
}

#[cfg(all(test, unix))]
#[path = "herdr_terminal_admission_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "herdr_terminal_restart_tests.rs"]
mod restart_tests;

#[cfg(test)]
pub(in crate::services::discord) mod test_hooks {
    type Hook = Box<dyn FnMut() + Send>;
    static AFTER_SOURCE_CHECK: std::sync::Mutex<Vec<(u64, Hook)>> =
        std::sync::Mutex::new(Vec::new());

    /// Runs `hook` once between the Source judgement and the commit of `channel`'s admission.
    pub(in crate::services::discord) fn after_source_check_once(channel: u64, hook: Hook) {
        let mut hooks = AFTER_SOURCE_CHECK.lock().unwrap_or_else(|e| e.into_inner());
        hooks.push((channel, hook));
    }

    pub(super) fn after_source_check(channel: u64) {
        let hook = {
            let mut hooks = AFTER_SOURCE_CHECK.lock().unwrap_or_else(|e| e.into_inner());
            let at = hooks.iter().position(|(owner, _)| *owner == channel);
            at.map(|at| hooks.remove(at).1)
        };
        if let Some(mut hook) = hook {
            hook();
        }
    }
}
