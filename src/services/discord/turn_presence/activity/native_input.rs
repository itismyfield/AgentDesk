//! Read-only native candidates from the activity reader; no result or cleanup producer is enabled.
use super::{InputFacts, Outcome, Watch};
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::tui_o::writer::input_facts::native::Observation;
use std::time::{Instant, SystemTime};

pub(super) fn prepare(facts: &mut InputFacts) {
    let result = (|| {
        let history = Registry::runtime()?;
        if history
            .native_evidence_registrations()?
            .iter()
            .any(|r| r.matches(facts.binding()))
        {
            facts.observe_native()?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = result {
        tracing::debug!(%error, "native capture unavailable; completion remains pending");
    }
}

pub(super) fn snapshot(watch: &Watch, history: &Registry) -> Result<Option<Observation>, String> {
    let facts = match &watch.outcome {
        Outcome::Source { facts, .. } if facts.native_capture_enabled() => facts,
        _ => return Ok(None),
    };
    if !facts.caught_up()? {
        return Err("native capture still catching up".into());
    }
    let key = watch.key.as_ref().ok_or("native watch unbound")?;
    if facts.binding().channel_id != key.channel || facts.binding().provider != key.provider {
        return Err("native watch source mismatch".into());
    }
    Ok(Some(Observation {
        binding: facts.binding().clone(),
        generation: watch.generation,
        binding_seq: key.seq,
        revision: watch.revision,
        through: facts.observation_progress().0,
        evidence: facts.native_evidence(&history.native_evidence_registrations()?)?,
    }))
}

pub(super) fn observe(watch: &Watch) {
    if !matches!(&watch.outcome, Outcome::Source { facts, .. } if facts.native_capture_enabled()) {
        return;
    }
    let result = Registry::runtime().and_then(|history| snapshot(watch, &history));
    match result {
        Ok(Some(batch)) => {
            tracing::debug!(candidate = ?serde_json::to_value(batch), "native input observation; settlement inactive")
        }
        Ok(None) => {}
        Err(error) => tracing::debug!(%error, "native observation pending"),
    }
}

/// The file's last write as a monotonic instant, so a restart does not read an old turn as fresh.
pub(super) fn modified_at(facts: &InputFacts) -> Option<Instant> {
    let modified = std::fs::metadata(&facts.binding().source.path)
        .and_then(|meta| meta.modified())
        .ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Instant::now().checked_sub(age)
}
