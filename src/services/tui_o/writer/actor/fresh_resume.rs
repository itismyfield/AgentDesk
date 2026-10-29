//! Rescope only at a new proven root, with no work or reader obligation left in flight.

use super::*;

#[cfg(test)]
static WORK: (AtomicUsize, std::sync::atomic::AtomicBool) = (
    AtomicUsize::new(0),
    std::sync::atomic::AtomicBool::new(false),
);

#[cfg(test)]
pub(super) fn note_work(owed: usize, unsealed: bool) {
    WORK.0.store(owed, Ordering::SeqCst);
    WORK.1.store(unsealed, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn work_for_tests() -> (usize, bool) {
    (WORK.0.load(Ordering::SeqCst), WORK.1.load(Ordering::SeqCst))
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    pub(super) fn rescope(&mut self) -> bool {
        if self.sources.transition_provider()
            != crate::services::tui_o::shadow::ShadowProvider::Codex
        {
            return false;
        }
        let selection = self.writer.suspension.clone();
        let nonce = selection.nonce();
        #[cfg(test)]
        let nonce = if crate::services::claude_tui::source_verify::n2b_mutant("b1b-fix") {
            None
        } else {
            nonce
        };
        #[cfg(test)]
        if nonce.is_some() && crate::services::claude_tui::source_verify::n2b_mutant("b1b-once") {
            return false;
        }
        let channel = self.writer.channel();
        if !crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_pending(
            channel,
            nonce.as_deref(),
        ) {
            return false;
        }
        let quiescent = self.owed.is_empty()
            && !self.deriver.has_unsealed()
            && !self.deriver.has_pending_projection_probe()
            && !self.writer.has_local_retry()
            && self.writer.resume_pieces().is_empty()
            && self.writer.store().ledger().unresolved().is_none()
            && !self.transitions.has_apply_job()
            && self.sources.quiescent_for_rescope();
        #[cfg(test)]
        let quiescent = quiescent
            || crate::services::claude_tui::source_verify::n2b_mutant("b1b-refreeze-noq")
            || crate::services::claude_tui::source_verify::n2b_mutant("b1b-fix");
        let sources: Vec<_> = self
            .writer
            .store()
            .cursors()
            .map(|cursor| cursor.source.clone())
            .collect();
        let mut persisted = Ok(());
        let result = crate::services::tui_prompt_dedupe::clear_relaunch::with_fresh_scope(
            channel,
            nonce.as_deref(),
            &sources,
            quiescent,
            |nonce, denied| {
                let changed = !denied.is_empty();
                persisted = selection.install(self.writer.store(), nonce, denied);
                if changed && persisted.is_ok() {
                    self.sources.reset_for_rescope();
                }
            },
        );
        if let Err(error) = persisted {
            self.writer.stop(WriterAlarm::Halted {
                detail: format!("fresh selection write: {error:?}"),
            });
            return false;
        }
        match result {
            Ok(changed) => {
                self.scope_hold = None;
                if changed {
                    let boot_cross_nonce =
                        self.boot_previous_nonce.take().is_some_and(|previous| {
                            selection.nonce().is_some_and(|current| current != previous)
                        });
                    #[cfg(test)]
                    let boot_cross_nonce = boot_cross_nonce
                        && !crate::services::claude_tui::source_verify::n2b_mutant(
                            "b1b-boot-xnonce-alarm",
                        );
                    if boot_cross_nonce {
                        tracing::warn!(
                            channel,
                            reason = "cross_nonce_boot_rescope",
                            "[tui_o] previous incarnation remains suspended"
                        );
                        let incident = format!("cross_nonce_boot_rescope:{}", uuid::Uuid::new_v4());
                        #[cfg(test)]
                        let remember = !crate::services::claude_tui::source_verify::n2b_mutant(
                            "b1b-boot-xnonce-incident",
                        );
                        #[cfg(not(test))]
                        let remember = true;
                        if remember {
                            self.waiting.incident = Some(incident.clone());
                        }
                        self.writer.alarm(WriterAlarm::WaitingTooLong {
                            incident,
                            ready: self.owed.len(),
                            prepared: None,
                        });
                    }
                    self.deriver = configured_deriver(
                        channel,
                        crate::services::tui_o::shadow::ShadowProvider::Codex,
                        self.writer.store().ledger(),
                    );
                }
                changed
            }
            Err(reason) => {
                if self.scope_hold != Some(reason) {
                    tracing::info!(channel, reason, "[tui_o] source resume held");
                    self.scope_hold = Some(reason);
                    if reason == "new_root_waiting" {
                        #[cfg(test)]
                        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-root-alarm")
                        {
                            return false;
                        }
                        let prepared = self
                            .writer
                            .store()
                            .ledger()
                            .unresolved()
                            .map(|(serial, _)| serial);
                        let incident = format!("new_root_waiting:{}", uuid::Uuid::new_v4());
                        #[cfg(test)]
                        let remember = !crate::services::claude_tui::source_verify::n2b_mutant(
                            "b1b-root-incident",
                        );
                        #[cfg(not(test))]
                        let remember = true;
                        if remember {
                            self.waiting.incident = Some(incident.clone());
                        }
                        self.writer.alarm(WriterAlarm::WaitingTooLong {
                            incident,
                            ready: self.owed.len(),
                            prepared,
                        });
                    }
                }
                false
            }
        }
    }
}

/// Startup and incarnation rebuilds retain the same durable projection dispositions.
pub(super) fn configured_deriver(
    channel: u64,
    provider: ShadowProvider,
    ledger: &crate::services::tui_o::store::ledger::LedgerState,
) -> UnitDeriver {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-bare-deriver") {
        return UnitDeriver::new(channel, provider);
    }
    let inputs = super::super::pieces::ToolCardInputs {
        known: ledger.non_card_tool_keys(),
        recorded_cards: ledger.card_tool_keys(),
        switch: crate::services::tui_o::store::tool_card_switch::ToolCardSwitch::for_channel(
            channel,
        ),
    };
    let summary = super::super::pieces::SummaryInputs {
        turn_marks: ledger.summary_turn_marks(),
        disposed: ledger.disposed_unit_hashes(),
        context_window: (provider == ShadowProvider::Claude)
            .then(|| crate::services::provider::ProviderKind::Claude.resolve_context_window(None)),
    };
    UnitDeriver::new(channel, provider)
        .with_input_notices(super::super::input_notices::Projection::for_provider(
            channel, provider,
        ))
        .with_tool_cards(inputs)
        .with_turn_summary(summary)
}
