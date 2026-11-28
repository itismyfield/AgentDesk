use crate::services::process::ProcessIdentity;

#[derive(Clone, Debug)]
pub struct CapturedProcess {
    pub pid: u32,
    pub identity: Option<ProcessIdentity>,
}

impl CapturedProcess {
    #[cfg(any(test, feature = "test-support"))]
    pub fn capture_for_test(pid: u32) -> Self {
        Self::capture(pid)
    }

    pub(crate) fn capture(pid: u32) -> Self {
        let identity = ProcessIdentity::capture(pid);
        let identity = (identity.persisted_starttime().is_some()
            || identity.persisted_macos_lstart_hash().is_some())
        .then_some(identity);
        Self { pid, identity }
    }
}
