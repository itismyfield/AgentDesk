use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RotationIntent {
    pub(crate) op_id: Uuid,
    pub(crate) native_session_id: Uuid,
    pub(crate) session_row_id: i64,
    pub(crate) requested_at: DateTime<Utc>,
    pub(crate) ticket: Ticket,
    pub(crate) submission: Option<StoredHandoff>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Ticket {
    pub(crate) digest: String,
    pub(crate) expires_at: DateTime<Utc>,
    #[serde(default)]
    pub(crate) revoked: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct StoredHandoff {
    pub(crate) receipt: Receipt,
    pub(crate) body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Receipt {
    pub(crate) op_id: Uuid,
    pub(crate) digest: String,
    pub(crate) stored_at: DateTime<Utc>,
}
