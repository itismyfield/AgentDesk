//! Explicit-auth mutation labels shared by the boot audit and handler gates.
//! Kept in services so auto-queue handlers do not depend on the HTTP layer.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExplicitAuthMutationRoute {
    /// Domain shown in the boot audit.
    pub domain: &'static str,
    /// Existing operation label echoed in the guard's 401 response.
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

    /// Preserve the existing token/channel policy while sharing the operation
    /// label with the audit. The explicit guard does not accept Origin/Referer.
    pub(crate) fn require(
        self,
        headers: &axum::http::HeaderMap,
    ) -> Result<(), (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
        crate::services::kanban::require_explicit_bearer_token(headers, self.operation)
    }
}

impl std::fmt::Debug for ExplicitAuthMutationRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the legacy quoted "domain: operation" audit log format.
        write!(f, "\"{}: {}\"", self.domain, self.operation)
    }
}
