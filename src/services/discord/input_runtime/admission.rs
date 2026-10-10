//! Durable receipt does not require a submit-ready binding, idle provider or empty composer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Admission {
    pub(crate) receipt_open: bool,
    pub(crate) submit_ready: bool,
}

impl Admission {
    pub(crate) fn new(receipt_open: bool, submit_ready: bool) -> Self {
        Self {
            receipt_open,
            submit_ready: receipt_open && submit_ready,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1a_running_or_draft_hold_blocks_submission_but_not_receipt() {
        let held = Admission::new(true, false);
        assert!(held.receipt_open);
        assert!(!held.submit_ready);
        assert_eq!(Admission::new(false, true), Admission::new(false, false));
        assert!(Admission::new(true, true).submit_ready);
    }
}
