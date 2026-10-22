use super::super::{PeerSource, apply};
use super::settlement::{self, CustodySink};
use super::workflow::{Current, Effect, Request, Transition, Workflow};
use super::{ApplyResult, Consent, ConsentKey, ConsentStore, Question, State, card};
use super::{apply_owner, preview::Prepared};
use std::sync::Arc;

pub(crate) enum Event {
    Hold {
        question: Box<Question>,
        now: u64,
        request: Request,
    },
    Bind {
        reserved: Box<Consent>,
        current: Box<Current>,
        message: u64,
    },
    Decide {
        expected: Box<Consent>,
        current: Box<Current>,
        actor: u64,
        yes: bool,
        now: u64,
    },
    Retry {
        question: Box<Question>,
        actor: u64,
        owner: Option<u64>,
        local_node: String,
        now: u64,
    },
    Recover(ConsentKey),
}

#[async_trait::async_trait]
pub(crate) trait Owner: Send {
    async fn current(&mut self, consent: &Consent) -> Result<Current, String>;
}

#[async_trait::async_trait]
pub(crate) trait Cards: Send {
    async fn send(&mut self, card: &card::Card) -> Result<u64, String>;
    async fn replace(&mut self, message: u64, card: &card::Card) -> Result<(), String>;
    async fn disable(&mut self, message: u64) -> Result<(), String>;
}

pub(crate) struct Engine<'a, S: CustodySink> {
    pub store: &'a ConsentStore,
    pub source: Arc<dyn PeerSource>,
    pub owner: &'a mut dyn Owner,
    pub cards: &'a mut dyn Cards,
    pub custody: &'a mut S,
}

impl<S: CustodySink> Engine<'_, S> {
    async fn disable_after(&mut self, message: u64, original: String) -> String {
        match self.cards.disable(message).await {
            Ok(()) => original,
            Err(cleanup) => {
                format!("{original}; disabling unbound consent card also failed: {cleanup}")
            }
        }
    }

    pub(crate) async fn handle(
        &mut self,
        event: Event,
        prepared: Option<Prepared>,
    ) -> Result<Option<Transition>, String> {
        let workflow = Workflow(self.store);
        let mut transition = match event {
            Event::Hold {
                question,
                now,
                request,
            } => workflow.hold(*question, now, request)?,
            Event::Bind {
                reserved,
                current,
                message,
            } => workflow.bind(&reserved, &current, message)?,
            Event::Decide {
                expected,
                current,
                actor,
                yes,
                now,
            } => {
                let latest = self.store.load(&expected.key)?.ok_or("consent missing")?;
                self.owner
                    .current(&latest)
                    .await?
                    .check(&latest, latest.state == State::Pending)?;
                if yes && latest.state == State::Pending {
                    let preview = prepared
                        .as_ref()
                        .ok_or("current consent preview is required before apply")?;
                    if !preview.matches_key(&latest.key) || preview.token != latest.preview {
                        return Err(
                            "consent preview changed before decision; preserve held inputs".into(),
                        );
                    }
                }
                workflow.decide(&expected, &current, actor, yes, now)?
            }
            Event::Retry {
                question,
                actor,
                owner,
                local_node,
                now,
            } => workflow.retry(*question, actor, owner, &local_node, now)?,
            Event::Recover(key) => match workflow.recover(&key)? {
                Some(transition) => transition,
                None => return Ok(None),
            },
        };
        if transition.effect == Effect::SendCard {
            let message = self.cards.send(&card::render(&transition.consent)?).await?;
            let bound = match self.owner.current(&transition.consent).await {
                Ok(current) => workflow.bind(&transition.consent, &current, message),
                Err(error) => Err(error),
            };
            transition = match bound {
                Ok(bound) => bound,
                Err(error) => return Err(self.disable_after(message, error).await),
            };
        }
        if transition.effect == Effect::Apply {
            let accepted = &transition.consent;
            if self.store.load(&accepted.key)?.as_ref() != Some(accepted)
                || accepted.state != State::Accepted
            {
                return Err("accepted consent changed before apply; preserve held inputs".into());
            }
            let before = prepared.as_ref().and_then(Prepared::before_hash);
            let preflight = async {
                self.owner.current(accepted).await?.check(accepted, true)?;
                let prepared =
                    prepared.ok_or("current consent preview is required before apply")?;
                if !prepared.matches_key(&accepted.key) || prepared.token != accepted.preview {
                    return Err("consent preview changed before apply; preserve held inputs".into());
                }
                Ok(prepared)
            }
            .await;
            let applied = match preflight {
                Ok(prepared) => {
                    apply_owner::apply(prepared, &accepted.preview, self.source.clone()).await
                }
                Err(detail) => Err(apply::ApplyFailure {
                    stage: apply::Stage::Preview,
                    published: false,
                    backup: None,
                    detail,
                }),
            };
            let result = match applied {
                Ok(applied) => ApplyResult {
                    before: applied.before_hash,
                    after: Some(applied.after_hash),
                    backup: applied.backup,
                    detail: None,
                },
                Err(failure) => ApplyResult {
                    before,
                    after: None,
                    backup: failure.backup,
                    detail: Some(format!(
                        "{:?}; published={}: {}",
                        failure.stage, failure.published, failure.detail
                    )),
                },
            };
            // A failed receipt never reaches the custody sink, even after config publication.
            transition = workflow.receipt(accepted, result)?;
        }
        match transition.effect {
            Effect::EnableButtons => {
                let message = transition.consent.message;
                if let Err(error) = self
                    .cards
                    .replace(message, &card::render(&transition.consent)?)
                    .await
                {
                    return Err(self.disable_after(message, error).await);
                }
            }
            Effect::ReleaseThenWake => {
                if self.owner.current(&transition.consent).await?.local_node
                    != transition.consent.key.node
                {
                    return Err(
                        "consent release requires the execution node; preserve held inputs".into(),
                    );
                }
                transition.consent =
                    settlement::release(self.store, &transition.consent, self.custody)?;
                if transition.consent.message != 0 {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::render(&transition.consent)?,
                        )
                        .await?;
                }
            }
            Effect::RevisionChanged => {
                if transition.consent.message != 0 {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::revision_changed(&transition.consent)?,
                        )
                        .await?;
                }
            }
            Effect::ShowResult | Effect::KeepHeld => {
                if transition.consent.message != 0 && transition.consent.state != State::Pending {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::render(&transition.consent)?,
                        )
                        .await?;
                }
            }
            Effect::SendCard | Effect::Apply => return Err("uninterpreted consent effect".into()),
        }
        Ok(Some(transition))
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
