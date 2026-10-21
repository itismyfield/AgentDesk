//! Cards own references to existing custody, never copies of prompts or uploads.

use super::{
    super::contract::ServerWitness,
    durable::{self, Directory},
};
use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

const TTL: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsentKey {
    pub node: String,
    pub user: u32,
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
    pub nonce: String,
    pub expires: u64,
    pub state: State,
    pub decision: Option<(u64, u64)>,
    pub intents: Vec<IntentRef>,
    pub apply: Option<ApplyResult>,
    pub launch: Option<String>,
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
                .is_some_and(|s| s > 1)
            {
                return Err("consent_snapshot_newer: preserve this file and use a matching AgentDesk version".into());
            }
            let value: Consent = serde_json::from_value(raw)
                .map_err(|e| format!("consent_snapshot_corrupt: {e}"))?;
            if value.schema != 1 || &value.key != key || value.nonce.is_empty() || value.owner == 0
            {
                return Err("consent_snapshot_corrupt: incompatible or mismatched consent".into());
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
            .read(&durable::key(&key)?)?
            .map(|bytes| self.decode(&bytes, &key))
            .transpose()
    }

    pub(crate) fn hold(&self, question: Question, now: u64) -> Result<Consent, String> {
        self.question(question, now, false)
    }

    pub(crate) fn retry(&self, question: Question, now: u64) -> Result<Consent, String> {
        self.question(question, now, true)
    }

    fn question(&self, question: Question, now: u64, retry: bool) -> Result<Consent, String> {
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
            || message == 0
            || provider.is_empty()
            || intent.store.is_empty()
            || intent.id.is_empty()
        {
            return Err(self.0.failure(
                &durable::key(&key)?,
                "consent question requires owner, card and custody identities",
            ));
        }
        let bytes = self.0.change(&durable::key(&key)?, |previous| {
            let previous = previous
                .map(|bytes| self.decode(&bytes, &key))
                .transpose()?;
            let mut value = if let Some(mut current) = previous {
                if !retry {
                    if current.preview.revision != preview.revision {
                        return Err("policy revision changed; explicit retry required".into());
                    }
                    if current.state == State::Pending && now >= current.expires {
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
                    current.nonce = uuid::Uuid::new_v4().to_string();
                    current.expires = now.saturating_add(TTL);
                    current.state = State::Pending;
                    current.decision = None;
                    current.apply = None;
                    current.launch = None;
                    current
                }
            } else {
                Consent {
                    schema: 1,
                    key: key.clone(),
                    preview,
                    owner,
                    bot,
                    provider,
                    channel,
                    message,
                    nonce: uuid::Uuid::new_v4().to_string(),
                    expires: now.saturating_add(TTL),
                    state: State::Pending,
                    decision: None,
                    intents: Vec::new(),
                    apply: None,
                    launch: None,
                }
            };
            if let Some(existing) = value
                .intents
                .iter()
                .find(|i| i.store == intent.store && i.id == intent.id)
            {
                if existing != &intent {
                    return Err("custody identity changed; preserve the original intent".into());
                }
            } else {
                value.intents.push(intent);
            }
            serde_json::to_vec(&value).map_err(|e| e.to_string())
        })?;
        self.decode(&bytes, &key)
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
            serde_json::to_vec(&current).map_err(|e| e.to_string())
        })?;
        self.decode(&bytes, &key)
    }

    pub(crate) fn decide(
        &self,
        expected: &Consent,
        actor: u64,
        yes: bool,
        now: u64,
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            if actor != current.owner || current.state != State::Pending || now >= current.expires {
                return Err("decision is not an active owner decision".into());
            }
            current.state = if yes { State::Accepted } else { State::Denied };
            current.decision = Some((actor, now));
            Ok(())
        })
    }

    pub(crate) fn expire(&self, expected: &Consent, now: u64) -> Result<Consent, String> {
        self.update(expected, |current| {
            if current.state == State::Pending && now >= current.expires {
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

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;
