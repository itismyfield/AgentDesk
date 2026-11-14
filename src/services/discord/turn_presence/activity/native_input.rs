//! Native evidence has its own ticket reader, never a Watch lock or tail cursor.
use super::InputFacts;
#[cfg(all(test, unix))]
use crate::services::tui_input::human_frame::history::v2::Registry;
#[cfg(all(test, unix))]
use crate::services::tui_o::{
    shadow::SourceBinding,
    writer::input_facts::native::{Observation, registrations_fingerprint},
};
use std::time::{Instant, SystemTime};

#[cfg(all(test, unix))]
pub(in crate::services::discord) struct Reader {
    binding: SourceBinding,
    history: Registry,
    facts: Option<InputFacts>,
    revision: u64,
}

#[cfg(all(test, unix))]
impl Reader {
    pub(in crate::services::discord) fn new(binding: SourceBinding, history: Registry) -> Self {
        Self {
            binding,
            history,
            facts: None,
            revision: 0,
        }
    }

    pub(in crate::services::discord) fn poll(
        &mut self,
        budget: u64,
    ) -> Result<Option<Observation>, String> {
        let registrations: Vec<_> = self
            .history
            .native_evidence_registrations()?
            .into_iter()
            .filter(|r| r.matches(&self.binding))
            .collect();
        if registrations.is_empty() {
            self.facts = None;
            return Ok(None);
        }
        if self.facts.is_none() {
            let mut facts = InputFacts::open(self.binding.clone())?;
            facts.observe_native()?;
            self.facts = Some(facts);
        }
        let facts = self.facts.as_mut().ok_or("native reader missing")?;
        facts.poll(budget)?;
        self.revision += 1;
        if !facts.caught_up()? {
            return Ok(None);
        }
        Ok(Some(Observation {
            binding: self.binding.clone(),
            generation: 1,
            binding_seq: 0,
            revision: self.revision,
            through: facts.observation_progress().0,
            registrations_fingerprint: registrations_fingerprint(&registrations)?,
            evidence: facts.native_evidence(&registrations)?,
        }))
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
