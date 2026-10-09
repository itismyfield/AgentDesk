//! Serialized supervisor commands; dropping a reply receiver never cancels a WAL loan.

use tokio::sync::oneshot;

use super::fence::Failure;
use super::ordering::{PendingSource, ScanCapability, ValidatedOrderCapability};
use super::receipt::Receipt;
use super::source::Source;
use crate::services::tui_input::receipt_identity::{ReceiptIdentity, Responsibility};

pub(crate) struct ScanCommit {
    pub(crate) receipt: Receipt,
    pub(crate) capability: ScanCapability,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FinalDisposition {
    NonTarget,
    TooOld,
    PermanentRefusal,
}

pub(crate) enum SupervisorCmd {
    Clear,
    PendingSource {
        sources: Vec<u64>,
        reply: oneshot::Sender<PendingSource>,
    },
    LookupResponsibility {
        identity: ReceiptIdentity,
        reply: oneshot::Sender<Responsibility>,
    },
    BeginScan {
        sources: Vec<u64>,
        horizon: u64,
        complete_fetch: bool,
        reply: oneshot::Sender<Result<ScanCapability, Failure>>,
    },
    CommitFromScan {
        source: Box<Source>,
        capability: ScanCapability,
        reply: oneshot::Sender<ScanCommit>,
    },
    SettleFromScan {
        source: u64,
        disposition: FinalDisposition,
        capability: ScanCapability,
        reply: oneshot::Sender<Result<ScanCapability, Failure>>,
    },
    CompleteScan {
        capability: ScanCapability,
        reply: oneshot::Sender<Result<ValidatedOrderCapability, Failure>>,
    },
}
