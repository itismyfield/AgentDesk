//! The execution decides whether the TUI owner takes an input; its origin decides whether it
//! joins a running turn or waits for its own.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputOrigin {
    Discord,
    Imessage,
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
        matches!(self, Self::Discord | Self::Imessage)
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
            return if context.is_some() {
                Self::Imessage
            } else {
                Self::Discord
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
        use sha2::{Digest, Sha256};
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
        let mut hash = Sha256::new();
        hash.update(b"imessage-input:");
        hash.update(channel.to_be_bytes());
        hash.update(guid.as_bytes());
        let digest = hash.finalize();
        let message = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) | (1 << 63);
        Some((message, format!("imsg-{}", hex::encode(&digest[..28]))))
    }

    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::Discord,
        Self::Imessage,
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
}
