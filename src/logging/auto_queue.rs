use tracing::field::{Empty, display};

use crate::domain::auto_queue_log::AutoQueueLogContext;

pub fn auto_queue_trace_span(action: &'static str, ctx: &AutoQueueLogContext<'_>) -> tracing::Span {
    let span = tracing::info_span!(
        target: "agentdesk::services::auto_queue",
        "auto_queue",
        action = action,
        run_id = Empty,
        entry_id = Empty,
        card_id = Empty,
        dispatch_id = Empty,
        thread_group = Empty,
        batch_phase = Empty,
        slot_index = Empty,
        agent_id = Empty,
    );

    if let Some(run_id) = ctx.run_id {
        span.record("run_id", display(run_id));
    }
    if let Some(entry_id) = ctx.entry_id {
        span.record("entry_id", display(entry_id));
    }
    if let Some(card_id) = ctx.card_id {
        span.record("card_id", display(card_id));
    }
    if let Some(dispatch_id) = ctx.dispatch_id {
        span.record("dispatch_id", display(dispatch_id));
    }
    if let Some(thread_group) = ctx.thread_group {
        span.record("thread_group", thread_group);
    }
    if let Some(batch_phase) = ctx.batch_phase {
        span.record("batch_phase", batch_phase);
    }
    if let Some(slot_index) = ctx.slot_index {
        span.record("slot_index", slot_index);
    }
    if let Some(agent_id) = ctx.agent_id {
        span.record("agent_id", display(agent_id));
    }

    span
}

#[macro_export]
macro_rules! auto_queue_log {
    ($level:ident, $action:expr, $ctx:expr, $($arg:tt)+) => {{
        let context = &$ctx;
        let span = $crate::logging::auto_queue::auto_queue_trace_span($action, context);
        let guard = span.enter();
        tracing::$level!($($arg)+);
        ::std::mem::drop(guard);
    }};
}
