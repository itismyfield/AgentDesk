use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row as SqlxRow};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use crate::server::routes::AppState;
use crate::services::{auto_queue::AutoQueueLogContext, provider::ProviderKind};

#[path = "activate_bridge.rs"]
mod activate_bridge;
#[path = "activate_command.rs"]
mod activate_command;
#[path = "activate_preflight.rs"]
mod activate_preflight;
#[path = "activate_route.rs"]
mod activate_route;
#[path = "command.rs"]
mod command;
#[path = "control_routes.rs"]
mod control_routes;
#[path = "dispatch_assignment_command.rs"]
mod dispatch_assignment_command;
#[path = "dispatch_command.rs"]
mod dispatch_command;
#[path = "dispatch_query.rs"]
mod dispatch_query;
#[path = "fsm.rs"]
mod fsm;
#[path = "order_routes.rs"]
mod order_routes;
#[path = "phase_gate.rs"]
mod phase_gate;
#[path = "phase_gate_catalog.rs"]
mod phase_gate_catalog;
#[path = "planning.rs"]
mod planning;
#[path = "query.rs"]
mod query;
#[path = "route_generate.rs"]
mod route_generate;
#[path = "route_request_generate.rs"]
mod route_request_generate;
#[path = "route_types.rs"]
mod route_types;
#[path = "slot_routes.rs"]
mod slot_routes;
#[path = "view.rs"]
mod view;
#[path = "view_admin_routes.rs"]
mod view_admin_routes;

pub use activate_route::activate;
pub use control_routes::{
    cancel, pause, reorder, repair_phase_gates, reset, reset_global, reset_slot_thread, resume_run,
    update_run,
};
pub use order_routes::{OrderBody, submit_order};
pub use phase_gate_catalog::{DEFAULT_PHASE_GATE_KIND, catalog as phase_gate_catalog};
pub use route_generate::generate;
pub use route_request_generate::request_generate;
pub use route_types::{
    ActivateBody, AddRunEntryBody, CancelQuery, GenerateBody, GenerateEntryBody, HistoryQuery,
    PauseBody, RebindSlotBody, ReorderBody, RepairPhaseGateBody, ResetBody, ResetGlobalBody,
    StatusQuery, UpdateEntryBody, UpdateRunBody,
};
pub use slot_routes::{rebind_slot, skip_entry};
pub use view_admin_routes::{add_run_entry, history, restore_run, status, update_entry};

pub(crate) use activate_bridge::activate_with_deps;
pub(crate) use activate_command::activate_with_deps_pg;
pub(crate) use fsm::{AutoQueueActivateDeps, activate_with_bridge_pg};

use activate_preflight::*;
use command::*;
use dispatch_assignment_command::*;
use dispatch_command::*;
use dispatch_query::*;
use fsm::{
    ActivatePreflightOutcome, apply_restore_state_changes_pg, attempt_restore_dispatch,
    clamp_retry_limit, create_activate_dispatch_for_entry_prefer_pg, finalize_restore_run_pg,
    load_kv_meta_value_pg, record_consultation_dispatch_prefer_pg,
    select_consultation_counterpart_prefer_pg, update_entry_status_prefer_pg,
};
use phase_gate::*;
use planning::*;
use query::*;
use route_types::{
    AUTO_QUEUE_REVIEW_MODE_DISABLED, AUTO_QUEUE_REVIEW_MODE_ENABLED, DependencyParseResult,
    GenerateCandidate, PlannedEntry, RESET_GLOBAL_CONFIRMATION_TOKEN,
};
use view::*;
