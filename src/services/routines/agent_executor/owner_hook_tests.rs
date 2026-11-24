//! Test hooks the routine executor reads; ROOT fixtures install their values.
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

/// A start the fixture already reserved: only the reserved turn id and its channel.
#[derive(Clone, Debug)]
pub(crate) struct ReservedStart {
    pub(crate) turn_id: String,
    pub(crate) channel: u64,
}

tokio::task_local! {
    /// Scripted starts in order: `None` refuses before admission, `Some` is handed to the owner.
    pub(crate) static STARTS: RefCell<VecDeque<Option<ReservedStart>>>;
    pub(crate) static OWNER: crate::services::turn_orchestrator::ChannelMailboxHandle;
    pub(crate) static LEGACY_TEARDOWNS: Cell<usize>;
}

/// The next scripted start, outside a `STARTS` scope `None` so the real launch runs.
pub(super) async fn scripted_start(
    executor: &RoutineAgentExecutor,
    store: &RoutineStore,
    start: &start_deferral::PendingAgentStart<'_>,
) -> Option<Result<StartedAgentTurn>> {
    let step = STARTS
        .try_with(|steps| {
            steps
                .borrow_mut()
                .pop_front()
                .expect("unexpected readmission")
        })
        .ok()?;
    let Some(ReservedStart { turn_id, channel }) = step else {
        return Some(Err(anyhow!("pre-admission refusal")));
    };
    let claimed = start.claimed;
    let channel = channel.to_string();
    let planned = json!({"status":"started","turn_id":turn_id,"run_id":claimed.run_id,"routine_id":claimed.routine_id,
        "agent_id":start.agent_id,"attempt_kind":start.attempt_kind,"provider":"claude","execution_mode":start.execution_mode.as_str(),
        "channel_id":channel,"parent_channel_id":channel,"discord_thread_id":claimed.discord_thread_id,"dm_user_id":start.dm_user_id,
        "is_dm":start.dm_user_id.is_some(),"prompt":start.prompt,"checkpoint":start.checkpoint,
        "next_due_at":start.next_due_at,"script_ref":claimed.script_ref,"fresh_context_guaranteed":false});
    let owned = owner_input::custody::owned(executor, store, start, &planned).await;
    Some(owned.and_then(|started| started.ok_or_else(|| anyhow!("owner left the start"))))
}
