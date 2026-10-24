use super::super::{PeerSource, apply};
use super::settlement::{self, CustodySink};
use super::workflow::Request;
use super::workflow::{Current, Effect, Transition, Workflow};
use super::{ApplyResult, Consent, ConsentKey, ConsentStore, Question, State, card};
use super::{
    apply_owner,
    boot_recovery::{BootRecovery, BootState},
    preview::Prepared,
};
use std::sync::Arc;

pub(crate) enum Event<'a> {
    Hold {
        question: Box<Question>,
        now: u64,
        request: Request,
    },
    #[cfg(test)]
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
    BootRecover(ConsentKey, &'a BootRecovery<'a>),
}

/// Startup uses the same typed recovery dispatch without interpreting card or custody effects.
pub(crate) fn recover(
    store: &ConsentStore,
    event: Event<'_>,
) -> Result<Option<Transition>, String> {
    match event {
        Event::Recover(key) => Workflow(store).recover(&key),
        Event::BootRecover(key, authority) => Workflow(store).boot_recover(&key, authority),
        _ => Err("metadata recovery requires a recovery event".into()),
    }
}

#[async_trait::async_trait]
pub(crate) trait Owner: Send {
    async fn current(&mut self, consent: &Consent) -> Result<Current, String>;
}

#[derive(Debug, Clone)]
pub(crate) enum SendFailure {
    Definitive(String),
    Unknown(String),
}

