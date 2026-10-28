//! Read-only termination evidence for a retired execution, separate from live input targets.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedRecord;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReturnEndRefusal {
    NotRetired,
    Incomplete,
    EndpointMismatch,
    ReadUnavailable,
    PaneUnverified,
    ProviderUnverified(pane_probe::RecordedProcess),
}

#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum RetiredProbeRefusal {
    Gate(HerdrGateRefusal),
    Observation(ReturnEndRefusal),
}

#[cfg(unix)]
impl RetiredProbeRefusal {
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Gate(HerdrGateRefusal::MutationBusy)
                | Self::Observation(ReturnEndRefusal::ReadUnavailable)
        )
    }
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

    pub(crate) fn confirm(self) -> Result<(TerminationFence, RetiredEnd), RetiredProbeRefusal> {
        let location = self
            .record
            .location
            .as_ref()
            .ok_or(RetiredProbeRefusal::Observation(
                ReturnEndRefusal::Incomplete,
            ))?;
        let held = execution_fence(
            &self.endpoint,
            &location.pane_id,
            &self.record.execution_nonce,
        )
        .exclude_termination()
        .map_err(RetiredProbeRefusal::Gate)?;
        let proof =
            RetiredEnd::observe_with_os(&self.endpoint, self.transport, &self.record, self.os)
                .map_err(RetiredProbeRefusal::Observation)?;
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
        #[cfg(test)]
        use crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::mutant;

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
        let witness = view
            .transport
            .server_witness()
            .map_err(|_| ReturnEndRefusal::ReadUnavailable)?;
        let pane = view.read_execution_on_checked(&witness).map_err(|_| {
            #[cfg(test)]
            if mutant("retired-server-retry") {
                return ReturnEndRefusal::ReadUnavailable;
            }
            ReturnEndRefusal::EndpointMismatch
        })?;
        Self::from_reads(record, pane, os.as_ref())
    }

    fn from_reads(
        record: &HostedExecution,
        pane: PaneReading,
        os: &dyn ProcessOs,
    ) -> Result<Self, ReturnEndRefusal> {
        use pane_probe::RecordedProcess::{ProvenAbsent, Unreadable};
        let expected = record
            .expected
            .as_ref()
            .ok_or(ReturnEndRefusal::Incomplete)?;
        let observed = |stamp| match pane_probe::recorded_process(os, stamp) {
            Unreadable => Err(ReturnEndRefusal::ReadUnavailable),
            process => Ok(process),
        };
        let ended = match pane {
            PaneReading::Missing => true,
            PaneReading::Unreadable(_) => return Err(ReturnEndRefusal::ReadUnavailable),
            PaneReading::Present {
                root,
                provider: PaneProvider::Exited,
            } => {
                root == expected.root
                    || (observed(&expected.root)? == ProvenAbsent
                        && observed(&expected.provider_process)? == ProvenAbsent)
            }
            PaneReading::Present {
                provider:
                    PaneProvider::Unverified(
                        EvidenceGap::ReadFailed(_)
                        | EvidenceGap::ProcessUnreadable(_)
                        | EvidenceGap::EnvUnreadable,
                    ),
                ..
            } => return Err(ReturnEndRefusal::ReadUnavailable),
            PaneReading::Present { .. } => false,
        };
        if !ended {
            return Err(ReturnEndRefusal::PaneUnverified);
        }
        let process = observed(&expected.provider_process)?;
        if process != ProvenAbsent {
            return Err(ReturnEndRefusal::ProviderUnverified(process));
        }
        Ok(Self(record.clone()))
    }
}

#[cfg(test)]
#[path = "legacy_return_tests.rs"]
pub(crate) mod tests;
