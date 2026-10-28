use serde::Deserialize;

use crate::services::campaign_execution::{
    definition::DefinitionInput,
    model::{ChangeReason, Command, StopMode},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    pub id: String,
    pub idempotency_key: String,
    pub definition: DefinitionInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    pub idempotency_key: String,
    pub expected_definition_revision: u64,
    pub definition: DefinitionInput,
    pub change: Option<ChangeReason>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandRequest {
    pub idempotency_key: String,
    pub command: PublicCommand,
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum PublicCommand {
    Start {
        expected_definition_revision: u64,
    },
    Stop {
        expected_definition_revision: u64,
        expected_run: String,
        mode: StopMode,
    },
    Resume {
        expected_definition_revision: u64,
        expected_run: String,
    },
    Retry {
        expected_definition_revision: u64,
        expected_run: String,
        node_id: String,
    },
}

impl PublicCommand {
    pub fn into_domain(self) -> crate::error::AppResult<Command> {
        let revision = match &self {
            Self::Start {
                expected_definition_revision,
            }
            | Self::Stop {
                expected_definition_revision,
                ..
            }
            | Self::Resume {
                expected_definition_revision,
                ..
            }
            | Self::Retry {
                expected_definition_revision,
                ..
            } => *expected_definition_revision,
        };
        super::protocol::revision(revision)?;
        Ok(match self {
            Self::Start {
                expected_definition_revision,
            } => Command::Start {
                expected_revision: expected_definition_revision,
            },
            Self::Stop {
                expected_definition_revision,
                expected_run,
                mode,
            } => Command::RequestStop {
                expected_revision: expected_definition_revision,
                expected_run,
                mode,
            },
            Self::Resume {
                expected_definition_revision,
                expected_run,
            } => Command::Resume {
                expected_revision: expected_definition_revision,
                expected_run,
            },
            Self::Retry {
                expected_definition_revision,
                expected_run,
                node_id,
            } => Command::PrepareRetry {
                expected_revision: expected_definition_revision,
                expected_run,
                node_id,
            },
        })
    }
}

fn page_size() -> i64 {
    100
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetailQuery {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    #[serde(default = "page_size")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventsQuery {
    #[serde(default = "page_size")]
    pub limit: i64,
    #[serde(default)]
    pub after: i64,
    pub run_id: Option<String>,
}
