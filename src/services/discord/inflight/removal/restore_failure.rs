/// Recovery errors preserve whether a fresh attempt can resolve the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestoreFailure {
    Retryable(String),
    Rejected(String),
}
impl RestoreFailure {
    pub(crate) fn retryable(reason: impl ToString) -> Self {
        Self::Retryable(reason.to_string())
    }
}
// Escape reserved tags at the String boundary so rejection text cannot become retryable.
fn encode_restore_failure(failure: &RestoreFailure) -> String {
    match failure {
        RestoreFailure::Retryable(reason) => format!("retryable: {reason}"),
        RestoreFailure::Rejected(reason)
            if reason.starts_with("retryable: ") || reason.starts_with("rejected: ") =>
        {
            format!("rejected: {reason}")
        }
        RestoreFailure::Rejected(reason) => reason.clone(),
    }
}
fn decode_restore_failure(reason: String) -> RestoreFailure {
    if let Some(payload) = reason.strip_prefix("rejected: ") {
        RestoreFailure::Rejected(payload.into())
    } else if let Some(payload) = reason.strip_prefix("retryable: ") {
        RestoreFailure::retryable(payload)
    } else {
        RestoreFailure::Rejected(reason)
    }
}
impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&encode_restore_failure(self))
    }
}
impl From<String> for RestoreFailure {
    fn from(reason: String) -> Self {
        decode_restore_failure(reason)
    }
}
impl From<&str> for RestoreFailure {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}
impl From<RestoreFailure> for String {
    fn from(reason: RestoreFailure) -> Self {
        reason.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::RestoreFailure;
    #[test]
    fn s3act_b3a2_failure_string_roundtrip_property() {
        let fragments = [
            "",
            "ordinary",
            "retryable: ",
            "retryable: X",
            "rejected: ",
            "rejected: retryable: X",
            "한글\n\0\\",
        ];
        for a in fragments {
            for b in fragments {
                for original in [
                    RestoreFailure::Retryable(format!("{a}{b}")),
                    RestoreFailure::Rejected(format!("{a}{b}")),
                ] {
                    assert_eq!(
                        RestoreFailure::from(original.to_string()),
                        original,
                        "failure variant and payload must roundtrip"
                    );
                    assert_eq!(
                        RestoreFailure::from(String::from(original.clone())),
                        original
                    );
                }
            }
        }
    }
}
