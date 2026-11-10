#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputDelivery {
    Started {
        turn_id: String,
    },
    /// `inject_veto` names why a busy turn could not take the input; `None` when none was asked.
    Queued {
        turn_id: String,
        reason: String,
        inject_veto: Option<String>,
    },
    /// The busy turn's transcript recorded the input; `turn_id` is null unless the turn's durable
    /// row names a Discord message.
    Injected {
        turn_id: Option<String>,
    },
    /// A paste was attempted but not confirmed; nothing was queued.
    Unconfirmed {
        turn_id: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputError {
    AuthorNotAllowed,
    RuntimeUnavailable(String),
    QueueRefused(String),
    InvalidTarget(String),
}

/// Queue reason when a turn that never claimed the mailbox holds the channel.
pub const EXTERNAL_TURN_ACTIVE: &str = "external_turn_active";

/// Queue reason when a confirmed turn-mode channel's transcript reads neither idle nor busy.
pub const TURN_ACTIVITY_UNKNOWN: &str = "turn_activity_unknown";

/// What keeps a start off a channel whose mailbox may be free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalHold {
    Row,
    Busy,
    Unknown,
}

impl ExternalHold {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Row | Self::Busy => EXTERNAL_TURN_ACTIVE,
            Self::Unknown => TURN_ACTIVITY_UNKNOWN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalHold, HumanInputDelivery, HumanInputError};

    #[test]
    fn human_input_values_have_domain_owner() {
        assert!(
            std::any::type_name::<HumanInputDelivery>()
                .ends_with("::domain::human_input::HumanInputDelivery")
        );
        assert!(
            std::any::type_name::<HumanInputError>()
                .ends_with("::domain::human_input::HumanInputError")
        );
        assert!(
            std::any::type_name::<ExternalHold>().ends_with("::domain::human_input::ExternalHold")
        );
    }

    #[test]
    fn external_hold_reasons_are_stable() {
        for (hold, name, reason) in [
            (ExternalHold::Row, "Row", "external_turn_active"),
            (ExternalHold::Busy, "Busy", "external_turn_active"),
            (ExternalHold::Unknown, "Unknown", "turn_activity_unknown"),
        ] {
            assert_eq!(format!("{hold:?}"), name);
            assert_eq!(hold.reason(), reason);
        }
    }

    #[test]
    fn human_input_variant_and_field_names_are_stable() {
        let deliveries = [
            (
                HumanInputDelivery::Started {
                    turn_id: "turn-1".into(),
                },
                r#"Started { turn_id: "turn-1" }"#,
            ),
            (
                HumanInputDelivery::Queued {
                    turn_id: "turn-1".into(),
                    reason: "external_turn_active".into(),
                    inject_veto: None,
                },
                r#"Queued { turn_id: "turn-1", reason: "external_turn_active", inject_veto: None }"#,
            ),
            (
                HumanInputDelivery::Queued {
                    turn_id: "turn-1".into(),
                    reason: "external_turn_active".into(),
                    inject_veto: Some("not_busy".into()),
                },
                r#"Queued { turn_id: "turn-1", reason: "external_turn_active", inject_veto: Some("not_busy") }"#,
            ),
            (
                HumanInputDelivery::Injected { turn_id: None },
                "Injected { turn_id: None }",
            ),
            (
                HumanInputDelivery::Unconfirmed {
                    turn_id: Some("turn-1".into()),
                    detail: "not_observed".into(),
                },
                r#"Unconfirmed { turn_id: Some("turn-1"), detail: "not_observed" }"#,
            ),
        ];
        for (delivery, expected) in deliveries {
            assert_eq!(format!("{delivery:?}"), expected);
        }
        for (error, expected) in [
            (HumanInputError::AuthorNotAllowed, "AuthorNotAllowed"),
            (
                HumanInputError::RuntimeUnavailable("unavailable".into()),
                r#"RuntimeUnavailable("unavailable")"#,
            ),
            (
                HumanInputError::QueueRefused("refused".into()),
                r#"QueueRefused("refused")"#,
            ),
            (
                HumanInputError::InvalidTarget("invalid".into()),
                r#"InvalidTarget("invalid")"#,
            ),
        ] {
            assert_eq!(format!("{error:?}"), expected);
        }
    }
}
