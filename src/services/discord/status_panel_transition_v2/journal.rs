//! Dormant channel journal adapter.
//!
//! The files provide crash-consistent local durability, not a transaction with
//! Discord. A successful send can still be orphaned before `record_sent`, and
//! recovery cannot prove every such orphan. The journal also does not provide
//! cryptographic protection from an attacker who can modify the runtime tree.
//! Cross-process correctness comes from `channel.lock`; every read-modify-write
//! holds that same kernel flock.

mod codec;
mod storage;

use std::path::Path;

use codec::{
    ChannelWire, OperationStamp, ReplayMetadata, bind_digest, commit_bind_digest, delete_digest,
    prepare_digest, retire_digest,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use storage::WriteTarget;
use storage::{ChannelLock, ChannelStorage, Directory, Failpoint};
use uuid::Uuid;

use super::{BoundPanel, Candidate, JournalState, PanelIdentity, PanelPlan};

pub(super) use storage::WriteStage;

const MAX_OPERATION_RECORDS: usize = 64;
const MAX_QUARANTINE_RECORDS: usize = 16;
const CHANNEL_FILE: &str = "channel.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CanonicalProvider(String);

impl CanonicalProvider {
    fn parse(value: &str) -> Result<Self, StoreError> {
        matches!(value, "claude" | "codex" | "gemini" | "opencode" | "qwen")
            .then(|| Self(value.to_owned()))
            .ok_or(StoreError::InvalidPathComponent("provider"))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CanonicalTokenHash(String);

impl CanonicalTokenHash {
    fn parse(value: &str) -> Result<Self, StoreError> {
        value
            .strip_prefix("discord_")
            .filter(|suffix| {
                suffix.len() == 16
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .map(|_| Self(value.to_owned()))
            .ok_or(StoreError::InvalidPathComponent("canonical_token_hash"))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StoreError {
    InvalidPathComponent(&'static str),
    SymlinkRejected,
    UnexpectedFileType,
    LockFailed,
    ReadFailed,
    MalformedRecord,
    Quarantined,
    InvariantViolation,
    WriteFailed(WriteStage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReadOutcome<T> {
    Present(T),
    Missing,
    DurabilityFailure(StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mutation<T> {
    Applied(T),
    Replayed(T),
    Stale,
    DurabilityFailure(StoreError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DeleteObservation {
    Deleted,
    NotFound404,
    UnknownMessage10008,
    Forbidden403,
    Transient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeleteDisposition {
    Retired,
    RetainAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedOperation {
    operation_id: String,
    owner_nonce: String,
    digest: String,
    expected_revision: u64,
    plan: PanelPlan,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct JournalOwnedBind {
    operation_id: String,
    owner_nonce: String,
    digest: String,
    revision: u64,
    candidate: Candidate,
    _seal: AdapterSeal,
}

impl JournalOwnedBind {
    pub(super) fn candidate(&self) -> &Candidate {
        &self.candidate
    }

    pub(super) fn matches_candidate(&self, candidate: &Candidate) -> bool {
        self.candidate == *candidate
            && self.digest == bind_digest(&self.operation_id, &self.owner_nonce, candidate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoundOperation {
    operation_id: String,
    owner_nonce: String,
    bind_digest: String,
    revision: u64,
    panel: BoundPanel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct JournalOwnedRetire {
    operation_id: String,
    owner_nonce: String,
    digest: String,
    revision: u64,
    panel: BoundPanel,
    delete_message_id: u64,
    _seal: AdapterSeal,
}

impl JournalOwnedRetire {
    pub(super) fn delete_message_id(&self) -> u64 {
        self.delete_message_id
    }

    pub(super) fn matches_panel(&self, panel: &BoundPanel) -> bool {
        self.panel == *panel
            && self.delete_message_id != 0
            && self.delete_message_id != panel.candidate.message_id
            && panel.candidate.plan.expected_prior_message_id == Some(self.delete_message_id)
            && self.digest
                == retire_digest(
                    &self.operation_id,
                    &self.owner_nonce,
                    panel,
                    self.delete_message_id,
                )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdapterSeal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChannelSnapshot {
    pub revision: u64,
    pub channel_generation: u64,
    pub current_singleton_message_id: Option<u64>,
    pub state: JournalState,
    last_operation: OperationStamp,
    replay: ReplayMetadata,
}

pub(super) struct ChannelJournal {
    storage: ChannelStorage,
    identity: PanelIdentity,
    #[cfg(test)]
    failpoint: Option<Failpoint>,
}

impl ChannelJournal {
    pub(super) fn open(
        root: &Path,
        provider: &str,
        canonical_token_hash: &str,
        channel_id: u64,
    ) -> Result<Self, StoreError> {
        let provider = CanonicalProvider::parse(provider)?;
        let canonical_token_hash = CanonicalTokenHash::parse(canonical_token_hash)?;
        if channel_id == 0 {
            return Err(StoreError::InvalidPathComponent("channel_id"));
        }
        let storage = ChannelStorage::open(
            root,
            provider.as_str(),
            canonical_token_hash.as_str(),
            channel_id,
        )?;
        Ok(Self {
            storage,
            identity: PanelIdentity {
                provider,
                canonical_token_hash,
                channel_id,
            },
            #[cfg(test)]
            failpoint: None,
        })
    }

    pub(super) fn identity(&self) -> &PanelIdentity {
        &self.identity
    }

    pub(super) fn load(&self) -> ReadOutcome<ChannelSnapshot> {
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return ReadOutcome::DurabilityFailure(error),
        };
        match self.load_optional_locked() {
            Ok(Some(snapshot)) => ReadOutcome::Present(snapshot),
            Ok(None) => ReadOutcome::Missing,
            Err(error) => ReadOutcome::DurabilityFailure(error),
        }
    }

    pub(super) fn prepare(
        &self,
        plan: PanelPlan,
        expected_revision: u64,
    ) -> Mutation<PreparedOperation> {
        if !plan.is_valid() || plan.identity != self.identity {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        let current = match self.load_optional_locked() {
            Ok(current) => current,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        if current.as_ref().map_or(0, |state| state.revision) != expected_revision {
            return Mutation::Stale;
        }
        let generation = match current
            .as_ref()
            .map_or(Some(1), |state| state.channel_generation.checked_add(1))
        {
            Some(generation) => generation,
            None => return Mutation::DurabilityFailure(StoreError::InvariantViolation),
        };
        let revision = match expected_revision.checked_add(1) {
            Some(revision) => revision,
            None => return Mutation::DurabilityFailure(StoreError::InvariantViolation),
        };
        let operation_id = Uuid::new_v4().simple().to_string();
        let owner_nonce = Uuid::new_v4().simple().to_string();
        let digest = prepare_digest(&operation_id, &owner_nonce, &plan, generation);
        let operation = PreparedOperation {
            operation_id,
            owner_nonce,
            digest,
            expected_revision,
            plan: plan.clone(),
            generation,
        };
        let stamp = operation.stamp(revision);
        let snapshot = ChannelSnapshot {
            revision,
            channel_generation: generation,
            current_singleton_message_id: current
                .and_then(|state| state.current_singleton_message_id),
            state: JournalState::prepared(plan, generation),
            last_operation: stamp.clone(),
            replay: ReplayMetadata::Prepared,
        };
        self.persist_locked(&snapshot)
            .map_or_else(Mutation::DurabilityFailure, |_| {
                Mutation::Applied(operation)
            })
    }

    pub(super) fn record_sent(
        &self,
        operation: &PreparedOperation,
        message_id: u64,
    ) -> Mutation<JournalOwnedBind> {
        if message_id == 0 || operation.plan.identity != self.identity {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        let candidate = Candidate {
            plan: operation.plan.clone(),
            message_id,
            generation: operation.generation,
        };
        if !candidate.is_valid() {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        let Some(expected_revision) = operation.expected_revision.checked_add(1) else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let Some(revision) = expected_revision.checked_add(1) else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let digest = bind_digest(&operation.operation_id, &operation.owner_nonce, &candidate);
        let authorization = JournalOwnedBind {
            operation_id: operation.operation_id.clone(),
            owner_nonce: operation.owner_nonce.clone(),
            digest: digest.clone(),
            revision,
            candidate: candidate.clone(),
            _seal: AdapterSeal,
        };
        let stamp = OperationStamp::new(
            &operation.operation_id,
            &operation.owner_nonce,
            &digest,
            revision,
        );
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        let mut snapshot = match self.require_locked() {
            Ok(snapshot) => snapshot,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        if snapshot.revision == revision {
            return if snapshot.last_operation == stamp
                && snapshot.replay == ReplayMetadata::BindAuthorized
                && matches!(&snapshot.state, JournalState::BindAuthorized { candidate: stored, authorization: proof } if stored == &candidate && proof == &authorization)
            {
                Mutation::Replayed(authorization)
            } else {
                Mutation::Stale
            };
        }
        if snapshot.revision != expected_revision
            || snapshot.last_operation != operation.stamp(expected_revision)
            || !matches!(&snapshot.state, JournalState::Prepared { plan, generation } if plan == &operation.plan && *generation == operation.generation)
        {
            return Mutation::Stale;
        }
        snapshot.revision = revision;
        snapshot.last_operation = stamp;
        snapshot.replay = ReplayMetadata::BindAuthorized;
        snapshot.state = JournalState::BindAuthorized {
            candidate,
            authorization: authorization.clone(),
        };
        self.persist_locked(&snapshot)
            .map_or_else(Mutation::DurabilityFailure, |_| {
                Mutation::Applied(authorization)
            })
    }

    pub(super) fn commit_bind(&self, authorization: &JournalOwnedBind) -> Mutation<BoundOperation> {
        if !authorization.matches_candidate(&authorization.candidate) {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        let Some(revision) = authorization.revision.checked_add(1) else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let digest = commit_bind_digest(authorization);
        let stamp = OperationStamp::new(
            &authorization.operation_id,
            &authorization.owner_nonce,
            &digest,
            revision,
        );
        let panel = BoundPanel {
            candidate: authorization.candidate.clone(),
        };
        let bound = BoundOperation {
            operation_id: authorization.operation_id.clone(),
            owner_nonce: authorization.owner_nonce.clone(),
            bind_digest: authorization.digest.clone(),
            revision,
            panel: panel.clone(),
        };
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        let mut snapshot = match self.require_locked() {
            Ok(snapshot) => snapshot,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        if snapshot.revision == revision {
            return if snapshot.last_operation == stamp
                && snapshot.replay
                    == (ReplayMetadata::Bound {
                        bind_digest: authorization.digest.clone(),
                    })
                && snapshot.state == (JournalState::Bound { panel })
            {
                Mutation::Replayed(bound)
            } else {
                Mutation::Stale
            };
        }
        if snapshot.revision != authorization.revision
            || !matches!(&snapshot.state, JournalState::BindAuthorized { candidate, authorization: stored } if candidate == &authorization.candidate && stored == authorization)
        {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        snapshot.revision = revision;
        snapshot.current_singleton_message_id = Some(panel.candidate.message_id);
        snapshot.last_operation = stamp;
        snapshot.replay = ReplayMetadata::Bound {
            bind_digest: authorization.digest.clone(),
        };
        snapshot.state = JournalState::Bound { panel };
        self.persist_locked(&snapshot)
            .map_or_else(Mutation::DurabilityFailure, |_| Mutation::Applied(bound))
    }

    pub(super) fn authorize_retire(&self, bound: &BoundOperation) -> Mutation<JournalOwnedRetire> {
        let Some(delete_message_id) = bound.panel.candidate.plan.expected_prior_message_id else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let Some(revision) = bound.revision.checked_add(1) else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let digest = retire_digest(
            &bound.operation_id,
            &bound.owner_nonce,
            &bound.panel,
            delete_message_id,
        );
        let authorization = JournalOwnedRetire {
            operation_id: bound.operation_id.clone(),
            owner_nonce: bound.owner_nonce.clone(),
            digest: digest.clone(),
            revision,
            panel: bound.panel.clone(),
            delete_message_id,
            _seal: AdapterSeal,
        };
        if !authorization.matches_panel(&bound.panel) {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        let stamp = OperationStamp::new(&bound.operation_id, &bound.owner_nonce, &digest, revision);
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        let mut snapshot = match self.require_locked() {
            Ok(snapshot) => snapshot,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        if snapshot.revision == revision {
            return if snapshot.last_operation == stamp
                && snapshot.replay == ReplayMetadata::RetireAuthorized
                && matches!(&snapshot.state, JournalState::RetireAuthorized { panel, authorization: stored } if panel == &bound.panel && stored == &authorization)
            {
                Mutation::Replayed(authorization)
            } else {
                Mutation::Stale
            };
        }
        if snapshot.revision != bound.revision
            || snapshot.replay
                != (ReplayMetadata::Bound {
                    bind_digest: bound.bind_digest.clone(),
                })
            || snapshot.state
                != (JournalState::Bound {
                    panel: bound.panel.clone(),
                })
        {
            return Mutation::Stale;
        }
        snapshot.revision = revision;
        snapshot.last_operation = stamp;
        snapshot.replay = ReplayMetadata::RetireAuthorized;
        snapshot.state = JournalState::RetireAuthorized {
            panel: bound.panel.clone(),
            authorization: authorization.clone(),
        };
        self.persist_locked(&snapshot)
            .map_or_else(Mutation::DurabilityFailure, |_| {
                Mutation::Applied(authorization)
            })
    }

    pub(super) fn record_delete_observation(
        &self,
        authorization: &JournalOwnedRetire,
        observation: DeleteObservation,
    ) -> Mutation<DeleteDisposition> {
        if !authorization.matches_panel(&authorization.panel) {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        if matches!(
            observation,
            DeleteObservation::Forbidden403 | DeleteObservation::Transient
        ) {
            return match self.load() {
                ReadOutcome::Present(snapshot)
                    if matches!(
                        snapshot.state,
                        JournalState::RetireAuthorized {
                            authorization: ref stored,
                            ..
                        } if stored == authorization
                    ) =>
                {
                    Mutation::Replayed(DeleteDisposition::RetainAuthorization)
                }
                ReadOutcome::Present(_) | ReadOutcome::Missing => Mutation::Stale,
                ReadOutcome::DurabilityFailure(error) => Mutation::DurabilityFailure(error),
            };
        }
        let Some(revision) = authorization.revision.checked_add(1) else {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        };
        let digest = delete_digest(authorization, observation);
        let stamp = OperationStamp::new(
            &authorization.operation_id,
            &authorization.owner_nonce,
            &digest,
            revision,
        );
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        let mut snapshot = match self.require_locked() {
            Ok(snapshot) => snapshot,
            Err(error) => return Mutation::DurabilityFailure(error),
        };
        if snapshot.revision == revision {
            return if snapshot.last_operation == stamp
                && snapshot.replay
                    == (ReplayMetadata::Retired {
                        retire_digest: authorization.digest.clone(),
                        observation,
                    })
            {
                Mutation::Replayed(DeleteDisposition::Retired)
            } else {
                Mutation::Stale
            };
        }
        if snapshot.revision != authorization.revision
            || !matches!(&snapshot.state, JournalState::RetireAuthorized { panel, authorization: stored } if panel == &authorization.panel && stored == authorization)
        {
            return Mutation::DurabilityFailure(StoreError::InvariantViolation);
        }
        snapshot.revision = revision;
        snapshot.last_operation = stamp;
        snapshot.replay = ReplayMetadata::Retired {
            retire_digest: authorization.digest.clone(),
            observation,
        };
        snapshot.state = JournalState::Retired {
            panel: authorization.panel.clone(),
            retired_message_id: authorization.delete_message_id,
        };
        self.persist_locked(&snapshot)
            .map_or_else(Mutation::DurabilityFailure, |_| {
                Mutation::Applied(DeleteDisposition::Retired)
            })
    }

    pub(super) fn recover_bind_authorization(&self) -> ReadOutcome<JournalOwnedBind> {
        match self.load() {
            ReadOutcome::Present(snapshot) => match snapshot.state {
                JournalState::BindAuthorized {
                    candidate,
                    authorization,
                } if authorization.matches_candidate(&candidate) => {
                    ReadOutcome::Present(authorization)
                }
                _ => ReadOutcome::Missing,
            },
            ReadOutcome::Missing => ReadOutcome::Missing,
            ReadOutcome::DurabilityFailure(error) => ReadOutcome::DurabilityFailure(error),
        }
    }

    fn lock(&self) -> Result<ChannelLock, StoreError> {
        self.storage.lock()
    }

    fn require_locked(&self) -> Result<ChannelSnapshot, StoreError> {
        self.load_optional_locked()?
            .ok_or(StoreError::InvariantViolation)
    }

    fn load_optional_locked(&self) -> Result<Option<ChannelSnapshot>, StoreError> {
        if self.storage.quarantine_marker_present()? {
            return Err(StoreError::Quarantined);
        }
        let bytes = match self.storage.read_channel()? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let wire: ChannelWire = match serde_json::from_slice(&bytes) {
            Ok(wire) => wire,
            Err(_) => return Err(self.quarantine_locked(&bytes, StoreError::MalformedRecord)),
        };
        let operation_name = Self::operation_name(&wire.last_operation);
        let operation_bytes = match self.storage.read_operation(&operation_name)? {
            Some(bytes) => bytes,
            None => {
                return Err(self.quarantine_locked(&bytes, StoreError::InvariantViolation));
            }
        };
        if operation_bytes != bytes {
            return Err(self.quarantine_locked(&bytes, StoreError::InvariantViolation));
        }
        wire.into_snapshot(&self.identity)
            .map(Some)
            .map_err(|error| self.quarantine_locked(&bytes, error))
    }

    fn quarantine_locked(&self, bytes: &[u8], source: StoreError) -> StoreError {
        let high_watermark = self.operation_high_watermark_locked();
        match self.storage.quarantine_channel(bytes, high_watermark) {
            Ok(()) => StoreError::Quarantined,
            Err(_) => source,
        }
    }

    fn operation_high_watermark_locked(&self) -> Option<(u64, u64)> {
        let records = self.storage.read_operation_records().ok()?;
        let mut high_watermark: Option<(u64, u64)> = None;
        for (_, bytes) in records {
            let Ok(wire) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(revision) = wire.get("revision").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let Some(generation) = wire
                .get("channel_generation")
                .and_then(serde_json::Value::as_u64)
            else {
                continue;
            };
            high_watermark = Some(high_watermark.map_or((revision, generation), |current| {
                current.max((revision, generation))
            }));
        }
        high_watermark
    }

    fn persist_locked(&self, snapshot: &ChannelSnapshot) -> Result<(), StoreError> {
        if self.storage.quarantine_marker_present()? {
            return Err(StoreError::Quarantined);
        }
        let wire = ChannelWire::from_snapshot(&self.identity, snapshot);
        let body = serde_json::to_vec(&wire).map_err(|_| StoreError::InvariantViolation)?;
        self.storage.write_operation(
            &Self::operation_name(&snapshot.last_operation),
            &body,
            self.failpoint(),
        )?;
        self.storage.write_channel(&body, self.failpoint())?;
        self.gc_locked()
    }

    fn operation_name(stamp: &OperationStamp) -> String {
        format!("{:020}-{}.json", stamp.revision, stamp.operation_id)
    }

    fn gc_locked(&self) -> Result<(), StoreError> {
        self.storage
            .prune(Directory::Operations, MAX_OPERATION_RECORDS)?;
        self.storage
            .prune(Directory::Quarantine, MAX_QUARANTINE_RECORDS)
    }

    #[cfg(test)]
    fn with_failpoint(mut self, target: WriteTarget, stage: WriteStage) -> Self {
        self.failpoint = Some(Failpoint { target, stage });
        self
    }

    fn failpoint(&self) -> Option<Failpoint> {
        #[cfg(test)]
        {
            self.failpoint
        }
        #[cfg(not(test))]
        {
            None
        }
    }
}

impl PreparedOperation {
    fn stamp(&self, revision: u64) -> OperationStamp {
        OperationStamp::new(
            &self.operation_id,
            &self.owner_nonce,
            &self.digest,
            revision,
        )
    }
}

#[cfg(test)]
pub(super) fn identity_for_test(provider: &str, hash: &str, channel_id: u64) -> PanelIdentity {
    PanelIdentity {
        provider: CanonicalProvider::parse(provider).unwrap(),
        canonical_token_hash: CanonicalTokenHash::parse(hash).unwrap(),
        channel_id,
    }
}

#[cfg(test)]
pub(super) fn bind_authorization_for_test(candidate: Candidate) -> JournalOwnedBind {
    let operation_id = "00000000000000000000000000000001".to_string();
    let owner_nonce = "00000000000000000000000000000002".to_string();
    JournalOwnedBind {
        digest: bind_digest(&operation_id, &owner_nonce, &candidate),
        operation_id,
        owner_nonce,
        revision: 2,
        candidate,
        _seal: AdapterSeal,
    }
}

#[cfg(test)]
pub(super) fn mutate_bind_authorization_for_test(
    mut authorization: JournalOwnedBind,
    candidate: Candidate,
) -> JournalOwnedBind {
    authorization.candidate = candidate;
    authorization
}

#[cfg(test)]
pub(super) fn retire_authorization_for_test(
    panel: BoundPanel,
    delete_message_id: u64,
) -> JournalOwnedRetire {
    let operation_id = "00000000000000000000000000000001".to_string();
    let owner_nonce = "00000000000000000000000000000002".to_string();
    JournalOwnedRetire {
        digest: retire_digest(&operation_id, &owner_nonce, &panel, delete_message_id),
        operation_id,
        owner_nonce,
        revision: 4,
        panel,
        delete_message_id,
        _seal: AdapterSeal,
    }
}

#[cfg(test)]
mod tests;
