//! Read-only termination evidence for a retired execution, separate from live input targets.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedRecord;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReturnEndRefusal {
    NotRetired,
    Incomplete,
    EndpointMismatch,
    PaneUnverified,
    ProviderUnverified(pane_probe::RecordedProcess),
}

/// Carries the exact record whose two termination observations succeeded.
#[derive(Debug)]
pub(crate) struct RetiredEnd(HostedExecution);

impl RetiredEnd {
    pub(crate) fn matches(&self, record: &HostedExecution) -> bool {
        self.0 == *record
    }

    /// Reads the original endpoint and the local process table; never sends input or close.
    pub(crate) fn observe(
        endpoint: &HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        record: &HostedExecution,
    ) -> Result<Self, ReturnEndRefusal> {
        Self::observe_with_os(endpoint, transport, record, Arc::new(HostOs))
    }

    fn observe_with_os(
        endpoint: &HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        record: &HostedExecution,
        os: Arc<dyn ProcessOs>,
    ) -> Result<Self, ReturnEndRefusal> {
        if !matches!(
            HostedRecord::decode(serde_json::to_value(record).ok().as_ref()),
            HostedRecord::Known(ref known) if known.state == HostedState::Retired
        ) {
            return Err(ReturnEndRefusal::NotRetired);
        }
        let location = record
            .location
            .as_ref()
            .ok_or(ReturnEndRefusal::Incomplete)?;
        record
            .expected
            .as_ref()
            .ok_or(ReturnEndRefusal::Incomplete)?;
        if !on_endpoint(location, endpoint) || record.owner.owner_node != endpoint.execution_node()
        {
            return Err(ReturnEndRefusal::EndpointMismatch);
        }
        let view = HerdrPaneView {
            transport,
            pane: location.pane_id.clone(),
            nonce: record.execution_nonce.clone(),
            provider: record.owner.provider.clone(),
            os: os.clone(),
            next_id: AtomicU64::new(1),
        };
        Self::from_reads(record, view.read_execution(), os.as_ref())
    }

    fn from_reads(
        record: &HostedExecution,
        pane: PaneReading,
        os: &dyn ProcessOs,
    ) -> Result<Self, ReturnEndRefusal> {
        if !crate::cli::herdr::confirmed_ended(record, &pane) {
            return Err(ReturnEndRefusal::PaneUnverified);
        }
        let expected = record
            .expected
            .as_ref()
            .ok_or(ReturnEndRefusal::Incomplete)?;
        let process = pane_probe::recorded_process(os, &expected.provider_process);
        if process != pane_probe::RecordedProcess::ProvenAbsent {
            return Err(ReturnEndRefusal::ProviderUnverified(process));
        }
        Ok(Self(record.clone()))
    }
}

#[cfg(test)]
#[path = "legacy_return_tests.rs"]
pub(crate) mod tests;