impl SendFailure {
    fn diagnostic(self, operation: &str) -> String {
        match self {
            Self::Definitive(detail) => format!("consent card {operation} rejected: {detail}"),
            Self::Unknown(detail) => format!("consent card {operation} outcome unknown: {detail}"),
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait Cards: Send {
    async fn send(&mut self, card: &card::Card) -> Result<u64, SendFailure>;
    async fn replace(&mut self, message: u64, card: &card::Card) -> Result<(), SendFailure>;
    async fn disable(&mut self, message: u64) -> Result<(), SendFailure>;
}

pub(crate) struct Engine<'a, S: CustodySink> {
    pub store: &'a ConsentStore,
    pub boot: &'a BootState,
    pub local_node: String,
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
                let cleanup = cleanup.diagnostic("disable");
                format!("{original}; disabling unbound consent card also failed: {cleanup}")
            }
        }
    }

    async fn replace_unbound_after(
        &mut self,
        consent: &Consent,
        message: u64,
        original: String,
    ) -> String {
        let replaced = match card::binding_failed(consent) {
            Ok(card) => self
                .cards
                .replace(message, &card)
                .await
                .map_err(|error| error.diagnostic("replace")),
            Err(error) => Err(error),
        };
        match replaced {
            Ok(()) => original,
            Err(cleanup) => {
                self.disable_after(
                    message,
                    format!("{original}; replacing unbound consent card also failed: {cleanup}"),
                )
                .await
            }
        }
    }

    pub(crate) async fn handle(
        &mut self,
        event: Event<'_>,
        prepared: Option<Prepared>,
    ) -> Result<Option<Transition>, String> {
        let workflow = Workflow(self.store);
        let mut transition = match event {
            Event::Hold {
                question,
                now,
                request,
            } => workflow.hold(*question, now, request)?,
            #[cfg(test)]
            Event::Bind {
                reserved,
                current,
                message,
            } => match workflow.bind(&reserved, &current, message) {
                Ok(bound) => bound,
                Err(error) => {
                    if let Some(latest) = self.store.load(&reserved.key)?
                        && latest.nonce == reserved.nonce
                        && latest.message == message
                        && latest.card == super::CardState::Bound
                        && latest.state == State::Pending
                    {
                        Transition {
                            consent: latest,
                            effect: Effect::KeepHeld,
                        }
                    } else {
                        return Err(self.replace_unbound_after(&reserved, message, error).await);
                    }
                }
            },
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
                    self.boot.require_ready()?;
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
            } => {
                let mut transition = workflow.retry(*question, actor, owner, &local_node, now)?;
                if transition.effect == Effect::SendCard
                    && let Some(preview) = prepared.as_ref()
                    && !preview.plan.requires_write()
                {
                    if !preview.matches_key(&transition.consent.key)
                        || preview.token != transition.consent.preview
                    {
                        return Err("owner recheck preview changed; preserve held inputs".into());
                    }
                    self.boot.require_ready()?;
                    self.owner
                        .current(&transition.consent)
                        .await?
                        .check(&transition.consent, true)?;
                    transition.consent =
                        self.store.accept_recheck(&transition.consent, actor, now)?;
                    transition.effect = Effect::Apply;
                }
                transition
            }
            Event::BootRecover(key, authority) => {
                if !authority.authorizes(self.boot) {
                    return Err(
                        "consent boot recovery authority belongs to another process scope".into(),
                    );
                }
                match recover(self.store, Event::BootRecover(key, authority))? {
                    Some(transition) => transition,
                    None => return Ok(None),
                }
            }
            Event::Recover(key) => match recover(self.store, Event::Recover(key))? {
                Some(transition) => transition,
                None => return Ok(None),
            },
        };
        if transition.effect == Effect::SendCard {
            let message = match self.cards.send(&card::render(&transition.consent)?).await {
                Ok(message) => message,
                Err(SendFailure::Definitive(error)) => {
                    return Err(match self.store.unreserve_card(&transition.consent) {
                        Ok(_) => error,
                        Err(cleanup) => format!(
                            "{error}; clearing unsent card reservation also failed: {cleanup}"
                        ),
                    });
                }
                // The message may exist: retaining the reservation prevents duplicate cards.
                Err(SendFailure::Unknown(error)) => {
                    return Err(format!(
                        "{error}; consent card delivery is unknown; preserve held inputs and use owner consent retry on execution node {}",
                        transition.consent.key.node
                    ));
                }
            };
            let bound = match self.owner.current(&transition.consent).await {
                Ok(current) => workflow.bind(&transition.consent, &current, message),
                Err(error) => Err(error),
            };
            transition = match bound {
                Ok(bound) => bound,
                Err(error) => {
                    return Err(self
                        .replace_unbound_after(&transition.consent, message, error)
                        .await);
                }
            };
        }
        let mut result_kind = card::ResultKind::Existing;
        if transition.effect == Effect::Apply {
            self.boot.require_ready()?;
            let accepted = &transition.consent;
            let before = prepared.as_ref().and_then(Prepared::before_hash);
            let preflight = async {
                self.owner.current(accepted).await?.check(accepted, true)?;
                let latest = self.store.load(&accepted.key)?.ok_or("consent missing")?;
                if (
                    latest.nonce.as_str(),
                    &latest.preview,
                    &latest.decision,
                    &latest.state,
                ) != (
                    accepted.nonce.as_str(),
                    &accepted.preview,
                    &accepted.decision,
                    &State::Accepted,
                ) {
                    return Err(
                        "accepted consent changed before apply; preserve held inputs".into(),
                    );
                }
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
            result_kind = card::ResultKind::FirstApply;
        }
        match transition.effect {
            Effect::EnableButtons => {
                let message = transition.consent.message;
                if let Err(error) = self
                    .cards
                    .replace(message, &card::render(&transition.consent)?)
                    .await
                {
                    return Err(self
                        .disable_after(message, error.diagnostic("replace"))
                        .await);
                }
            }
            Effect::ReleaseThenWake => {
                if self.local_node != transition.consent.key.node {
                    return Err(
                        "consent release requires the execution node; preserve held inputs".into(),
                    );
                }
                transition.consent =
                    settlement::release(self.store, &transition.consent, self.custody).await?;
                if transition.consent.message != 0 {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::render_result(&transition.consent, result_kind)?,
                        )
                        .await
                        .map_err(|error| error.diagnostic("replace"))?;
                }
            }
            Effect::RevisionChanged => {
                if transition.consent.message != 0 {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::revision_changed(&transition.consent)?,
                        )
                        .await
                        .map_err(|error| error.diagnostic("replace"))?;
                }
            }
            Effect::ShowResult | Effect::KeepHeld => {
                if transition.consent.message != 0 && transition.consent.state != State::Pending {
                    self.cards
                        .replace(
                            transition.consent.message,
                            &card::render(&transition.consent)?,
                        )
                        .await
                        .map_err(|error| error.diagnostic("replace"))?;
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
