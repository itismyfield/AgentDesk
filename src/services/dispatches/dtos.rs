//! Dispatch loopback request DTOs.
//!
//! `UpdateDispatchBody` is the JSON request body for `PATCH /api/dispatches/{id}`.
//! It is consumed both by the axum route handler
//! (`crate::server::routes::dispatches::crud::update_dispatch`) and by service-layer
//! callers that drive the same endpoint over the internal-HTTP loopback
//! (`turn_bridge::completion_guard`, `discord::recovery_engine`,
//! `discord::internal_api`). It was relocated here from
//! `crate::server::routes::dispatches::crud` (#3037) so the dependency direction
//! is server → services; the route handler now references this services path via
//! `Json<crate::services::dispatches::UpdateDispatchBody>`. JSON shape and serde
//! attributes are byte-identical to the original definition.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_from: Option<Vec<String>>,
}
