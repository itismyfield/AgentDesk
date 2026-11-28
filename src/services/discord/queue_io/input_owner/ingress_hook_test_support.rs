// A test adopts only inside this scope; any other test keeps the channel unadopted.
tokio::task_local! { pub(crate) static ADOPTS: bool; }
tokio::task_local! {
    pub(crate) static BEFORE_RESUME: std::cell::RefCell<Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>>;
    pub(crate) static BEFORE_INGRESS_HOLD: std::cell::RefCell<Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>>;
}
