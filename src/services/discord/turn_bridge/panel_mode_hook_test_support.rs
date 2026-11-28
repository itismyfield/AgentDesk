thread_local! {
    /// Lets a test reach the two-message completion that the process-wide footer flag hides.
    pub(in crate::services::discord) static SEPARATE_PANEL_FOR_TESTS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}
