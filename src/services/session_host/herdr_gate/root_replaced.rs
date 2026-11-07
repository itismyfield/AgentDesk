//! Strict read-only evidence for retiring one recorded execution.
use super::*;

#[derive(Debug)]
pub(crate) struct ReplacedRootEnd {
    record: HostedExecution,
    witness: ServerWitness,
}

impl HerdrTarget {
    /// E7, pane and original process absence must all belong to the same server.
    pub(crate) fn replaced_root_end(
        &self,
        record: &HostedExecution,
        previous: Option<&ReplacedRootEnd>,
    ) -> Result<ReplacedRootEnd, String> {
        self.recovery_end(record, previous, false)
    }

    pub(crate) fn recovery_end(
        &self,
        record: &HostedExecution,
        previous: Option<&ReplacedRootEnd>,
        marker_missing: bool,
    ) -> Result<ReplacedRootEnd, String> {
        if record.state != HostedState::Bound
            || record.expected.as_ref() != Some(&self.0.expected)
            || record.execution_nonce != self.0.nonce
        {
            return Err("stored launch evidence differs".into());
        }
        let witness = if marker_missing {
            self.missing_marker_witness()?
        } else {
            self.revalidate_termination_reads()
                .map_err(|why| format!("restore: {why:?}"))?
        };
        if previous.is_some_and(|old| old.record != *record || old.witness != witness) {
            return Err("execution or server changed between proofs".into());
        }
        let view = HerdrPaneView {
            transport: self.0.transport.clone(),
            pane: self.0.pane.clone(),
            nonce: self.0.nonce.clone(),
            provider: record.owner.provider.clone(),
            os: self.0.os.clone(),
            next_id: AtomicU64::new(1),
        };
        match view.read_execution_on(&witness) {
            PaneReading::Missing => {}
            PaneReading::Present {
                root,
                provider: PaneProvider::Exited,
            } if marker_missing && root == self.0.expected.root => {}
            PaneReading::Present {
                root,
                provider: PaneProvider::Exited,
            } if root != self.0.expected.root => {
                if marker_missing {
                    pane_probe::original_gone(
                        self.0.os.as_ref(),
                        &self.0.expected.root,
                        self.0.rebooted(&witness),
                    )
                    .map_err(|read| format!("original root is not proven absent: {read:?}"))?;
                }
            }
            other => return Err(format!("replaced pane is not a lone shell: {other:?}")),
        }
        if marker_missing {
            if !self.provider_absent()? {
                return Err("original provider is still alive".into());
            }
        } else {
            let rebooted = self.0.rebooted(&witness);
            for (name, stamp) in [
                ("root", &self.0.expected.root),
                ("provider", &self.0.expected.provider_process),
            ] {
                pane_probe::original_gone(self.0.os.as_ref(), stamp, rebooted)
                    .map_err(|read| format!("original {name} is not proven absent: {read:?}"))?;
            }
        }
        if self.0.transport.server_witness().as_ref() != Ok(&witness) {
            return Err("server changed after process observations".into());
        }
        Ok(ReplacedRootEnd {
            record: record.clone(),
            witness,
        })
    }

    fn missing_marker_witness(&self) -> Result<ServerWitness, String> {
        if read_host_kind_marker(&self.0.logical_key) != HostKindMarker::Absent {
            return Err("marker is not confirmed absent".into());
        }
        if self.0.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault {
            let evidence = self
                .0
                .evidence
                .as_ref()
                .map_err(|why| format!("policy: {why:?}"))?;
            let witness = self
                .0
                .transport
                .server_witness()
                .map_err(|why| format!("peer: {why:?}"))?;
            let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
                execution_node: self.0.endpoint.execution_node().into(),
                config_key: self.0.endpoint.config_key().into(),
                socket_addr: self.0.endpoint.socket_path().to_string_lossy().into_owned(),
                herdr_session: self.0.endpoint.herdr_session().into(),
            };
            return ExistingMutationPermit::issue(&endpoint, &self.0.nonce, witness, evidence)
                .map(|permit| permit.witness().clone())
                .map_err(|why| format!("policy: {why:?}"));
        }
        match (self.0.read_restore)(self.0.transport.as_ref(), &self.0.endpoint) {
            RestoreResume::Off { witness } => Ok(witness),
            other => Err(format!("restore: {other:?}")),
        }
    }

    /// `HerdrPaneView::ended` of a reading of this target's pane; on a server that replaced the
    /// granted one, a recorded process that started before this boot also counts as gone.
    pub(crate) fn ended_reading(&self, record: &HostedExecution, reading: &PaneReading) -> bool {
        let rebooted = || match self.0.transport.server_witness() {
            Ok(current) => self.0.rebooted(&current),
            Err(_) => None,
        };
        ended_across(self.0.os.as_ref(), record, reading, &rebooted)
    }
}

impl PaneGate {
    /// The host's boot, only when `current` is not the server this nonce's grant admitted.
    fn rebooted(&self, current: &ServerWitness) -> Option<provenance::StartIdentity> {
        let evidence = self.evidence.as_ref().ok()?;
        let launch = crate::services::herdr_launch::HerdrLaunchEndpoint {
            execution_node: self.endpoint.execution_node().into(),
            config_key: self.endpoint.config_key().into(),
            socket_addr: self.endpoint.socket_path().to_string_lossy().into_owned(),
            herdr_session: self.endpoint.herdr_session().into(),
        };
        let granted = evidence
            .grant
            .as_ref()?
            .witness_for(&launch, &self.nonce)
            .ok()?;
        (granted != current).then(|| self.os.boot_time()).flatten()
    }
}
