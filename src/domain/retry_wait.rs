//! The shared cause and typed refusal for a held input front.

#[cfg(unix)]
const REFUSED: &str = "NotSent(\"pending_unattached:";

/// Why a front waits: a Claude Pending execution's typed refusal, or a held root recovery.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryCause {
    PendingUnplaced { nonce: String },
    PendingUnstarted { nonce: String },
    PendingUnproven { nonce: String },
    OwnLaunchTimeout { nonce: String },
    RootRecoveryHeld,
}

impl RetryCause {
    #[cfg(unix)]
    fn pending(&self) -> Option<(&str, &'static str)> {
        match self {
            Self::PendingUnplaced { nonce } => Some((nonce, "unplaced")),
            Self::PendingUnstarted { nonce } => Some((nonce, "unstarted")),
            Self::PendingUnproven { nonce } => Some((nonce, "unproven")),
            Self::OwnLaunchTimeout { nonce } => Some((nonce, "own_timeout")),
            Self::RootRecoveryHeld => None,
        }
    }

    /// The execution a Pending cause names; attempts carry over only within it.
    #[cfg(unix)]
    pub fn nonce(&self) -> Option<&str> {
        self.pending().map(|(nonce, _)| nonce)
    }

    /// The typed refusal a Claude turn returns for a Pending cause, ahead of its notice.
    #[cfg(unix)]
    pub fn refusal(&self, detail: &str) -> String {
        match self.pending() {
            Some((nonce, kind)) => format!("{REFUSED}{nonce}:{kind}\"): {detail}"),
            None => detail.to_owned(),
        }
    }

    /// The Pending cause a typed refusal names; any other error names none.
    #[cfg(unix)]
    pub fn from_refusal(error: &str) -> Option<Self> {
        let (label, _) = error.strip_prefix(REFUSED)?.split_once("\")")?;
        let (nonce, kind) = label.rsplit_once(':')?;
        let nonce = (!nonce.is_empty()).then(|| nonce.to_owned())?;
        match kind {
            "unplaced" => Some(Self::PendingUnplaced { nonce }),
            "unstarted" => Some(Self::PendingUnstarted { nonce }),
            "unproven" => Some(Self::PendingUnproven { nonce }),
            "own_timeout" => Some(Self::OwnLaunchTimeout { nonce }),
            _ => None,
        }
    }
}
