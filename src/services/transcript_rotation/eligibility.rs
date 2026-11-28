use crate::services::tui_o::{channel_policy::Adoption, writer::actor::Undelivered};

#[derive(Clone, Copy)]
pub(crate) enum InputOrigin {
    Ordinary,
    RotationRequest,
    RotationSeed,
}

pub(crate) struct Capability {
    pub(crate) cooperative_exit: bool,
    pub(crate) fresh_exact: bool,
    pub(crate) exit_queues_behind_turn: bool,
}

pub(crate) struct Eligibility<'a> {
    pub(crate) transcript_bytes: u64,
    pub(crate) adoption: Adoption,
    pub(crate) undelivered: Option<&'a Undelivered>,
    pub(crate) has_blocker: bool,
    pub(crate) queue_empty: bool,
    pub(crate) capability: Capability,
    pub(crate) origin: InputOrigin,
}

pub(crate) fn eligible(soft_bytes: u64, evidence: &Eligibility<'_>) -> bool {
    evidence.transcript_bytes >= soft_bytes
}
