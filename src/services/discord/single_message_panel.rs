use std::sync::OnceLock;

pub(super) fn enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let raw = std::env::var("AGENTDESK_SINGLE_MESSAGE_PANEL").ok();
        let enabled = parse_single_message_panel_flag(raw.as_deref());
        let state = if enabled { "enabled" } else { "disabled" };
        tracing::info!("  ✓ single_message_panel: {state}");
        enabled
    })
}

fn parse_single_message_panel_flag(raw: Option<&str>) -> bool {
    raw.map(str::trim)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn single_message_panel_flag_defaults_off_when_unset() {
        assert!(!super::parse_single_message_panel_flag(None));
    }

    #[test]
    fn single_message_panel_flag_accepts_only_documented_truthy_values() {
        for raw in ["1", "true", "TRUE", "TrUe", " true "] {
            assert!(
                super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should enable the flag"
            );
        }
    }

    #[test]
    fn single_message_panel_flag_rejects_falsy_and_garbage_values() {
        for raw in ["", "0", "false", "FALSE", "yes", "on", "garbage"] {
            assert!(
                !super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should leave the flag disabled"
            );
        }
    }
}
