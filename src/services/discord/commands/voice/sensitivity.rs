use crate::voice::barge_in::BargeInSensitivity;

fn label(sensitivity: BargeInSensitivity) -> &'static str {
    match sensitivity {
        BargeInSensitivity::Normal => "normal",
        BargeInSensitivity::Conservative => "conservative",
    }
}

/// `/voice sensitivity` reply. Conservative mode falls back to the configured
/// default after the TTL (clamped to 1s like the runtime), so say when.
pub(super) fn reply(
    selected: BargeInSensitivity,
    default: BargeInSensitivity,
    conservative_ttl_secs: u64,
) -> String {
    let mut reply = format!("Voice barge-in sensitivity: {}.", label(selected));
    if selected == BargeInSensitivity::Conservative && default != selected {
        let ttl = conservative_ttl_secs.max(1);
        reply.push_str(&format!(
            " Returns to {} automatically after {ttl}s.",
            label(default)
        ));
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Conservative mode announces when it reverts to the default, using the
    /// same 1s floor as the runtime; modes that never revert say nothing.
    #[test]
    fn reply_announces_auto_revert_only_when_it_happens() {
        use BargeInSensitivity::{Conservative, Normal};
        assert_eq!(
            reply(Conservative, Normal, 900),
            "Voice barge-in sensitivity: conservative. Returns to normal automatically after 900s."
        );
        assert!(reply(Conservative, Normal, 0).ends_with("after 1s."));
        assert_eq!(
            reply(Normal, Normal, 900),
            "Voice barge-in sensitivity: normal."
        );
        assert_eq!(
            reply(Conservative, Conservative, 900),
            "Voice barge-in sensitivity: conservative."
        );
    }
}
