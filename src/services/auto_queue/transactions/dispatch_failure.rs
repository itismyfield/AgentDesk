//! Entry dispatch-failure recording used by the engine `recordDispatchFailure` op.

pub(crate) use crate::db::auto_queue::record_entry_dispatch_failure_on_pg;
