//! Thin dispatch outbox route shim.
//!
//! Outbox orchestration and message shaping live in
//! `crate::services::dispatches::outbox_route`; persistence lives in
//! `crate::db::dispatches::outbox`; queue processing lives in
//! `crate::services::dispatches::outbox_queue`.

pub(crate) use crate::services::dispatches::outbox_queue::dispatch_outbox_loop;
pub use crate::services::dispatches::outbox_route::resolve_channel_alias_pub;
pub(crate) use crate::services::dispatches::outbox_route::use_counter_model_channel;
