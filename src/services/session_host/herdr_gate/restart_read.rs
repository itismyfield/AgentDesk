use super::*;

impl HerdrPaneView {
    /// A restart observation carries the server identity across the publication boundary.
    pub(crate) fn observe_execution(&self) -> Option<(ServerWitness, PaneReading)> {
        let witness = self.transport.server_witness().ok()?;
        let reading = self.read_execution_on_checked(&witness).ok()?;
        Some((witness, reading))
    }

    /// A shell regaining foreground cannot by itself prove that its provider exited.
    pub(crate) fn restart_ended(&self, record: &HostedExecution, reading: &PaneReading) -> bool {
        if let PaneReading::Present {
            provider: PaneProvider::Exited,
            ..
        } = reading
            && !record.expected.as_ref().is_some_and(|expected| {
                pane_probe::original_gone(self.os.as_ref(), &expected.provider_process, None)
                    .is_ok()
            })
        {
            return false;
        }
        self.ended(record, reading)
    }
}
