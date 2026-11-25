//! The value of the owner's retry wait and the typed refusal that names its cause.
use super::RefusedMember;

#[cfg(unix)]
const REFUSED: &str = "NotSent(\"pending_unattached:";

/// Why a front waits: a Claude Pending execution's typed refusal, or a held root recovery.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RetryCause {
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
    pub(crate) fn nonce(&self) -> Option<&str> {
        self.pending().map(|(nonce, _)| nonce)
    }

    /// The typed refusal a Claude turn returns for a Pending cause, ahead of its notice.
    #[cfg(unix)]
    pub(crate) fn refusal(&self, detail: &str) -> String {
        match self.pending() {
            Some((nonce, kind)) => format!("{REFUSED}{nonce}:{kind}\"): {detail}"),
            None => detail.to_owned(),
        }
    }

    /// The Pending cause a typed refusal names; any other error names none.
    #[cfg(unix)]
    pub(crate) fn from_refusal(error: &str) -> Option<Self> {
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

/// One front's wait, kept across restarts; callbacks name it by front and `wait_seq` alone.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RetryWait {
    pub(crate) ticket: Option<String>,
    pub(crate) front: RefusedMember,
    pub(crate) wait_seq: u64,
    pub(crate) cause: RetryCause,
    pub(crate) due_at_ms: u64,
    /// The dues this front consumed for the same execution.
    pub(crate) attempts: u32,
    pub(crate) armed: bool,
}

/// The current wait and the last wait number issued; a number is never issued twice.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Waits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wait: Option<RetryWait>,
    #[serde(default)]
    pub(crate) seq: u64,
}

impl Waits {
    pub(crate) fn is_unused(&self) -> bool {
        self.wait.is_none() && self.seq == 0
    }
}
