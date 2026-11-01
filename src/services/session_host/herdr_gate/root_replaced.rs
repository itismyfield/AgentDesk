//! Strict read-only evidence for retiring one replaced-root execution.
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
        if record.state != HostedState::Bound
            || record.expected.as_ref() != Some(&self.0.expected)
            || record.execution_nonce != self.0.nonce
        {
            return Err("stored launch evidence differs".into());
        }
        let witness = self
            .revalidate_termination_reads()
            .map_err(|why| format!("restore: {why:?}"))?;
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
            } if root != self.0.expected.root => {}
            other => return Err(format!("replaced pane is not a lone shell: {other:?}")),
        }
        let rebooted = self.0.rebooted(&witness);
        for (name, stamp) in [
            ("root", &self.0.expected.root),
            ("provider", &self.0.expected.provider_process),
        ] {
            if let Err(read) = pane_probe::original_gone(self.0.os.as_ref(), stamp, rebooted) {
                return Err(format!("original {name} is not proven absent: {read:?}"));
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

    /// `HerdrPaneView::ended` of a reading of this target's pane; on a server that replaced the
    /// granted one, a recorded process that started before this boot also counts as gone.
    pub(crate) fn ended_reading(&self, record: &HostedExecution, reading: &PaneReading) -> bool {
        let rebooted = match self.0.transport.server_witness() {
            Ok(current) => self.0.rebooted(&current),
            Err(_) => None,
        };
        ended_across(self.0.os.as_ref(), record, reading, rebooted)
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
