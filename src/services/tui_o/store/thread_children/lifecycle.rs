//! A thread child's durable lifecycle mark: a logical retirement that deletes nothing.

use serde::{Deserialize, Serialize};

use super::super::{OStore, StoreError, damage, durable};

const FILE: &str = "child_lifecycle";
const SCHEMA: u32 = 1;

/// The newest lifecycle fact of one child store; no mark is an active first generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildLifecycle {
    schema: u32,
    pub(crate) generation: u64,
    pub(crate) retired: bool,
}

impl ChildLifecycle {
    pub(crate) fn new(generation: u64, retired: bool) -> Self {
        Self {
            schema: SCHEMA,
            generation,
            retired,
        }
    }
}

impl OStore {
    /// `Ok(None)` is no mark. A mark that cannot be read whole is an error, never a retirement.
    pub(crate) fn read_child_lifecycle(
        &self,
        channel: u64,
    ) -> Result<Option<ChildLifecycle>, StoreError> {
        let mark: Option<ChildLifecycle> =
            durable::read_json(&self.channel_dir(channel).join(FILE))?;
        match mark {
            Some(mark) if mark.schema != SCHEMA || mark.generation == 0 => {
                Err(damage("child lifecycle mark of an unknown form"))
            }
            mark => Ok(mark),
        }
    }

    /// Replaces the mark atomically beside the child's recorded `init`, which stays as it is.
    pub(crate) fn publish_child_lifecycle(
        &self,
        channel: u64,
        mark: ChildLifecycle,
    ) -> Result<(), StoreError> {
        if !matches!(self.read_init(channel)?, Some(init) if init.policy_parent.is_some()) {
            return Err(StoreError::Rejected(
                "a lifecycle mark requires a recorded thread child".into(),
            ));
        }
        let bytes = serde_json::to_vec(&mark).map_err(|error| damage(error.to_string()))?;
        Ok(durable::replace(
            &self.channel_dir(channel).join(FILE),
            &bytes,
        )?)
    }
}
