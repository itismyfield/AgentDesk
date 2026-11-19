//! How far a diagnosed problem may be repaired; shared by the doctor CLI and health diagnostics.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixSafety {
    ReadOnly,
    SafeLocalRepair,
    SafeIdleTmuxRepair,
    ExplicitRestartRequired,
    ExplicitDbRepairRequired,
    NotFixable,
}

impl FixSafety {
    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            "safe_local_repair" => Some(Self::SafeLocalRepair),
            "safe_idle_tmux_repair" => Some(Self::SafeIdleTmuxRepair),
            "explicit_restart_required" => Some(Self::ExplicitRestartRequired),
            "explicit_db_repair_required" => Some(Self::ExplicitDbRepairRequired),
            "not_fixable" => Some(Self::NotFixable),
            _ => None,
        }
    }

    pub(crate) fn restriction_rank(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::SafeLocalRepair => 1,
            Self::SafeIdleTmuxRepair => 2,
            Self::ExplicitRestartRequired => 3,
            Self::ExplicitDbRepairRequired => 4,
            Self::NotFixable => 5,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::SafeLocalRepair => "safe_local_repair",
            Self::SafeIdleTmuxRepair => "safe_idle_tmux_repair",
            Self::ExplicitRestartRequired => "explicit_restart_required",
            Self::ExplicitDbRepairRequired => "explicit_db_repair_required",
            Self::NotFixable => "not_fixable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FixSafety;

    #[test]
    fn wire_json_and_rank_cover_the_six_values_in_restriction_order() {
        let ordered = [
            (FixSafety::ReadOnly, "read_only"),
            (FixSafety::SafeLocalRepair, "safe_local_repair"),
            (FixSafety::SafeIdleTmuxRepair, "safe_idle_tmux_repair"),
            (
                FixSafety::ExplicitRestartRequired,
                "explicit_restart_required",
            ),
            (
                FixSafety::ExplicitDbRepairRequired,
                "explicit_db_repair_required",
            ),
            (FixSafety::NotFixable, "not_fixable"),
        ];
        for (rank, (value, wire)) in ordered.into_iter().enumerate() {
            assert_eq!(value.as_str(), wire);
            assert_eq!(FixSafety::from_wire(wire), Some(value));
            assert_eq!(serde_json::json!(value), serde_json::json!(wire));
            assert_eq!(usize::from(value.restriction_rank()), rank);
        }
        assert_eq!(FixSafety::from_wire("unknown"), None);
    }
}
