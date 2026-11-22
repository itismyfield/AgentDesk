//! The execution decides whether the TUI owner takes an input; its origin decides whether it
//! joins a running turn or waits for its own.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputOrigin {
    Discord,
    Imessage,
    /// A selection or opinion the Decision service approved; its origin names the PG event.
    Decision,
    HttpStart,
    EffortContinue,
    /// The greeting an authorized restart command queued; only its durable bind names it.
    RestartSeed,
    Routine,
    #[cfg(test)]
    DirectHeadless,
}

/// The verified execution of the target, never read from an origin name or a source string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Execution {
    LocalTui,
    /// A TUI on another node: placement hands it to that node's owner.
    RemoteTui,
    Process,
    NonTuiProvider,
}

impl InputOrigin {
    pub(crate) fn is_person(self) -> bool {
        matches!(self, Self::Discord | Self::Imessage | Self::Decision)
    }

    /// Whether the input may enter a turn that already runs. A Decision is a person's instruction
    /// and still waits, as its one original, for the turn boundary and a ready input.
    pub(crate) fn joins_running_turn(self) -> bool {
        self.is_person() && self != Self::Decision
    }

    /// Old headless iMessage contexts and new ones follow the same person policy as the queue.
    pub(crate) fn from_context(context: Option<&serde_json::Value>) -> Self {
        if super::effort_continue::is_continuation(context) {
            return Self::EffortContinue;
        }
        if crate::services::turn_orchestrator::ChannelMailboxHandle::restart_seed_input(context) {
            return Self::RestartSeed;
        }
        if crate::services::turn_orchestrator::ChannelMailboxHandle::person_input(context) {
            return match context {
                None => Self::Discord,
                Some(context)
                    if crate::services::turn_orchestrator::ChannelMailboxHandle::decision_input(
                        context,
                    ) =>
                {
                    Self::Decision
                }
                Some(_) => Self::Imessage,
            };
        }
        match context.map(|context| context["origin"].as_str()) {
            Some(Some("headless")) => Self::HttpStart,
            _ => Self::Routine,
        }
    }

    /// A channel and the original GUID fix both the reservation and admission across retries.
    pub(crate) fn imessage_identity(
        channel: u64,
        source: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Option<(u64, String)> {
        if source != Some("imessage") {
            return None;
        }
        let metadata = metadata?;
        if metadata.get("routine_id").is_some() || metadata.get("scheduled_message_id").is_some() {
            return None;
        }
        let guid = metadata.pointer("/human_input/origin_id")?.as_str()?;
        if guid.is_empty() {
            return None;
        }
        Some(Self::hashed_identity(
            b"imessage-input:",
            channel,
            guid,
            "imsg",
        ))
    }

    /// One domain's message id and ticket for a channel and an origin key.
    fn hashed_identity(domain: &[u8], channel: u64, key: &str, tag: &str) -> (u64, String) {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(domain);
        hash.update(channel.to_be_bytes());
        hash.update(key.as_bytes());
        let digest = hash.finalize();
        let message = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) | (1 << 63);
        (message, format!("{tag}-{}", hex::encode(&digest[..28])))
    }

    /// The Decision counterpart under its own domain: the channel and the event origin fix the
    /// message and the ticket, never the session that was current when the request was made.
    pub(crate) fn decision_identity(
        channel: u64,
        source: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Option<(u64, String)> {
        if source != Some(crate::db::decisions::SOURCE) {
            return None;
        }
        let origin = metadata?.pointer("/human_input/origin_id")?.as_str()?;
        crate::db::decisions::event_of_origin(origin)?;
        Some(Self::hashed_identity(
            b"decision-input:",
            channel,
            origin,
            "dcsn",
        ))
    }

    /// The identity an input's own origin fixes, whichever domain names it.
    pub(crate) fn stable_identity(
        channel: u64,
        source: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Option<(u64, String)> {
        Self::imessage_identity(channel, source, metadata)
            .or_else(|| Self::decision_identity(channel, source, metadata))
    }

    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::Discord,
        Self::Imessage,
        Self::Decision,
        Self::HttpStart,
        Self::EffortContinue,
        Self::RestartSeed,
        Self::Routine,
        Self::DirectHeadless,
    ];
}

impl Execution {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [
        Self::LocalTui,
        Self::RemoteTui,
        Self::Process,
        Self::NonTuiProvider,
    ];

    /// Any TUI target goes to a TUI owner whatever the origin, a remote one to its own node's
    /// through placement; a process or non-TUI provider keeps its own path.
    pub(crate) fn owner_takes(self, _origin: InputOrigin) -> bool {
        matches!(self, Self::LocalTui | Self::RemoteTui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_execution_alone_decides_whether_the_owner_takes_an_input() {
        for origin in InputOrigin::ALL {
            let taken: Vec<_> = (Execution::ALL.into_iter())
                .filter(|execution| execution.owner_takes(origin))
                .collect();
            assert_eq!(
                taken,
                [Execution::LocalTui, Execution::RemoteTui],
                "{origin:?}"
            );
        }
    }

    fn headless(source: &str, metadata: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"origin": "headless", "source": source, "metadata": metadata})
    }

    #[test]
    fn decision_origin_is_a_person_input_only_under_an_event_origin() {
        let event = "decision:7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e";
        let input = |origin: &str| serde_json::json!({"human_input": {"origin_id": origin}});
        for origin in [event.to_string(), format!("{event}:opinion:42")] {
            let context = headless("decision", input(&origin));
            let read = InputOrigin::from_context(Some(&context));
            assert_eq!(read, InputOrigin::Decision, "{origin}");
            assert!(read.is_person());
        }
        // The source name alone, a malformed event, or a routine's metadata is not a Decision.
        let mut routine = input(event);
        routine["routine_id"] = "r1".into();
        let others = [
            headless("decision", serde_json::json!({})),
            headless("decision", input("decision:not-a-uuid")),
            headless("decision", input(&format!("{event}:opinion:"))),
            headless("decision", input(&format!("{event}:extra"))),
            headless("decision", routine),
            headless("external", input(event)),
        ];
        for context in others {
            let read = InputOrigin::from_context(Some(&context));
            assert!(!read.is_person(), "{context}");
            assert_ne!(read, InputOrigin::Decision, "{context}");
        }
        let imessage = headless("imessage", input("guid-1"));
        assert_eq!(
            InputOrigin::from_context(Some(&imessage)),
            InputOrigin::Imessage
        );
    }

    #[test]
    fn decision_identity_is_fixed_by_channel_and_event_under_its_own_domain() {
        let event = "decision:7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e";
        let metadata = serde_json::json!({"human_input": {"origin_id": event}});
        let identity =
            |channel, source| InputOrigin::stable_identity(channel, Some(source), Some(&metadata));
        let (message, ticket) = identity(7, "decision").unwrap();
        assert_eq!(identity(7, "decision").unwrap(), (message, ticket.clone()));
        assert!(
            ticket.starts_with("dcsn-") && ticket.len() <= 64,
            "{ticket}"
        );
        assert_ne!(identity(8, "decision").unwrap().1, ticket);
        // The same origin text under the iMessage source is another domain's identity.
        let imessage = identity(7, "imessage").unwrap();
        assert!(imessage.1.starts_with("imsg-") && imessage.0 != message);
        assert_eq!(identity(7, "external"), None);
        let forged = serde_json::json!({"human_input": {"origin_id": "decision:forged"}});
        let read = InputOrigin::stable_identity(7, Some("decision"), Some(&forged));
        assert_eq!(read, None);
    }
}
