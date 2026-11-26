//! Read the screen and foreground list of the stored pane on the same server.
use super::{ForegroundProcesses, HerdrTarget, Reads, contract, pane_probe};

impl HerdrTarget {
    /// `None` means a listed tool is in the foreground. An unreported list alone is neutral.
    pub(crate) fn hold_bound_capture(&self) -> Result<Option<String>, ()> {
        let pane = &self.0;
        let witness = pane.transport.server_witness().map_err(|_| ())?;
        let reads = Reads {
            transport: pane.transport.as_ref(),
            next_id: &pane.next_id,
            pane: &pane.pane,
        };
        let (processes, answered) = reads.processes();
        let (root, foreground) = processes.map_err(|_| ())?;
        let expected = &pane.expected;
        if answered.ok().as_ref() != Some(&witness)
            || root != Some(expected.root.pid)
            || [&expected.root, &expected.provider_process]
                .into_iter()
                .any(|stamp| {
                    pane_probe::recorded_process(pane.os.as_ref(), stamp)
                        != pane_probe::RecordedProcess::ExactAlive
                })
            || pane.os.parent(expected.provider_process.pid) != Ok(expected.root.pid)
        {
            return Err(());
        }
        if let ForegroundProcesses::Listed(pids) = foreground {
            if !pids.contains(&expected.provider_process.pid) {
                return Err(());
            }
            if pids
                .iter()
                .any(|&pid| pid != expected.root.pid && pid != expected.provider_process.pid)
            {
                return Ok(None);
            }
        }
        let call = pane.call(contract::capture_ansi_request(&pane.pane, -80));
        let outcome = pane.transport.call_with_witness(&call, &witness);
        contract::capture_result(&call, outcome, &pane.pane)
            .map(Some)
            .map_err(|_| ())
    }
}
