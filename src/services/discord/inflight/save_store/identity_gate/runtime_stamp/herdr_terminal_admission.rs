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
    fn refuse(
        &self,
        axis: &str,
        replayed: Option<&Replayed>,
        outcome: GuardedSaveOutcome,
    ) -> GuardedSaveOutcome {
        tracing::warn!(
            provider = self.provider.as_str(),
            logical = %self.logical,
            axis,
            source_start = self.source_start,
            source_end = self.end,
            replay_start = ?replayed.map(|turn| turn.start),
            replay_end = ?replayed.map(|turn| turn.end),
            "herdr terminal admission refused"
        );
        outcome
    }

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
        let state = actor
            .herdr_interrupt_state()
            .ok_or_else(|| self.refuse("actor", None, refused))?;
        let start = state
            .turn_start
            .get()
            .ok_or_else(|| self.refuse("turn_start", None, refused))?
            .clone();
        let owner = &state.owner;
        if !can_deliver_directly
            || owner.provider != self.provider.as_str()
            || owner.channel_id != local.channel_id.to_string()
            || owner.logical_key != self.logical
            || actor.tmux_session_name().as_deref() != Some(self.logical.as_str())
            || actor.turn_nonce() != Some(self.turn_nonce.as_str())
        {
            return Err(self.refuse("actor", None, refused));
        }
        let root =
            inflight_runtime_root().ok_or_else(|| self.refuse("authority", None, refused))?;
        let path = inflight_state_path(&root, &self.provider, local.channel_id);
        let _lock = lock_inflight_state_path(&path)
            .map_err(|_| self.refuse("authority", None, GuardedSaveOutcome::IoError))?;
        let mut fresh = read_inflight_state_for_guarded_write(
            &path,
            &self.provider,
            local.channel_id,
            expected,
            "turn_bridge::herdr_terminal_admission",
        )
        .map_err(|outcome| self.refuse("authority", None, outcome))?;
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
                return Err(self.refuse("source", None, refused));
            }
            let source = canonical_source(channel, &self.logical, &self.provider, &start)
                .ok_or_else(|| self.refuse("source", None, refused))?;
            #[cfg(test)]
            test_hooks::after_source_check(channel);
            let mut file = std::fs::File::open(&source.path)
                .map_err(|_| self.refuse("source", None, refused))?;
            let meta = file
                .metadata()
                .map_err(|_| self.refuse("file_identity", None, refused))?;
            let opened = opened_identity(&meta);
            let named = std::fs::canonicalize(&self.path).ok();
            if opened != Some((source.dev, source.ino))
                || opened != Some(self.file)
                || start.file.is_some_and(|file| Some(file) != opened)
                || named != std::fs::canonicalize(&source.path).ok()
            {
                return Err(self.refuse("file_identity", None, refused));
            }
            let replayed = replay(&self.provider, &mut file, &start, meta.len(), actor);
            let replayed = replayed.ok_or_else(|| self.refuse("replay_missing", None, refused))?;
            if replayed.start != self.source_start
                && !crate::services::provider::herdr_before_start::mutant(
                    "start_offset_no_strict_start",
                )
            {
                return Err(self.refuse("source_start", Some(&replayed), refused));
            }
            if replayed.end != self.end {
                return Err(self.refuse("end", Some(&replayed), refused));
            }
            if replayed.kind != self.kind {
                return Err(self.refuse("kind", Some(&replayed), refused));
            }
            if fresh.turn_nonce.as_deref() != Some(self.turn_nonce.as_str())
                || fresh.rebind_origin
                || fresh.restart_mode.is_some()
                || fresh.terminal_delivery_committed
                || fresh.tui_terminal_kind.is_some()
                || !StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
            {
                return Err(self.refuse(
                    "authority",
                    Some(&replayed),
                    GuardedSaveOutcome::from_durable_authority(&fresh),
                ));
            }
            fresh.tui_terminal_kind = Some(self.kind);
            // The bridge ends on this kind before any runtime handoff, so the row names its TUI
            // here: the body's owner is judged from it.
            fresh.runtime_kind = fresh.runtime_kind.or(match self.provider {
                ProviderKind::Claude => Some(RuntimeHandoffKind::ClaudeTui),
                ProviderKind::Codex => Some(RuntimeHandoffKind::CodexTui),
                _ => None,
            });
            let persisted = persist_under_lock_with_snapshot(
                &root,
                &path,
                &fresh,
                "inflight::runtime_stamp::admit_herdr_terminal",
            )
            .map_err(|_| self.refuse("authority", Some(&replayed), GuardedSaveOutcome::IoError))?
            .ok_or_else(|| self.refuse("authority", Some(&replayed), refused))?;
            #[cfg(all(test, unix))]
            {
                use crate::services::discord::recovery_engine::herdr_admitted_restart::test_hooks;
                test_hooks::after_admitted_persist(channel, persisted.tui_terminal_kind);
            }
            baseline.clone_from(&persisted);
            local.tui_terminal_kind = persisted.tui_terminal_kind;
            local.runtime_kind = persisted.runtime_kind;
            local.save_generation = persisted.save_generation;
            Ok(self.kind)
        })
    }
}

#[cfg(unix)]
impl InflightTurnState {
    /// Takes a drain restart's authority only while the locked row is still this admitted episode
    /// under that drain marker; the marker is consumed in the same write and its row returned.
    pub(in crate::services::discord) fn take_drain_restart(
        &self,
        provider: &ProviderKind,
        current_generation: u64,
    ) -> Option<InflightTurnState> {
        use crate::services::discord::{InflightRestartMode, inflight};
        let root = inflight_runtime_root()?;
        let path = inflight_state_path(&root, provider, self.channel_id);
        let _lock = lock_inflight_state_path(&path).ok()?;
        let expected = InflightTurnIdentity::from_state(self);
        let caller = "recovery::herdr_admitted_restart::take_drain_restart";
        let read = read_inflight_state_for_guarded_write;
        let mut fresh = read(&path, provider, self.channel_id, &expected, caller).ok()?;
        let drain = fresh.restart_mode == Some(InflightRestartMode::DrainRestart);
        if !drain
            || self.turn_nonce.as_deref().is_none_or(str::is_empty)
            || fresh.turn_nonce != self.turn_nonce
            || fresh.tui_terminal_kind.is_none()
            || fresh.tui_terminal_kind != self.tui_terminal_kind
            || fresh.save_generation != self.save_generation
            || inflight::row_is_current_generation(&fresh, current_generation)
            || fresh.rebind_origin
            || fresh.herdr_before_start_stop.is_some()
            || !expected.matches_state(&fresh)
            || !inflight::InflightEpisodePin::from_state(self).matches_state(&fresh)
        {
            return None;
        }
        fresh.readopted_from_inflight = true;
        fresh.clear_restart_mode();
        persist_under_lock_with_snapshot(&root, &path, &fresh, caller).ok()?
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
