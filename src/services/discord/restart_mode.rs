use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InflightRestartMode {
    DrainRestart,
    HotSwapHandoff,
}

impl InflightRestartMode {
    pub(crate) const fn as_u8(self) -> u8 {
        match self {
            Self::DrainRestart => 1,
            Self::HotSwapHandoff => 2,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::DrainRestart),
            2 => Some(Self::HotSwapHandoff),
            _ => None,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DrainRestart => "drain_restart",
            Self::HotSwapHandoff => "hot_swap_handoff",
        }
    }
}
