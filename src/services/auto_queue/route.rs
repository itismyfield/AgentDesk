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

include!("route_types.rs");
include!("query.rs");
include!("phase_gate.rs");
include!("command.rs");
include!("view.rs");
include!("fsm.rs");
include!("planning.rs");
include!("dispatch_query.rs");
include!("dispatch_command.rs");
include!("route_generate.rs");
include!("activate_route.rs");
include!("activate_preflight.rs");
include!("activate_command.rs");
include!("activate_bridge.rs");
include!("route_dispatch.rs");
include!("view_routes.rs");
include!("slot_routes.rs");
include!("control_routes.rs");
include!("order_routes.rs");
