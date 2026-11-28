pub(in crate::services::discord) struct TestBindingRoot(Option<std::path::PathBuf>);

impl TestBindingRoot {
    pub(in crate::services::discord) fn enter(root: Option<&std::path::Path>) -> Self {
        let p5 = crate::services::tui_prompt_dedupe::binding_events::test_root();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(root);
        Self(p5)
    }
}

impl Drop for TestBindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}
