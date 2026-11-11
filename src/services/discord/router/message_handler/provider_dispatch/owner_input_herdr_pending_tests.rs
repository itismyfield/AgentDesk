//! Durable launch prerequisites for the owner's Codex Herdr first-input tests.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::HostedRecord;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

pub(super) fn recorded(owner: &HerdrOwner) -> (TestPostgresDb, sqlx::PgPool, HostedExecution) {
    prepare(owner, true)
}

fn prepare(owner: &HerdrOwner, persist: bool) -> (TestPostgresDb, sqlx::PgPool, HostedExecution) {
    let mut context: BindingContext =
        serde_json::from_slice(&std::fs::read(&owner.context).unwrap()).unwrap();
    context.provider = "codex".into();
    context.tmux_session = owner.logical.clone();
    context.channel_id = Some(owner.channel);
    let context = PreparedIncarnation::create(context).unwrap();
    owner.rig.run_provider(&context.path, false);
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&owner.logical, "spawn_nonce"),
        &owner.execution,
    )
    .unwrap();
    let mut record = owner.record("codex", HostedState::Pending);
    record.expected.as_mut().unwrap().binding_provider = "codex".into();
    let raw = serde_json::to_value(&record).unwrap();
    assert_eq!(
        HostedRecord::decode(Some(&raw)),
        HostedRecord::Known(record.clone())
    );
    let database = owner.runtime.block_on(TestPostgresDb::create());
    let pool = owner.runtime.block_on(database.connect_and_migrate());
    if persist {
        owner.runtime.block_on(sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)"
        )
        .bind(format!("codex/owned-herdr/{NODE}:{}", owner.logical))
        .bind("owned-herdr")
        .bind(owner.channel.to_string())
        .bind(raw)
        .execute(&pool)).unwrap();
    }
    (database, pool, record)
}

#[test]
fn missing_durable_pending_row_refuses_owned_first_input_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 7_272_614);
    let registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", codex_ready());
    let (database, pool, record) = prepare(&owner, false);
    let turn = owner.codex_turn(&pool);
    let entered = owner.runtime.enter();
    let waiting = owner.wait_for("codex", Lineage::Created);
    let result = crate::services::codex::herdr_turn::owned_first_prompt_for_tests(
        &turn,
        &record,
        Lineage::Created,
    );
    assert!(
        result
            .unwrap_err()
            .contains("Pending row unreadable before input")
    );
    assert!(owner.rig.sends().is_empty());
    assert!(crate::services::claude::herdr_turn::not_held(&owner.execution).is_ok());
    let snapshot = owner.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["retained_inputs"][0]["disposition"], "known_not_sent",
        "{snapshot}"
    );
    drop((waiting, entered));
    drop((pool, database, registry));
    drop(owner);
    drop(root);
}
