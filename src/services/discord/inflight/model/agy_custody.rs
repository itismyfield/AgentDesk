//! The durable output owner of one AGY execution, written with the row's first create.

use std::num::NonZeroU64;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::services::tui_o::store::agy_delivery::AgyOriginal;

/// The spool producer that writes the execution's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum AgyProducer {
    /// The one-shot `-p` CLI; ACP turns are never routed.
    Print,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct AgyCustodyRoute {
    pub v: u32,
    pub producer: AgyProducer,
    pub destination: u64,
    pub logical_channel: u64,
    pub message: u64,
    pub nonce: Uuid,
    pub writer_generation: NonZeroU64,
    pub original: AgyOriginal,
}

const ROUTE_VERSION: u32 = 1;

/// A route this binary cannot read is kept verbatim and held, never read as Legacy.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::services::discord) enum AgyCustodyField {
    Route(AgyCustodyRoute),
    Unreadable(serde_json::Value),
}

impl Serialize for AgyCustodyField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Route(route) => route.serialize(serializer),
            Self::Unreadable(value) => value.serialize(serializer),
        }
    }
}

/// The row field's reader: a present field, `null` included, is custody; only absence is Legacy.
pub(super) fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<AgyCustodyField>, D::Error> {
    AgyCustodyField::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for AgyCustodyField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(
            match serde_json::from_value::<AgyCustodyRoute>(value.clone()) {
                Ok(route) if route.v == ROUTE_VERSION => Self::Route(route),
                _ => Self::Unreadable(value),
            },
        )
    }
}
