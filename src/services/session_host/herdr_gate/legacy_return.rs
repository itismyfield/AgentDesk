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

/// A retired execution's endpoint reads and nonce exclusion; it exposes no pane mutation.
#[cfg(unix)]
pub(crate) struct RetiredProbe {
    endpoint: HerdrEndpoint,
    transport: Arc<dyn HerdrTransport>,
    record: HostedExecution,
    os: Arc<dyn ProcessOs>,
}

#[cfg(unix)]
impl RetiredProbe {
    pub(crate) fn new(
        endpoint: HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        record: &HostedExecution,
    ) -> Option<Self> {
        let location = record.location.as_ref()?;
        if record.state != HostedState::Retired || !on_endpoint(location, &endpoint) {
            return None;
        }
        Some(Self {
            endpoint,
            transport,
            record: record.clone(),
            os: Arc::new(HostOs),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_os(mut self, os: Arc<dyn ProcessOs>) -> Self {
        self.os = os;
        self
    }

    pub(crate) fn confirm(self) -> Result<(TerminationFence, RetiredEnd), String> {
        let location = self
            .record
            .location
            .as_ref()
            .ok_or("missing retired location")?;
        let fence = execution_fence(
            &self.endpoint,
            &location.pane_id,
            &self.record.execution_nonce,
        );
        let permit = fence
            .acquire(Mutation::Terminate)
            .map_err(|error| format!("{error:?}"))?;
        let was_closing = {
            let mut state = fence.state.lock().unwrap_or_else(PoisonError::into_inner);
            let closing = state.1;
            state.1 = true;
            closing
        };
        let held = TerminationFence(permit, was_closing);
        let proof =
            RetiredEnd::observe_with_os(&self.endpoint, self.transport, &self.record, self.os)
                .map_err(|error| format!("{error:?}"))?;
        Ok((held, proof))
    }
}

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
        if !confirmed_ended(os, record, &pane) {
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
