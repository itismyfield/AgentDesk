//! A child store's logical retirement: one transition at a time, compared by lifecycle generation
//! and input epoch. A retired child stays O's; only its writer and refresh candidacy end.

use std::sync::PoisonError;

use super::super::BootChannels;
use super::ChildEntries;
use crate::services::tui_o::store::OStore;
use crate::services::tui_o::store::thread_children::ChildLifecycle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Active,
    /// One retirement or supersede is in flight; inputs are held and counted meanwhile.
    Moving,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Lifecycle {
    phase: Phase,
    generation: u64,
    /// Inputs seen by this process; any input stales a ticket pinned before it.
    epoch: u64,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            phase: Phase::Active,
            generation: 1,
            epoch: 0,
        }
    }
}

/// What thread_gc pinned before its row delete; it names one generation and input epoch only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetirementTicket {
    channel: u64,
    generation: u64,
    epoch: u64,
}

impl RetirementTicket {
    pub(crate) fn channel(&self) -> u64 {
        self.channel
    }
}

impl ChildEntries {
    /// Boot reads the child's mark once; a mark it cannot read whole never retires the child.
    pub(super) fn rehydrate(&self, channel: u64, store: &OStore) {
        let life = match store.read_child_lifecycle(channel) {
            Ok(None) => Lifecycle::default(),
            Ok(Some(mark)) => Lifecycle {
                phase: if mark.retired {
                    Phase::Retired
                } else {
                    Phase::Active
                },
                generation: mark.generation,
                epoch: 0,
            },
            Err(error) => {
                tracing::warn!(
                    channel,
                    ?error,
                    "[tui_o] thread child lifecycle mark unreadable; the child stays active"
                );
                Lifecycle::default()
            }
        };
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(&channel) {
            entry.life = life;
        }
    }
}

impl BootChannels {
    fn with_life<R>(&self, channel: u64, change: impl FnOnce(&mut Lifecycle) -> R) -> Option<R> {
        let mut entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Some(change(&mut entries.get_mut(&channel)?.life))
    }

    /// Whether `channel` is a known child whose store is retired or mid-transition, so no writer
    /// hosts it and no refresh visits it.
    pub(crate) fn child_dormant(&self, channel: u64) -> bool {
        self.with_life(channel, |life| life.phase != Phase::Active)
            .unwrap_or(false)
    }

    /// `output_channels` without dormant children: what hosting and the owner refresh visit.
    pub(crate) fn proactive_channels(&self) -> std::collections::BTreeSet<u64> {
        let mut channels = self.output_channels();
        channels.retain(|channel| !self.child_dormant(*channel));
        channels
    }

    /// Pins a resolved, active child's generation and input epoch for one retirement request.
    pub(crate) fn child_retirement_ticket(&self, channel: u64) -> Option<RetirementTicket> {
        let entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(&channel)?;
        let life = entry.life;
        (life.phase == Phase::Active && matches!(entry.read(), Some(Ok(_)))).then_some(
            RetirementTicket {
                channel,
                generation: life.generation,
                epoch: life.epoch,
            },
        )
    }

    /// Takes the child's transition only for the generation and epoch the ticket pinned.
    pub(crate) fn begin_child_retirement(&self, ticket: &RetirementTicket) -> bool {
        let pinned = Lifecycle {
            phase: Phase::Active,
            generation: ticket.generation,
            epoch: ticket.epoch,
        };
        self.with_life(ticket.channel, |life| {
            let exact = *life == pinned;
            if exact {
                life.phase = Phase::Moving;
            }
            exact
        })
        .unwrap_or(false)
    }

    /// Returns a transition this ticket took and did not commit; the store stays active.
    pub(crate) fn abort_child_retirement(&self, ticket: &RetirementTicket) {
        self.with_life(ticket.channel, |life| {
            if life.phase == Phase::Moving && life.generation == ticket.generation {
                life.phase = Phase::Active;
            }
        });
    }

    /// Publishes the retirement mark for the transition this ticket holds. `Ok(false)`: an input
    /// arrived first, so the store stayed or became active again under a new generation.
    pub(crate) fn commit_child_retirement(
        &self,
        ticket: &RetirementTicket,
    ) -> Result<bool, String> {
        let unchanged = self.with_life(ticket.channel, |life| {
            let held = life.phase == Phase::Moving && life.generation == ticket.generation;
            if held && life.epoch != ticket.epoch {
                life.phase = Phase::Active;
            }
            held && life.epoch == ticket.epoch
        });
        if unchanged != Some(true) {
            return Ok(false);
        }
        // Written with no entry lock held, so the epoch is compared again after it.
        if let Err(error) = self.publish_child_mark(ticket.channel, ticket.generation, true) {
            self.abort_child_retirement(ticket);
            return Err(error);
        }
        let retired = self.with_life(ticket.channel, |life| {
            life.phase = Phase::Retired;
            life.epoch == ticket.epoch
        });
        if retired == Some(true) {
            return Ok(true);
        }
        self.supersede_child_retirement(ticket.channel)
            .map(|()| false)
    }

    /// Replaces a retirement mark with a new active generation over the same store.
    fn supersede_child_retirement(&self, channel: u64) -> Result<(), String> {
        let retired = self.with_life(channel, |life| {
            (life.phase == Phase::Retired).then(|| {
                life.phase = Phase::Moving;
                life.generation
            })
        });
        let Some(Some(generation)) = retired else {
            return Ok(());
        };
        let published = self.publish_child_mark(channel, generation + 1, false);
        self.with_life(channel, |life| match &published {
            Ok(()) => {
                life.phase = Phase::Active;
                life.generation = generation + 1;
            }
            Err(_) => life.phase = Phase::Retired,
        });
        published
    }

    /// An input at the child's placement or claim boundary stales older tickets and supersedes a
    /// retirement; `Err` holds it while a transition is in flight or the mark cannot be replaced.
    pub(crate) fn child_input_arrived(&self, channel: u64) -> Result<(), String> {
        let phase = self.with_life(channel, |life| {
            life.epoch += 1;
            life.phase
        });
        match phase {
            None | Some(Phase::Active) => Ok(()),
            Some(Phase::Moving) => Err(format!(
                "O thread {channel} is in a store lifecycle transition"
            )),
            Some(Phase::Retired) => self.supersede_child_retirement(channel),
        }
    }

    fn publish_child_mark(
        &self,
        channel: u64,
        generation: u64,
        retired: bool,
    ) -> Result<(), String> {
        let store = crate::config::runtime_root()
            .as_deref()
            .and_then(OStore::existing)
            .ok_or("O store unavailable for a child lifecycle mark")?;
        store
            .publish_child_lifecycle(channel, ChildLifecycle::new(generation, retired))
            .map_err(|error| format!("child lifecycle mark: {error:?}"))
    }
}
