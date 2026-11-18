//! Boundary progress never settles a refused run's delivery obligation.
use super::*;
use crate::services::tui_o::channel_policy::{Candidate, LegacyRun};
use crate::services::tui_o::writer::late_adopt::claude::{
    Owed, Sample, nonce_of, old_source_done_with_sends,
};
use crate::services::tui_o::writer::rotation::bound_source;
use std::sync::Arc;

pub(in crate::services::tui_o::writer) struct ChildDebt {
    runs: Vec<LegacyRun>,
    owed: Vec<Owed>,
    sends: (u64, u64),
}
impl ChildDebt {
    pub(in crate::services::tui_o::writer) async fn pin<B: BindingEvents>(
        candidate: &Candidate,
        legacy: Arc<dyn LegacyView>,
        log: Arc<B>,
        channel: u64,
    ) -> Result<Self, String> {
        let (runs, sends) = {
            let _state = candidate.lock();
            (candidate.owed_runs(), candidate.sends())
        };
        if runs.is_empty() {
            return Ok(Self {
                runs,
                owed: Vec::new(),
                sends,
            });
        }
        if sends.0 != sends.1 {
            return Err("old child delivery is still in flight".into());
        }
        tokio::task::spawn_blocking(move || {
            let events = log
                .binding_events_since(channel, 0)
                .map(super::super::renumbered::first_named)?;
            let current = super::current(&events);
            let mut owed = Vec::new();
            for run in &runs {
                let source = bound_source(&run.binding).ok_or("old child has no source")?;
                if nonce_of(&events, source).as_deref()
                    != Some(run.binding.execution_nonce.as_str())
                {
                    return Err("old child execution changed".into());
                }
                let len = len_of(&source.path)?;
                if len < run.observed_end {
                    return Err("old child source shrank".into());
                }
                let cursor = legacy.late_cursor(&run.binding.tmux_session);
                let cursor_matches = match &cursor {
                    Ok(LegacyCursor::NoPane) => true,
                    Ok(LegacyCursor::Bound { path, .. }) => path == &source.path,
                    _ => false,
                };
                let is_current = cursor_matches
                    && current
                        .as_ref()
                        .is_some_and(|(s, tmux)| s == source && tmux == &run.binding.tmux_session);
                let frontier = if is_current {
                    legacy.frontier(channel, &run.binding.tmux_session, len)
                } else {
                    legacy.historical_frontier(
                        &run.binding,
                        run.turn.as_ref(),
                        run.delivery.as_ref(),
                        len,
                    )
                };
                // Without a receipt, only a source containing no output can settle at zero.
                let frontier = frontier.unwrap_or(0);
                let mut turns = Turns::new(frontier);
                let pinned = read(source, len, Some(&mut turns)).map_err(|error| match error {
                    Unread::Short(e) | Unread::Other(e) => e,
                })?;
                if frontier > len || !turns.frontier_on_line || turns.undelivered.is_some() {
                    return Err("old child still owes output before its source end".into());
                }
                pinned.unchanged()?;
                if is_current && legacy.late_cursor(&run.binding.tmux_session).ok() != cursor.ok() {
                    return Err("old child source changed during delivery proof".into());
                }
                owed.push(Owed {
                    old: (source.clone(), run.binding.seq),
                    sample: Some(Sample {
                        source: source.clone(),
                        nonce: run.binding.execution_nonce.clone(),
                        len,
                        sends,
                    }),
                });
            }
            let after = log
                .binding_events_since(channel, 0)
                .map(super::super::renumbered::first_named)?;
            if after != events {
                return Err("binding changed during old child delivery proof".into());
            }
            Ok(Self { runs, owed, sends })
        })
        .await
        .map_err(|error| format!("child delivery proof task: {error}"))?
    }
    pub(in crate::services::tui_o::writer) fn sends(&self) -> Option<(u64, u64)> {
        (!self.runs.is_empty()).then_some(self.sends)
    }
    /// Called with the adoption lock held, in both ordinary pin and fresh takeover publication.
    pub(in crate::services::tui_o::writer) fn recheck(
        &self,
        events: &[BindingEvent],
        candidate: &Candidate,
    ) -> Result<(), String> {
        if candidate.owed_runs() != self.runs {
            return Err("old child delivery obligations changed".into());
        }
        for owed in &self.owed {
            old_source_done_with_sends(owed.sample.as_ref(), &owed.old, events, candidate.sends())?;
        }
        Ok(())
    }
}
