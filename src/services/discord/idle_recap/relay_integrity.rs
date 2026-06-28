//! Deterministic idle-recap relay integrity probe.
//!
//! The probe is deliberately read-only: it compares a trusted provider output
//! end with a trusted Discord delivery frontier and classifies only the
//! unrelayed-tail case as suspect. Missing or untrusted evidence is `Unknown`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayIntegrityStatus {
    Ok,
    Suspect,
    Unknown,
}

impl RelayIntegrityStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "relay OK",
            Self::Suspect => "relay suspect",
            Self::Unknown => "relay unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayIntegrityInput {
    pub(crate) provider: String,
    pub(crate) session_key: String,
    pub(crate) provider_session_id: Option<String>,
    pub(crate) channel_id: u64,
    pub(crate) recap_message_id: Option<u64>,
    pub(crate) output_path: Option<String>,
    pub(crate) output_end: Option<u64>,
    pub(crate) committed_end: Option<u64>,
    pub(crate) committed_source: Option<String>,
    pub(crate) committed_range: Option<(u64, u64)>,
    pub(crate) anchor_message_id: Option<u64>,
    pub(crate) anchor_channel_id: Option<u64>,
    pub(crate) unknown_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayIntegrityProbe {
    pub(crate) status: RelayIntegrityStatus,
    pub(crate) provider: String,
    pub(crate) session_key: String,
    pub(crate) provider_session_id: Option<String>,
    pub(crate) channel_id: u64,
    pub(crate) recap_message_id: Option<u64>,
    pub(crate) output_path: Option<String>,
    pub(crate) output_end: Option<u64>,
    pub(crate) committed_end: Option<u64>,
    pub(crate) committed_source: Option<String>,
    pub(crate) committed_range: Option<(u64, u64)>,
    pub(crate) missing_range: Option<(u64, u64)>,
    pub(crate) anchor_message_id: Option<u64>,
    pub(crate) anchor_channel_id: Option<u64>,
    pub(crate) unknown_reason: Option<String>,
}

impl RelayIntegrityProbe {
    pub(crate) fn is_suspect(&self) -> bool {
        self.status == RelayIntegrityStatus::Suspect
    }

    pub(crate) fn diagnostic_report(&self) -> String {
        let mut lines = vec![
            "릴레이 조사".to_string(),
            format!("provider: {}", self.provider),
            format!("session_key: {}", self.session_key),
            format!(
                "provider_session_id: {}",
                display_opt_str(self.provider_session_id.as_deref())
            ),
            format!("channel_id: {}", self.channel_id),
            format!(
                "recap_message_id: {}",
                display_opt_u64(self.recap_message_id)
            ),
            format!("status: {}", self.status.label()),
            format!(
                "output_path: {}",
                self.output_path.as_deref().unwrap_or("unknown")
            ),
            format!("output_end: {}", display_opt_u64(self.output_end)),
            format!("committed_end: {}", display_opt_u64(self.committed_end)),
            format!(
                "committed_source: {}",
                self.committed_source.as_deref().unwrap_or("unknown")
            ),
            format!("committed_range: {}", display_range(self.committed_range)),
            format!("missing_range: {}", display_range(self.missing_range)),
            format!(
                "anchor_channel_id: {}",
                display_opt_u64(self.anchor_channel_id)
            ),
            format!(
                "anchor_message_id: {}",
                display_opt_u64(self.anchor_message_id)
            ),
        ];
        if let Some(reason) = self.unknown_reason.as_deref() {
            lines.push(format!("unknown_reason: {reason}"));
        }
        lines.join("\n")
    }
}

pub(crate) fn decide_relay_integrity(input: RelayIntegrityInput) -> RelayIntegrityProbe {
    let (status, missing_range, unknown_reason) = match (input.output_end, input.committed_end) {
        (Some(output_end), Some(committed_end)) if output_end > committed_end => (
            RelayIntegrityStatus::Suspect,
            Some((committed_end, output_end)),
            None,
        ),
        (Some(_), Some(_)) => (RelayIntegrityStatus::Ok, None, None),
        _ => (
            RelayIntegrityStatus::Unknown,
            None,
            input
                .unknown_reason
                .clone()
                .or_else(|| Some("missing output end or committed frontier".to_string())),
        ),
    };

    RelayIntegrityProbe {
        status,
        provider: input.provider,
        session_key: input.session_key,
        provider_session_id: input.provider_session_id,
        channel_id: input.channel_id,
        recap_message_id: input.recap_message_id,
        output_path: input.output_path,
        output_end: input.output_end,
        committed_end: input.committed_end,
        committed_source: input.committed_source,
        committed_range: input.committed_range,
        missing_range,
        anchor_message_id: input.anchor_message_id,
        anchor_channel_id: input.anchor_channel_id,
        unknown_reason,
    }
}

fn display_opt_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn display_opt_str(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn display_range(range: Option<(u64, u64)>) -> String {
    range
        .map(|(start, end)| format!("[{start}, {end})"))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(output_end: Option<u64>, committed_end: Option<u64>) -> RelayIntegrityInput {
        RelayIntegrityInput {
            provider: "codex".to_string(),
            session_key: "discord:codex:test".to_string(),
            provider_session_id: Some("provider-session-1".to_string()),
            channel_id: 42,
            recap_message_id: Some(9001),
            output_path: Some("/tmp/provider.jsonl".to_string()),
            output_end,
            committed_end,
            committed_source: committed_end
                .map(|_| "durable_delivery_record_current_generation".to_string()),
            committed_range: committed_end.map(|end| (0, end)),
            anchor_message_id: Some(111),
            anchor_channel_id: Some(42),
            unknown_reason: None,
        }
    }

    #[test]
    fn probe_ok_when_output_end_is_not_beyond_committed_end() {
        let probe = decide_relay_integrity(input(Some(100), Some(100)));
        assert_eq!(probe.status, RelayIntegrityStatus::Ok);
        assert_eq!(probe.missing_range, None);
    }

    #[test]
    fn probe_suspect_only_for_known_unrelayed_tail() {
        let probe = decide_relay_integrity(input(Some(150), Some(100)));
        assert_eq!(probe.status, RelayIntegrityStatus::Suspect);
        assert_eq!(probe.missing_range, Some((100, 150)));
        let report = probe.diagnostic_report();
        assert!(report.contains("missing_range: [100, 150)"));
        assert!(report.contains("provider_session_id: provider-session-1"));
        assert!(report.contains("anchor_message_id: 111"));
    }

    #[test]
    fn probe_unknown_when_any_required_signal_is_missing() {
        for probe in [
            decide_relay_integrity(input(None, Some(100))),
            decide_relay_integrity(input(Some(100), None)),
        ] {
            assert_eq!(probe.status, RelayIntegrityStatus::Unknown);
            assert_eq!(probe.missing_range, None);
            assert!(!probe.is_suspect());
        }
    }
}
