use super::BeforeEffects;
use crate::db::intake_outbox::IntakeOutboxRow;
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::cluster::intake_worker::WorkerScopeKey;

tokio::task_local! {
    static BEFORE_EFFECTS: std::cell::RefCell<Option<BeforeEffects>>;
}

pub(crate) fn take(key: WorkerScopeKey) -> Option<BeforeEffects> {
    let WorkerScopeKey { .. } = key;
    BEFORE_EFFECTS
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten()
}

pub(crate) async fn scope<T>(
    key: WorkerScopeKey,
    row: &IntakeOutboxRow,
    execution: impl std::future::Future<Output = T>,
) -> T {
    let WorkerScopeKey { .. } = key;
    let mut spawned = row.clone();
    spawned.status = IntakeOutboxStatus::Spawned;
    BEFORE_EFFECTS
        .scope(
            std::cell::RefCell::new(Some(BeforeEffects(spawned))),
            execution,
        )
        .await
}
