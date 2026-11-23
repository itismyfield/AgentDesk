//! Test hook recording the provider and channels a scheduled turn launch resolved.
tokio::task_local! {
    pub(crate) static START_TARGET: std::cell::RefCell<Option<(crate::services::provider::ProviderKind, u64, u64)>>;
}
