//! Explicit-auth mutation route inventory shared by the HTTP layer and the
//! service handlers that gate themselves with `require_explicit_bearer_token`.
//! Lives in `services` so `services::auto_queue` can reference it without a
//! service→server backflow (audit_maintainability `service_server_backflow`).

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExplicitAuthMutationRoute {
    /// Route domain used only for the boot-audit log (`kanban`, `auto-queue`).
    pub domain: &'static str,
    /// Operation label passed to `require_explicit_bearer_token` and echoed
    /// in its 401 error body.
    pub operation: &'static str,
}

impl ExplicitAuthMutationRoute {
    const fn new(domain: &'static str, operation: &'static str) -> Self {
        Self { domain, operation }
    }

    pub const KANBAN_REREVIEW: Self = Self::new("kanban", "rereview");
    pub const KANBAN_BATCH_REREVIEW: Self = Self::new("kanban", "batch rereview");
    pub const KANBAN_REOPEN: Self = Self::new("kanban", "reopen");
    pub const KANBAN_FORCE_TRANSITION: Self = Self::new("kanban", "force-transition");
    pub const AUTO_QUEUE_SUBMIT_ORDER: Self = Self::new("auto-queue", "submit_order");

    /// Gate a handler with this route's explicit-auth requirement (Bearer
    /// token and/or `x-channel-id`, see `services::kanban`). Thin wrapper so
    /// handlers stay one line and the label cannot drift from the inventory.
    pub(crate) fn require(
        self,
        headers: &axum::http::HeaderMap,
    ) -> Result<(), (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
        crate::services::kanban::require_explicit_bearer_token(headers, self.operation)
    }
}

impl std::fmt::Debug for ExplicitAuthMutationRoute {
    // Renders as the quoted `"domain: operation"` string the audit log has
    // always emitted, so log consumers see an unchanged format.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}: {}\"", self.domain, self.operation)
    }
}
