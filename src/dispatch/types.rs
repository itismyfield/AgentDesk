#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchCreateOptions {
    pub skip_outbox: bool,
    pub sidecar_dispatch: bool,
}
