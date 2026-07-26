use serde::{Deserialize, Serialize};

use super::{
    AdapterSeal, ChannelSnapshot, DeleteObservation, JournalOwnedBind, JournalOwnedRetire,
    StoreError,
};
use crate::services::discord::status_panel_transition_v2::{
    BoundPanel, Candidate, JournalState, PanelIdentity, PanelPlan, QuarantineReason,
};

const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct OperationStamp {
    pub operation_id: String,
    pub owner_nonce: String,
    pub digest: String,
    pub revision: u64,
}

impl OperationStamp {
    pub(super) fn new(operation_id: &str, owner_nonce: &str, digest: &str, revision: u64) -> Self {
        Self {
            operation_id: operation_id.to_owned(),
            owner_nonce: owner_nonce.to_owned(),
            digest: digest.to_owned(),
            revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ReplayMetadata {
    Prepared,
    BindAuthorized,
    Bound {
        bind_digest: String,
    },
    RetireAuthorized,
    Retired {
        retire_digest: String,
        observation: DeleteObservation,
    },
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ChannelWire {
    schema_version: u32,
    provider: String,
    canonical_token_hash: String,
    channel_id: u64,
    revision: u64,
    channel_generation: u64,
    current_singleton_message_id: Option<u64>,
    pub last_operation: OperationStamp,
    replay: ReplayMetadata,
    state: StateWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StateWire {
    Prepared {
        turn_id: u64,
        expected_prior_message_id: Option<u64>,
        generation: u64,
    },
    BindAuthorized {
        turn_id: u64,
        expected_prior_message_id: Option<u64>,
        message_id: u64,
        generation: u64,
    },
    Bound {
        turn_id: u64,
        expected_prior_message_id: Option<u64>,
        message_id: u64,
        generation: u64,
    },
    RetireAuthorized {
        turn_id: u64,
        expected_prior_message_id: u64,
        message_id: u64,
        generation: u64,
    },
    Retired {
        turn_id: u64,
        expected_prior_message_id: u64,
        message_id: u64,
        generation: u64,
    },
    Quarantined,
}

impl ChannelWire {
    pub(super) fn from_snapshot(identity: &PanelIdentity, snapshot: &ChannelSnapshot) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            provider: identity.provider().to_owned(),
            canonical_token_hash: identity.canonical_token_hash().to_owned(),
            channel_id: identity.channel_id(),
            revision: snapshot.revision,
            channel_generation: snapshot.channel_generation,
            current_singleton_message_id: snapshot.current_singleton_message_id,
            last_operation: snapshot.last_operation.clone(),
            replay: snapshot.replay.clone(),
            state: StateWire::from_state(&snapshot.state),
        }
    }

    pub(super) fn into_snapshot(
        self,
        expected: &PanelIdentity,
    ) -> Result<ChannelSnapshot, StoreError> {
        if self.schema_version != SCHEMA_VERSION
            || self.provider != expected.provider()
            || self.canonical_token_hash != expected.canonical_token_hash()
            || self.channel_id != expected.channel_id()
            || self.revision == 0
            || self.channel_generation == 0
            || self.last_operation.revision != self.revision
            || !valid_id(&self.last_operation.operation_id)
            || !valid_id(&self.last_operation.owner_nonce)
            || !valid_digest(&self.last_operation.digest)
        {
            return Err(StoreError::InvariantViolation);
        }
        let state = self
            .state
            .into_state(expected, &self.last_operation, &self.replay)?;
        validate_replay(&state, &self.replay, &self.last_operation)?;
        validate_snapshot_invariants(
            &state,
            self.channel_generation,
            self.current_singleton_message_id,
        )?;
        Ok(ChannelSnapshot {
            revision: self.revision,
            channel_generation: self.channel_generation,
            current_singleton_message_id: self.current_singleton_message_id,
            state,
            last_operation: self.last_operation,
            replay: self.replay,
        })
    }
}

impl StateWire {
    fn from_state(state: &JournalState) -> Self {
        let fields = |candidate: &Candidate| {
            (
                candidate.plan.turn_id,
                candidate.plan.expected_prior_message_id,
                candidate.message_id,
                candidate.generation,
            )
        };
        match state {
            JournalState::Prepared { plan, generation } => Self::Prepared {
                turn_id: plan.turn_id,
                expected_prior_message_id: plan.expected_prior_message_id,
                generation: *generation,
            },
            JournalState::BindAuthorized { candidate, .. } => {
                let (turn_id, expected_prior_message_id, message_id, generation) =
                    fields(candidate);
                Self::BindAuthorized {
                    turn_id,
                    expected_prior_message_id,
                    message_id,
                    generation,
                }
            }
            JournalState::Bound { panel } => {
                let (turn_id, expected_prior_message_id, message_id, generation) =
                    fields(&panel.candidate);
                Self::Bound {
                    turn_id,
                    expected_prior_message_id,
                    message_id,
                    generation,
                }
            }
            JournalState::RetireAuthorized { panel, .. } => {
                let (turn_id, expected_prior_message_id, message_id, generation) =
                    fields(&panel.candidate);
                Self::RetireAuthorized {
                    turn_id,
                    expected_prior_message_id: expected_prior_message_id.unwrap_or(0),
                    message_id,
                    generation,
                }
            }
            JournalState::Retired { panel, .. } => {
                let (turn_id, expected_prior_message_id, message_id, generation) =
                    fields(&panel.candidate);
                Self::Retired {
                    turn_id,
                    expected_prior_message_id: expected_prior_message_id.unwrap_or(0),
                    message_id,
                    generation,
                }
            }
            _ => Self::Quarantined,
        }
    }

    fn into_state(
        self,
        identity: &PanelIdentity,
        stamp: &OperationStamp,
        replay: &ReplayMetadata,
    ) -> Result<JournalState, StoreError> {
        let plan = |turn_id, prior| PanelPlan {
            identity: identity.clone(),
            turn_id,
            expected_prior_message_id: prior,
        };
        let candidate = |turn_id, prior, message_id, generation| Candidate {
            plan: plan(turn_id, prior),
            message_id,
            generation,
        };
        match self {
            Self::Prepared {
                turn_id,
                expected_prior_message_id,
                generation,
            } => Ok(JournalState::Prepared {
                plan: plan(turn_id, expected_prior_message_id),
                generation,
            }),
            Self::BindAuthorized {
                turn_id,
                expected_prior_message_id,
                message_id,
                generation,
            } => {
                let candidate =
                    candidate(turn_id, expected_prior_message_id, message_id, generation);
                let authorization = JournalOwnedBind {
                    operation_id: stamp.operation_id.clone(),
                    owner_nonce: stamp.owner_nonce.clone(),
                    digest: stamp.digest.clone(),
                    revision: stamp.revision,
                    candidate: candidate.clone(),
                    _seal: AdapterSeal,
                };
                authorization
                    .matches_candidate(&candidate)
                    .then_some(JournalState::BindAuthorized {
                        candidate,
                        authorization,
                    })
                    .ok_or(StoreError::InvariantViolation)
            }
            Self::Bound {
                turn_id,
                expected_prior_message_id,
                message_id,
                generation,
            } => Ok(JournalState::Bound {
                panel: BoundPanel {
                    candidate: candidate(
                        turn_id,
                        expected_prior_message_id,
                        message_id,
                        generation,
                    ),
                },
            }),
            Self::RetireAuthorized {
                turn_id,
                expected_prior_message_id,
                message_id,
                generation,
            } => {
                let panel = BoundPanel {
                    candidate: candidate(
                        turn_id,
                        Some(expected_prior_message_id),
                        message_id,
                        generation,
                    ),
                };
                let authorization = JournalOwnedRetire {
                    operation_id: stamp.operation_id.clone(),
                    owner_nonce: stamp.owner_nonce.clone(),
                    digest: stamp.digest.clone(),
                    revision: stamp.revision,
                    panel: panel.clone(),
                    delete_message_id: expected_prior_message_id,
                    _seal: AdapterSeal,
                };
                authorization
                    .matches_panel(&panel)
                    .then_some(JournalState::RetireAuthorized {
                        panel,
                        authorization,
                    })
                    .ok_or(StoreError::InvariantViolation)
            }
            Self::Retired {
                turn_id,
                expected_prior_message_id,
                message_id,
                generation,
            } => Ok(JournalState::Retired {
                panel: BoundPanel {
                    candidate: candidate(
                        turn_id,
                        Some(expected_prior_message_id),
                        message_id,
                        generation,
                    ),
                },
                retired_message_id: expected_prior_message_id,
            }),
            Self::Quarantined if *replay == ReplayMetadata::Quarantined => {
                Ok(JournalState::Quarantined {
                    reason: QuarantineReason::UnknownState,
                })
            }
            Self::Quarantined => Err(StoreError::InvariantViolation),
        }
    }
}

fn validate_snapshot_invariants(
    state: &JournalState,
    channel_generation: u64,
    current_singleton_message_id: Option<u64>,
) -> Result<(), StoreError> {
    let candidate = match state {
        JournalState::Prepared { plan, generation } => {
            if !plan.is_valid() || *generation != channel_generation {
                return Err(StoreError::InvariantViolation);
            }
            return Ok(());
        }
        JournalState::BindAuthorized { candidate, .. } => candidate,
        JournalState::Bound { panel }
        | JournalState::RetireAuthorized { panel, .. }
        | JournalState::Retired { panel, .. } => &panel.candidate,
        JournalState::Failed { .. } | JournalState::Quarantined { .. } => return Ok(()),
    };
    if !candidate.is_valid() || candidate.generation != channel_generation {
        return Err(StoreError::InvariantViolation);
    }
    if matches!(
        state,
        JournalState::Bound { .. }
            | JournalState::RetireAuthorized { .. }
            | JournalState::Retired { .. }
    ) && current_singleton_message_id != Some(candidate.message_id)
    {
        return Err(StoreError::InvariantViolation);
    }
    Ok(())
}

fn validate_replay(
    state: &JournalState,
    replay: &ReplayMetadata,
    stamp: &OperationStamp,
) -> Result<(), StoreError> {
    let valid = match (state, replay) {
        (JournalState::Prepared { plan, generation }, ReplayMetadata::Prepared) => {
            stamp.digest
                == prepare_digest(&stamp.operation_id, &stamp.owner_nonce, plan, *generation)
        }
        (
            JournalState::BindAuthorized {
                candidate,
                authorization,
            },
            ReplayMetadata::BindAuthorized,
        ) => {
            authorization.matches_candidate(candidate)
                && stamp.digest == bind_digest(&stamp.operation_id, &stamp.owner_nonce, candidate)
        }
        (JournalState::Bound { .. }, ReplayMetadata::Bound { bind_digest }) => {
            valid_digest(bind_digest)
                && stamp.digest
                    == digest(&[
                        b"bound",
                        stamp.operation_id.as_bytes(),
                        stamp.owner_nonce.as_bytes(),
                        bind_digest.as_bytes(),
                    ])
        }
        (
            JournalState::RetireAuthorized {
                panel,
                authorization,
            },
            ReplayMetadata::RetireAuthorized,
        ) => {
            authorization.matches_panel(panel)
                && stamp.digest
                    == retire_digest(
                        &stamp.operation_id,
                        &stamp.owner_nonce,
                        panel,
                        authorization.delete_message_id,
                    )
        }
        (
            JournalState::Retired { .. },
            ReplayMetadata::Retired {
                retire_digest,
                observation,
            },
        ) => {
            valid_digest(retire_digest)
                && stamp.digest
                    == retired_digest(
                        &stamp.operation_id,
                        &stamp.owner_nonce,
                        retire_digest,
                        *observation,
                    )
        }
        (JournalState::Quarantined { .. }, ReplayMetadata::Quarantined) => true,
        _ => false,
    };
    valid.then_some(()).ok_or(StoreError::InvariantViolation)
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"agentdesk.status-panel-journal.v2\0");
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}

fn plan_parts(plan: &PanelPlan, generation: u64) -> Vec<Vec<u8>> {
    vec![
        plan.identity.provider().as_bytes().to_vec(),
        plan.identity.canonical_token_hash().as_bytes().to_vec(),
        plan.identity.channel_id().to_be_bytes().to_vec(),
        plan.turn_id.to_be_bytes().to_vec(),
        generation.to_be_bytes().to_vec(),
        vec![u8::from(plan.expected_prior_message_id.is_some())],
        plan.expected_prior_message_id
            .unwrap_or(0)
            .to_be_bytes()
            .to_vec(),
    ]
}

pub(super) fn prepare_digest(
    operation_id: &str,
    nonce: &str,
    plan: &PanelPlan,
    generation: u64,
) -> String {
    let mut owned = vec![
        b"prepare".to_vec(),
        operation_id.as_bytes().to_vec(),
        nonce.as_bytes().to_vec(),
    ];
    owned.extend(plan_parts(plan, generation));
    digest(&owned.iter().map(Vec::as_slice).collect::<Vec<_>>())
}

pub(super) fn bind_digest(operation_id: &str, nonce: &str, candidate: &Candidate) -> String {
    let mut owned = vec![
        b"bind_authorized".to_vec(),
        operation_id.as_bytes().to_vec(),
        nonce.as_bytes().to_vec(),
    ];
    owned.extend(plan_parts(&candidate.plan, candidate.generation));
    owned.push(candidate.message_id.to_be_bytes().to_vec());
    digest(&owned.iter().map(Vec::as_slice).collect::<Vec<_>>())
}

pub(super) fn commit_bind_digest(authorization: &JournalOwnedBind) -> String {
    digest(&[
        b"bound",
        authorization.operation_id.as_bytes(),
        authorization.owner_nonce.as_bytes(),
        authorization.digest.as_bytes(),
    ])
}

pub(super) fn retire_digest(
    operation_id: &str,
    nonce: &str,
    panel: &BoundPanel,
    delete_message_id: u64,
) -> String {
    digest(&[
        b"retire_authorized",
        operation_id.as_bytes(),
        nonce.as_bytes(),
        &panel.candidate.message_id.to_be_bytes(),
        &delete_message_id.to_be_bytes(),
        &panel.candidate.generation.to_be_bytes(),
    ])
}

pub(super) fn delete_digest(
    authorization: &JournalOwnedRetire,
    observation: DeleteObservation,
) -> String {
    retired_digest(
        &authorization.operation_id,
        &authorization.owner_nonce,
        &authorization.digest,
        observation,
    )
}

fn retired_digest(
    operation_id: &str,
    nonce: &str,
    retire_digest: &str,
    observation: DeleteObservation,
) -> String {
    let code = match observation {
        DeleteObservation::Deleted => b"deleted".as_slice(),
        DeleteObservation::NotFound404 => b"404",
        DeleteObservation::UnknownMessage10008 => b"10008",
        DeleteObservation::Forbidden403 => b"403",
        DeleteObservation::Transient => b"transient",
    };
    digest(&[
        b"retired",
        operation_id.as_bytes(),
        nonce.as_bytes(),
        retire_digest.as_bytes(),
        code,
    ])
}

fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
