//! Cards own references to existing custody, never copies of prompts or uploads.

use super::{
    super::contract::ServerWitness,
    durable::{self, Directory},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, PathBuf};

const TTL: u64 = 30 * 60;
const SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsentKey {
    pub node: String,
    pub user: u32,
    // Use the registry endpoint's socket_path verbatim; do not introduce a second alias policy.
    pub socket: PathBuf,
    pub named_session: String,
    pub config: PathBuf,
    pub target: PathBuf,
}

impl ConsentKey {
    pub(crate) fn normalized(mut self) -> Result<Self, String> {
        self.node = self.node.trim().to_owned();
        self.named_session = self.named_session.trim().to_owned();
        if self.node.is_empty() || self.named_session.is_empty() {
            return Err("consent key requires a node and named session".into());
        }
        for path in [&mut self.socket, &mut self.config, &mut self.target] {
            if !path.is_absolute() {
                return Err(format!(
                    "{}: consent key requires an absolute path",
                    path.display()
                ));
            }
            let mut result = PathBuf::from("/");
            for part in path.components() {
                match part {
                    Component::Normal(part) => result.push(part),
                    Component::ParentDir => {
                        return Err("consent paths must not contain parent-directory components; use an explicit absolute path".into());
                    }
                    Component::RootDir | Component::CurDir => {}
                    _ => return Err("unsupported consent path".into()),
                }
            }
            if result.file_name().is_none() {
                return Err("consent key requires a file path".into());
            }
            *path = result;
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preview {
    pub revision: u64,
    #[serde(with = "super::witness_codec")]
    pub witness: ServerWitness,
    pub target_hash: [u8; 32],
    pub chain_hash: [u8; 32],
    pub checker_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntentRef {
    pub store: String,
    pub id: String,
    pub provider: String,
    pub channel: u64,
    pub source_message: u64,
    pub fifo: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum State {
    Pending,
    Denied,
    Expired,
    Accepted,
    Applied,
    ApplyFailed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CardState {
    #[default]
    Bound,
    Awaiting,
    Reserved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuestionResult {
    Current(Consent),
    RevisionChanged(Consent),
    AlreadyApplied(Consent),
}

impl QuestionResult {
    pub(crate) fn into_consent(self) -> Consent {
        match self {
            Self::Current(value) | Self::RevisionChanged(value) | Self::AlreadyApplied(value) => {
                value
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyResult {
    pub before: Option<[u8; 32]>,
    pub after: Option<[u8; 32]>,
    pub backup: Option<PathBuf>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Consent {
    schema: u32,
    pub key: ConsentKey,
    pub preview: Preview,
    pub owner: u64,
    pub bot: u64,
    pub provider: String,
    pub channel: u64,
    pub message: u64,
    #[serde(default)]
    pub card: CardState,
    pub nonce: String,
    pub expires: u64,
    pub state: State,
    pub decision: Option<(u64, u64)>,
    pub intents: Vec<IntentRef>,
    pub apply: Option<ApplyResult>,
    pub launch: Option<String>,
    #[serde(default)]
    pub history: Vec<Generation>,
    #[serde(default)]
    pub settlements: Vec<settlement::Settlement>,
    #[serde(default)]
    intent_previews: Vec<IntentPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Generation {
    pub nonce: String,
    pub preview: Preview,
    pub decision: Option<(u64, u64)>,
    pub apply: Option<ApplyResult>,
    pub launch: Option<String>,
}

/// Re-observe the logical path after an interrupted apply; absence is an observed state.
pub(super) fn observe_interrupted(consent: &Consent) -> Result<ApplyResult, String> {
    let hash = match std::fs::read(&consent.key.config) {
        Ok(bytes) => Some(Sha256::digest(bytes).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "{}: cannot re-observe interrupted apply: {error}",
                consent.key.config.display()
            ));
        }
    };
    Ok(ApplyResult {
        before: hash,
        after: hash,
        backup: None,
        detail: Some("apply interrupted; config state re-observed".into()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentPreview {
    store: String,
    id: String,
    preview: Preview,
}

pub(crate) struct Question {
    pub key: ConsentKey,
    pub preview: Preview,
    pub owner: u64,
    pub bot: u64,
    pub provider: String,
    pub channel: u64,
    pub message: u64,
    pub intent: IntentRef,
}

pub(crate) struct ConsentStore(Directory);
impl ConsentStore {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self(Directory(root))
    }

    fn decode(&self, bytes: &[u8], key: &ConsentKey) -> Result<Consent, String> {
        let name = durable::key(key)?;
        let decoded = (|| {
            let raw: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| format!("consent_snapshot_corrupt: {e}"))?;
            if raw
                .get("schema")
                .and_then(|v| v.as_u64())
                .is_some_and(|s| s > u64::from(SCHEMA))
            {
                return Err("consent_snapshot_newer: preserve this file and use a matching AgentDesk version".into());
            }
            let value: Consent = serde_json::from_value(raw)
                .map_err(|e| format!("consent_snapshot_corrupt: {e}"))?;
            if !(1..=SCHEMA).contains(&value.schema)
                || &value.key != key
                || value.nonce.is_empty()
                || value.owner == 0
            {
                return Err("consent_snapshot_corrupt: incompatible or mismatched consent".into());
            }
            if value.state == State::Applied
                && value
                    .apply
                    .as_ref()
                    .and_then(|result| result.after)
                    .is_none()
            {
                return Err("consent_snapshot_corrupt: Applied consent requires a successful durable apply receipt".into());
            }
            let mut identities = std::collections::BTreeSet::new();
            let mut settlements = std::collections::BTreeSet::new();
            if value
                .intents
                .iter()
                .any(|intent| !identities.insert((&intent.store, &intent.id)))
                || (!value.intent_previews.is_empty()
                    && (value.intent_previews.len() != value.intents.len()
                        || value.intents.iter().any(|intent| {
                            value
                                .intent_previews
                                .iter()
                                .filter(|held| held.store == intent.store && held.id == intent.id)
                                .count()
                                != 1
                        })))
                || value.settlements.iter().any(|settled| {
                    settled.key.nonce.is_empty()
                        || !settlements.insert((&settled.key.store, &settled.key.id))
                        || !value.intents.iter().any(|intent| {
                            settled.key.store == intent.store && settled.key.id == intent.id
                        })
                })
            {
                return Err("consent_snapshot_corrupt: incompatible custody metadata".into());
            }
            Ok(value)
        })();
        decoded.map_err(|e: String| self.0.failure(&name, e))
    }

    pub(crate) fn load(&self, key: &ConsentKey) -> Result<Option<Consent>, String> {
        let key = key
            .clone()
            .normalized()
            .map_err(|e| self.0.failure("", e))?;
        self.0
            .read_stable(&durable::key(&key)?)?
            .map(|bytes| self.decode(&bytes, &key))
            .transpose()
    }

    #[cfg(test)]
    pub(crate) fn hold(&self, question: Question, now: u64) -> Result<QuestionResult, String> {
        self.question(question, now, false, false)
    }

    #[cfg(test)]
    pub(crate) fn retry(&self, question: Question, now: u64) -> Result<Consent, String> {
        self.question(question, now, true, false)
            .map(QuestionResult::into_consent)
    }

    #[cfg(test)]
    pub(crate) fn hold_unbound(
        &self,
        question: Question,
        now: u64,
    ) -> Result<QuestionResult, String> {
        self.question(question, now, false, true)
    }

    pub(crate) fn retry_unbound(&self, question: Question, now: u64) -> Result<Consent, String> {
        self.question(question, now, true, true)
            .map(QuestionResult::into_consent)
    }

    fn question(
        &self,
        question: Question,
        now: u64,
        retry: bool,
        unbound: bool,
    ) -> Result<QuestionResult, String> {
        let Question {
            key,
            preview,
            owner,
            bot,
            provider,
            channel,
            message,
            intent,
        } = question;
        let key = key.normalized().map_err(|e| self.0.failure("", e))?;
        if owner == 0
            || bot == 0
            || channel == 0
            || (message == 0) != unbound
            || provider.is_empty()
            || intent.store.is_empty()
            || intent.id.is_empty()
        {
            return Err(self.0.failure(
                &durable::key(&key)?,
                "consent question requires owner, card and custody identities",
            ));
        }
        let question_preview = preview.clone();
        let mut revision_changed = false;
        let mut already_applied = false;
        let bytes = self.0.change(&durable::key(&key)?, |previous| {
            let mut decoded = previous
                .as_ref()
                .map(|bytes| self.decode(bytes, &key))
                .transpose()?;
            if let Some(current) = &mut decoded {
                current.schema = SCHEMA;
            }
            if retry && let Some(current) = &decoded {
                if current.state == State::Accepted {
                    return Err(
                        "apply in progress; preserve this decision and wait for its receipt".into(),
                    );
                }
            }
            let previous = decoded;
            let mut value = if let Some(mut current) = previous {
                if current.intent_previews.is_empty() {
                    current.intent_previews = current
                        .intents
                        .iter()
                        .map(|intent| IntentPreview {
                            store: intent.store.clone(),
                            id: intent.id.clone(),
                            preview: current.preview.clone(),
                        })
                        .collect();
                }
                if retry && current.state == State::Applied {
                    Self::append_intent(&mut current, &intent, &question_preview)?;
                    let changed = current.unresolved_intents().iter().any(|intent| {
                        current.intent_previews.iter().any(|held| {
                            held.store == intent.store
                                && held.id == intent.id
                                && held.preview != current.preview
                        })
                    });
                    if !current.releasable_intents().is_empty() || !changed {
                        already_applied = true;
                        return serde_json::to_vec(&current).map_err(|e| e.to_string());
                    }
                    current.history.push(Generation {
                        nonce: current.nonce.clone(),
                        preview: current.preview.clone(),
                        decision: current.decision,
                        apply: current.apply.clone(),
                        launch: current.launch.clone(),
                    });
                }
                if !retry {
                    revision_changed = current.preview.revision != preview.revision;
                    if current.state == State::Pending
                        && current.card != CardState::Awaiting
                        && now >= current.expires
                    {
                        current.state = State::Expired;
                    }
                    current
                } else {
                    let revision = current
                        .preview
                        .revision
                        .checked_add(1)
                        .ok_or("consent revision exhausted")?;
                    current.preview = preview;
                    current.preview.revision = current.preview.revision.max(revision);
                    current.owner = owner;
                    current.bot = bot;
                    current.provider = provider;
                    current.channel = channel;
                    current.message = message;
                    current.card = if unbound {
                        CardState::Awaiting
                    } else {
                        CardState::Bound
                    };
                    current.nonce = uuid::Uuid::new_v4().to_string();
                    current.expires = now.saturating_add(TTL);
                    current.state = State::Pending;
                    current.decision = None;
                    current.apply = None;
                    current.launch = None;
                    for intent in &mut current.intent_previews {
                        if !current.settlements.iter().any(|settled| {
                            settled.key.store == intent.store && settled.key.id == intent.id
                        }) {
                            intent.preview = current.preview.clone();
                        }
                    }
                    current
                }
            } else {
                Consent {
                    schema: SCHEMA,
                    key: key.clone(),
                    preview,
                    owner,
                    bot,
                    provider,
                    channel,
                    message,
                    card: if unbound {
                        CardState::Awaiting
                    } else {
                        CardState::Bound
                    },
                    nonce: uuid::Uuid::new_v4().to_string(),
                    expires: now.saturating_add(TTL),
                    state: State::Pending,
                    decision: None,
                    intents: Vec::new(),
                    apply: None,
                    launch: None,
                    history: Vec::new(),
                    settlements: Vec::new(),
                    intent_previews: Vec::new(),
                }
            };
            let intent_preview = if retry {
                value.preview.clone()
            } else {
                question_preview
            };
            Self::append_intent(&mut value, &intent, &intent_preview)?;
            serde_json::to_vec(&value).map_err(|e| e.to_string())
        })?;
        let value = self.decode(&bytes, &key)?;
        Ok(if already_applied {
            QuestionResult::AlreadyApplied(value)
        } else if revision_changed {
            QuestionResult::RevisionChanged(value)
        } else {
            QuestionResult::Current(value)
        })
    }

    fn append_intent(
        value: &mut Consent,
        intent: &IntentRef,
        preview: &Preview,
    ) -> Result<(), String> {
        if let Some(existing) = value
            .intents
            .iter()
            .find(|i| i.store == intent.store && i.id == intent.id)
        {
            if existing != intent {
                return Err("custody identity changed; preserve the original intent".into());
            }
        } else {
            value.intent_previews.push(IntentPreview {
                store: intent.store.clone(),
                id: intent.id.clone(),
                preview: preview.clone(),
            });
            value.intents.push(intent.clone());
        }
        Ok(())
    }

    pub(crate) fn unreserve_card(&self, expected: &Consent) -> Result<Consent, String> {
        self.update(expected, |current| {
            if expected.state != State::Pending
                || expected.card != CardState::Reserved
                || expected.message != 0
                || current.state != State::Pending
                || current.message != 0
                || current.card != CardState::Reserved
            {
                return Err("only an unsent reserved card can be unreserved".into());
            }
            current.card = CardState::Awaiting;
            Ok(())
        })
    }

    pub(in crate::services::session_host::herdr::policy::consent) fn interrupt_accepted(
        &self,
        expected: &Consent,
        mut observed: ApplyResult,
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            if expected.state != State::Accepted
                || current.state != State::Accepted
                || current.decision != expected.decision
            {
                return Err("interruption requires the current accepted decision".into());
            }
            observed.backup = None;
            observed.detail = Some("apply interrupted; config state re-observed".into());
            current.state = State::ApplyFailed;
            current.apply = Some(observed);
            Ok(())
        })
    }

    pub(crate) fn reserve_card(&self, expected: &Consent, now: u64) -> Result<Consent, String> {
        self.update(expected, |current| {
            if current.state != State::Pending
                || current.message != 0
                || current.card != CardState::Awaiting
            {
                return Err("card is already reserved or no longer pending".into());
            }
            current.card = CardState::Reserved;
            current.expires = now.saturating_add(TTL);
            Ok(())
        })
    }

    pub(crate) fn bind_card(&self, expected: &Consent, message: u64) -> Result<Consent, String> {
        self.update(expected, |current| {
            if message == 0
                || current.state != State::Pending
                || current.message != 0
                || current.card != CardState::Reserved
            {
                return Err("card binding requires its reserved unpublished decision".into());
            }
            current.message = message;
            current.card = CardState::Bound;
            Ok(())
        })
    }

    fn update(
        &self,
        expected: &Consent,
        apply: impl FnOnce(&mut Consent) -> Result<(), String>,
    ) -> Result<Consent, String> {
        let key = expected
            .key
            .clone()
            .normalized()
            .map_err(|e| self.0.failure("", e))?;
        let bytes = self.0.change(&durable::key(&key)?, |previous| {
            let mut current = self.decode(&previous.ok_or("consent missing")?, &key)?;
            if (
                &current.nonce,
                &current.preview,
                current.owner,
                current.bot,
                &current.provider,
                current.channel,
                current.message,
            ) != (
                &expected.nonce,
                &expected.preview,
                expected.owner,
                expected.bot,
                &expected.provider,
                expected.channel,
                expected.message,
            ) {
                return Err("stale consent decision".into());
            }
            apply(&mut current)?;
            current.schema = SCHEMA;
            serde_json::to_vec(&current).map_err(|e| e.to_string())
        })?;
        self.decode(&bytes, &key)
    }

    /// A current owner recheck can receipt an already-verified false config without a card.
    pub(crate) fn accept_recheck(
        &self,
        expected: &Consent,
        actor: u64,
        now: u64,
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            if actor != current.owner
                || actor == 0
                || expected.state != State::Pending
                || expected.card != CardState::Reserved
                || expected.message != 0
                || current.state != State::Pending
                || current.card != CardState::Reserved
                || current.message != 0
                || now >= current.expires
            {
                return Err(
                    "owner recheck requires its current unpublished reserved consent".into(),
                );
            }
            current.state = State::Accepted;
            current.decision = Some((actor, now));
            Ok(())
        })
    }

    pub(crate) fn decide(
        &self,
        expected: &Consent,
        actor: u64,
        yes: bool,
        now: u64,
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            if actor != current.owner
                || current.state != State::Pending
                || now >= current.expires
                || current.message == 0
                || current.card != CardState::Bound
            {
                return Err("decision is not an active owner decision".into());
            }
            current.state = if yes { State::Accepted } else { State::Denied };
            current.decision = Some((actor, now));
            Ok(())
        })
    }

    pub(crate) fn expire(&self, expected: &Consent, now: u64) -> Result<Consent, String> {
        self.update(expected, |current| {
            if current.state == State::Pending
                && current.card != CardState::Awaiting
                && now >= current.expires
            {
                current.state = State::Expired;
            }
            Ok(())
        })
    }

    pub(crate) fn record_apply(
        &self,
        expected: &Consent,
        result: ApplyResult,
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            if current.state != State::Accepted {
                return Err("apply requires an accepted consent".into());
            }
            current.state = if result.after.is_some() {
                State::Applied
            } else {
                State::ApplyFailed
            };
            current.apply = Some(result);
            Ok(())
        })
    }
}

pub(crate) mod settlement;
pub(crate) mod workflow;

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;

pub(crate) mod apply_owner;
pub(crate) mod boot_recovery;
pub(crate) mod card;
pub(crate) mod engine;
pub(crate) mod preview;
