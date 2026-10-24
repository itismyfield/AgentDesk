//! Durable transitions describe effects; Discord, file apply and queue owners execute them later.

use super::QuestionResult;

use super::{ApplyResult, CardState, Consent, ConsentKey, ConsentStore, Preview, Question, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    ExplicitLaunch,
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    KeepHeld,
    RevisionChanged,
    SendCard,
    EnableButtons,
    ShowResult,
    Apply,
    ReleaseThenWake,
}

#[derive(Debug)]
pub(crate) struct Transition {
    pub consent: Consent,
    pub effect: Effect,
}

/// The async owner supplies current settings, registry bindings and validated peer evidence.
#[derive(Clone)]
pub(crate) struct Current {
    pub owner: Option<u64>,
    pub bot: u64,
    pub provider: String,
    pub channel: u64,
    pub message: u64,
    pub nonce: String,
    pub key: ConsentKey,
    pub preview: Preview,
    pub local_node: String,
}

impl Current {
    pub(super) fn check(&self, consent: &Consent, preview: bool) -> Result<(), String> {
        if self.owner != Some(consent.owner)
            || consent.owner == 0
            || self.bot != consent.bot
            || self.provider != consent.provider
            || self.channel != consent.channel
            || self.message != consent.message
            || self.nonce != consent.nonce
            || self.key != consent.key
            || self.local_node != consent.key.node
            || self.preview.witness != consent.preview.witness
            || (preview && self.preview != consent.preview)
        {
            return Err("consent identity or preview changed; use owner consent retry on the execution node".into());
        }
        Ok(())
    }
}

pub(crate) struct Workflow<'a>(pub &'a ConsentStore);

impl Workflow<'_> {
    pub(crate) fn hold(
        &self,
        question: Question,
        now: u64,
        request: Request,
    ) -> Result<Transition, String> {
        let held = self.0.hold_unbound(question, now)?;
        let changed = matches!(&held, QuestionResult::RevisionChanged(_));
        let consent = held.into_consent();
        if changed {
            return Ok(Transition {
                consent,
                effect: Effect::RevisionChanged,
            });
        }
        if request == Request::ExplicitLaunch
            && consent.state == State::Pending
            && consent.card == CardState::Awaiting
        {
            return match self.0.reserve_card(&consent, now) {
                Ok(consent) => Ok(Transition {
                    consent,
                    effect: Effect::SendCard,
                }),
                Err(error) => {
                    // A competing sender may already own this reservation; never send again.
                    let current = self.0.load(&consent.key)?;
                    match current {
                        Some(current)
                            if current.nonce == consent.nonce
                                && current.card != CardState::Awaiting =>
                        {
                            Ok(Transition {
                                consent: current,
                                effect: Effect::KeepHeld,
                            })
                        }
                        _ => Err(error),
                    }
                }
            };
        }
        Ok(Transition {
            consent,
            effect: Effect::KeepHeld,
        })
    }

    pub(crate) fn bind(
        &self,
        reserved: &Consent,
        current: &Current,
        message: u64,
    ) -> Result<Transition, String> {
        current.check(reserved, true)?;
        let consent = self.0.bind_card(reserved, message)?;
        Ok(Transition {
            consent,
            effect: Effect::EnableButtons,
        })
    }

    pub(crate) fn decide(
        &self,
        expected: &Consent,
        current: &Current,
        actor: u64,
        yes: bool,
        now: u64,
    ) -> Result<Transition, String> {
        let consent = self.0.load(&expected.key)?.ok_or("consent missing")?;
        current.check(&consent, consent.state == State::Pending)?;
        if actor != consent.owner || consent.card != CardState::Bound || consent.message == 0 {
            return Err("only the current owner can decide a bound consent card".into());
        }
        if consent.state != State::Pending {
            return Ok(Transition {
                consent,
                effect: Effect::ShowResult,
            });
        }
        if now >= consent.expires {
            let consent = self.0.expire(&consent, now)?;
            return Ok(Transition {
                consent,
                effect: Effect::KeepHeld,
            });
        }
        let consent = match self.0.decide(&consent, actor, yes, now) {
            Ok(consent) => consent,
            Err(error) => {
                let latest = self.0.load(&consent.key)?.ok_or("consent missing")?;
                current.check(&latest, latest.state == State::Pending)?;
                if latest.state == State::Pending {
                    return Err(error);
                }
                return Ok(Transition {
                    consent: latest,
                    effect: Effect::ShowResult,
                });
            }
        };
        Ok(Transition {
            consent,
            effect: if yes { Effect::Apply } else { Effect::KeepHeld },
        })
    }

    pub(crate) fn receipt(
        &self,
        accepted: &Consent,
        result: ApplyResult,
    ) -> Result<Transition, String> {
        let consent = self.0.record_apply(accepted, result)?;
        let effect = if consent.state == State::Applied {
            Effect::ReleaseThenWake
        } else {
            Effect::KeepHeld
        };
        Ok(Transition { consent, effect })
    }

    pub(crate) fn retry(
        &self,
        question: Question,
        actor: u64,
        owner: Option<u64>,
        local_node: &str,
        now: u64,
    ) -> Result<Transition, String> {
        if owner != Some(actor)
            || actor != question.owner
            || actor == 0
            || local_node != question.key.node
        {
            return Err("consent retry requires the current owner on the execution node".into());
        }
        let consent = self.0.retry_unbound(question, now)?;
        if consent.state == State::Applied {
            let effect = if consent.releasable_intents().is_empty() {
                Effect::ShowResult
            } else {
                Effect::ReleaseThenWake
            };
            return Ok(Transition { consent, effect });
        }
        let consent = self.0.reserve_card(&consent, now)?;
        Ok(Transition {
            consent,
            effect: Effect::SendCard,
        })
    }

    pub(crate) fn recover(&self, key: &ConsentKey) -> Result<Option<Transition>, String> {
        self.0
            .load(key)?
            .map(|consent| {
                let effect = if consent.state == State::Applied {
                    Effect::ReleaseThenWake
                } else {
                    Effect::KeepHeld
                };
                Ok(Transition { consent, effect })
            })
            .transpose()
    }

    pub(crate) fn boot_recover(
        &self,
        key: &ConsentKey,
        authority: &super::boot_recovery::BootRecovery<'_>,
    ) -> Result<Option<Transition>, String> {
        authority.check()?;
        let Some(mut transition) = self.recover(key)? else {
            return Ok(None);
        };
        if transition.consent.state == State::Accepted {
            let observed = super::observe_interrupted(&transition.consent)?;
            transition.consent = self.0.interrupt_accepted(&transition.consent, observed)?;
        }
        Ok(Some(transition))
    }
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
