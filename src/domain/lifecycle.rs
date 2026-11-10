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

#[cfg(test)]
mod tests {
    use super::InflightRestartMode;

    #[test]
    fn restart_mode_has_domain_owner() {
        assert!(
            std::any::type_name::<InflightRestartMode>()
                .ends_with("::domain::lifecycle::InflightRestartMode")
        );
    }

    #[test]
    fn restart_mode_wire_names_and_labels_are_stable() {
        for (mode, wire) in [
            (InflightRestartMode::DrainRestart, "drain_restart"),
            (InflightRestartMode::HotSwapHandoff, "hot_swap_handoff"),
        ] {
            assert_eq!(mode.label(), wire);
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(wire));
            assert_eq!(
                serde_json::from_value::<InflightRestartMode>(serde_json::json!(wire)).unwrap(),
                mode
            );
        }
    }

    #[test]
    fn restart_mode_numeric_values_are_stable() {
        assert_eq!(InflightRestartMode::DrainRestart.as_u8(), 1);
        assert_eq!(InflightRestartMode::HotSwapHandoff.as_u8(), 2);
        assert_eq!(
            InflightRestartMode::from_u8(1),
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(
            InflightRestartMode::from_u8(2),
            Some(InflightRestartMode::HotSwapHandoff)
        );
    }

    #[test]
    fn restart_mode_invalid_numeric_values_are_none() {
        for value in 0..=u8::MAX {
            if value != 1 && value != 2 {
                assert_eq!(InflightRestartMode::from_u8(value), None, "{value}");
            }
        }
    }
}
