//! E1/E2 boot reservation: a sealed plan registers, protects and closes every selected channel
//! before any producer or supervisor starts. No production path calls it yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::activation::plan::InputSelection;
use super::activation::scope::{self, Refusal, RoutingSnapshot, ScopeDecision};
use super::{Entry, Refused, Registration, Registry, held};
use crate::services::discord::input_runtime::fence::{self, Gate};
use crate::services::discord::input_runtime::reconcile::HoldCause;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::ledger::Presence;

/// One selected channel's registration and protected, closed gate, handed to one supervisor.
pub(crate) struct Reserved {
    pub(super) registration: Registration,
    pub(super) root: PathBuf,
    pub(super) gate: Option<Arc<Gate>>,
    // Why boot must stop at once: scope or responsibility held, or an incomplete install.
    pub(super) hold: Option<HoldCause>,
}

/// The sealed boot plan: reservations for supervisors, and channels that stay Legacy.
pub(crate) struct BootPlan {
    pub(crate) reserved: Vec<Reserved>,
    pub(crate) refused: Vec<(u64, Refusal)>,
}

#[derive(Debug)]
pub(crate) enum PlanError {
    Sealed,
    Selection(anyhow::Error),
    Install(u64, Refused),
}

impl Registry {
    /// E1: installs the whole boot plan once, synchronously and before any producer starts. Every
    /// reservation returns with its gate protected and closed; nothing here awaits or reads PG.
    pub(crate) fn reserve_boot(
        &'static self,
        root: &Path,
        selection: &InputSelection,
        config: &crate::config::Config,
        snapshots: &BTreeMap<u64, RoutingSnapshot>,
        responsibility: &BTreeMap<u64, Presence>,
    ) -> Result<BootPlan, PlanError> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if self.sealed.swap(true, Ordering::AcqRel) {
            return Err(PlanError::Sealed);
        }
        let selected = selection.validate(config).map_err(PlanError::Selection)?;
        let mut plan = BootPlan {
            reserved: Vec::new(),
            refused: Vec::new(),
        };
        for (channel, name) in selected {
            let presence = (responsibility.get(&channel).copied()).unwrap_or(Presence::Unreadable);
            // E2 runs here on the routing snapshot; a caller cannot hand in a Candidate.
            let hold = match scope::evaluate(channel, name, snapshots.get(&channel), presence) {
                ScopeDecision::Refused(reason) => {
                    let key = (name.to_owned(), channel);
                    let line = HoldCause::ScopeRefused(reason).health(name, channel);
                    let entry = entries.entry(key).or_default();
                    entry.health.insert("scope", line);
                    self.used.store(true, Ordering::Release);
                    plan.refused.push((channel, reason));
                    continue;
                }
                ScopeDecision::Held(reason) => Some(HoldCause::ScopeHeld(reason)),
                ScopeDecision::Candidate if presence == Presence::Unreadable => {
                    Some(HoldCause::LedgerUnreadable)
                }
                ScopeDecision::Candidate => None,
            };
            let provider = ProviderKind::from_str(name).ok_or(PlanError::Selection(
                anyhow::anyhow!("input provider {name} is unknown"),
            ))?;
            let reserved = self.install(&mut entries, &provider, channel, root, hold);
            plan.reserved
                .push(reserved.map_err(|refused| PlanError::Install(channel, refused))?);
        }
        Ok(plan)
    }

    /// Registers, protects and closes one channel. A failed protect or close keeps the
    /// registration as a hold rather than removing what is already installed.
    fn install(
        &'static self,
        entries: &mut BTreeMap<(String, u64), Entry>,
        provider: &ProviderKind,
        channel: u64,
        root: &Path,
        hold: Option<HoldCause>,
    ) -> Result<Reserved, Refused> {
        let registration = self.insert(entries, provider, channel, root)?;
        // A second provider's gate on one channel would let either writer miss the other's close.
        let rival = fence::channel_gate(channel).is_some_and(|gate| gate.provider() != provider);
        let gate = (!rival)
            .then(|| Gate::protect(provider.clone(), channel).ok())
            .flatten();
        let closed = gate.as_ref().map(|gate| registration.closing(gate).is_ok());
        let hold = hold.or(match closed {
            None => Some(held("protect")),
            Some(false) => Some(held("close")),
            Some(true) => None,
        });
        if let (Some(cause), Some(entry)) = (&hold, entries.get_mut(&registration.key)) {
            let (name, channel) = &registration.key;
            entry
                .health
                .insert(cause.slot(), cause.health(name, *channel));
        }
        Ok(Reserved {
            registration,
            root: root.to_path_buf(),
            gate,
            hold,
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve_for_test(
        &'static self,
        provider: &ProviderKind,
        channel: u64,
        root: &Path,
    ) -> Result<Reserved, Refused> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        self.install(&mut entries, provider, channel, root, None)
    }
}
