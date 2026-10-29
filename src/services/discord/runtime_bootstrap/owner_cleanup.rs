//! Local control work is independent of provider execution and the gateway role.
use super::*;
use crate::services::cluster::intake_router_hook::owner_record::cleanup;
use std::collections::HashSet;
use std::sync::Mutex;

pub(super) fn spawn(shared: &Arc<SharedData>, provider: &ProviderKind) {
    let Some(pool) = shared.pg_pool.clone() else {
        return;
    };
    let shared = shared.clone();
    let provider = provider.clone();
    let running = Arc::new(Mutex::new(HashSet::new()));
    crate::services::resident_loops::spawn_resident(
        format!("{}:owner_cleanup", provider.as_str()),
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                publish_routing_owner(&shared, &provider).await;
                if let Err(error) = cleanup::advance_pending(&pool, provider.as_str()).await {
                    tracing::warn!(%error,"stored owner transition remains pending");
                }
                super::owner_execution::refresh(&provider, Some(&pool)).await;
                if let Some(config) = crate::config_live_reload::current() {
                    super::o_writer_host::spawn_execution(
                        &shared,
                        &provider,
                        config.tui_o.as_ref(),
                    );
                }
                let configured = crate::services::tui_o::cutover::boot_snapshot()
                    .and_then(|s| s.configured_id().map(str::to_owned));
                let Ok(local) =
                    super::o_writer_host::local_id(configured.as_deref(), Duration::from_secs(10))
                        .await
                else {
                    continue;
                };
                let rows = match tokio::time::timeout(
                    Duration::from_secs(3),
                    cleanup::due(&pool, provider.as_str(), &local),
                )
                .await
                {
                    Ok(Ok(rows)) => rows,
                    error => {
                        tracing::warn!(?error, "owner cleanup read deferred");
                        continue;
                    }
                };
                for row in rows {
                    if !running
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(row.id)
                    {
                        continue;
                    }
                    let pool = pool.clone();
                    let shared = shared.clone();
                    let running = running.clone();
                    tokio::spawn(async move {
                        struct Running(Arc<Mutex<HashSet<i64>>>, i64);
                        impl Drop for Running {
                            fn drop(&mut self) {
                                self.0
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .remove(&self.1);
                            }
                        }
                        let _running = Running(running, row.id);
                        let result = reconcile_one(&pool, &shared, &row).await;
                        if let Err(error) = result {
                            tracing::warn!(%error,operation=%row.routing_cleanup.operation,"owner cleanup retry retained");
                        }
                    });
                }
            }
        },
    );
}

/// This runtime vouches only for itself: control is this loop, REST is its own prepared client.
pub(super) async fn publish_routing_owner(shared: &SharedData, provider: &ProviderKind) {
    #[cfg(test)]
    let rest = if cleanup::tests::mutant("rest-token") {
        shared.http.cached_bot_token.get().is_some()
    } else {
        super::o_writer_host::prepare_owner_rest(shared).await
    };
    #[cfg(not(test))]
    let rest = super::o_writer_host::prepare_owner_rest(shared).await;
    crate::services::cluster::intake_worker_capabilities::publish_routing_owner(
        provider.as_str(),
        rest,
    );
}

async fn reconcile_one(
    pool: &sqlx::PgPool,
    shared: &Arc<SharedData>,
    row: &cleanup::Record,
) -> Result<(), sqlx::Error> {
    #[cfg(test)]
    if cleanup::tests::mutant("periodic")
        || (cleanup::tests::mutant("retry-skip") && row.routing_cleanup_attempts > 0)
    {
        return Ok(());
    }
    cleanup::advance(pool, row.id).await?;
    let Some(claimed) = cleanup::claim(pool, row).await? else {
        return Ok(());
    };
    let response = super::super::commands::control::execution_cleanup::run(shared, &claimed).await;
    cleanup::respond(pool, &claimed, response).await?;
    cleanup::advance(pool, claimed.id).await?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::Fixture;
    use crate::services::tui_o::{channel_policy::BootChannels, cutover};
    #[tokio::test]
    async fn owner_cleanup_actual_periodic_pass_waits_busy_and_resumes_without_input_pg() {
        if !cutover::test_override::isolated_binding_case(concat!(
            module_path!(),
            "::owner_cleanup_actual_periodic_pass_waits_busy_and_resumes_without_input_pg"
        )) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let channel = 73369320;
        let binding =
            super::super::super::commands::control::execution_cleanup::tests::native(channel);
        let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
            binding["owner"].as_str().unwrap(),
        );
        fake.fail_kills();
        let fx = Fixture::with_native(channel, Some(binding)).await;
        let config:crate::config::Config=serde_json::from_value(serde_json::json!({"server":{},"cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-b"},"agents":[{"id":"custody-agent","name":"owner","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],"tui_o":{"writer":{"channels":[channel]}}})).unwrap();
        let _selection =
            cutover::test_override::force_boot(BootChannels::validate(&config).unwrap());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            fx.pool.clone(),
        ));
        let pin = shared
            .session_transition_lock(ChannelId::new(channel))
            .lock_owned()
            .await;
        spawn(&shared, &ProviderKind::Claude);
        tokio::time::sleep(Duration::from_secs(15)).await;
        let busy = fx.row().await;
        assert!(matches!(
            busy.routing_cleanup_state.as_str(),
            "busy" | "running"
        ));
        assert!(busy.routing_cleanup_seen.is_some());
        let active:String=sqlx::query_scalar("SELECT owner_instance_id FROM intake_session_owners WHERE provider='claude' AND raw_channel_id=$1 AND status='active'").bind(channel.to_string()).fetch_one(&fx.pool).await.unwrap();
        assert_eq!(
            active, "mac-a",
            "a responsive Busy operation must outlive the control deadline"
        );
        drop(pin);
        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = fx.row().await;
                if row.routing_cleanup_attempts > 0
                    && matches!(row.routing_cleanup_state.as_str(), "retry" | "deferred")
                {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the first physical normal end must fail without claiming success");
        assert_ne!(failed.routing_cleanup_state, "ended");
        let owner = failed.routing_cleanup.native.as_ref().unwrap()["owner"]
            .as_str()
            .unwrap();
        let recovered =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(owner);
        recovered.become_missing_after_kill();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if fx.row().await.routing_cleanup_state == "ended" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the resident loop must retry the failed original operation without new input");
        let ended = fx.row().await;
        assert_eq!(
            ended.routing_cleanup.operation,
            failed.routing_cleanup.operation
        );
        assert!(ended.routing_cleanup_attempts > failed.routing_cleanup_attempts);
        assert!(
            recovered
                .take_calls()
                .iter()
                .any(|call| call.starts_with("kill-session"))
        );
        tokio::time::timeout(Duration::from_secs(10),async{
            loop{
                let target:Option<String>=sqlx::query_scalar("SELECT target_instance_id FROM intake_outbox WHERE channel_id=$1 AND routing_state='admitted' AND status='pending'").bind(channel.to_string()).fetch_optional(&fx.pool).await.unwrap();
                if target.as_deref()==Some("mac-b"){break}
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await.expect("periodic pass must release the durable input without a callback or fresh message");
        assert_eq!(
            fx.row().await.routing_cleanup_state,
            "ended",
            "only the confirmed normal end may report success"
        );
        let row = crate::db::intake_outbox::claim_pending_for_target_except(
            &fx.pool,
            "mac-b",
            "claude",
            "periodic-fixture",
            &[],
        )
        .await
        .unwrap()
        .unwrap();
        crate::db::intake_outbox::mark_accepted(&fx.pool, row.id, "periodic-fixture")
            .await
            .unwrap();
        crate::db::intake_outbox::mark_spawned(&fx.pool, row.id, "periodic-fixture")
            .await
            .unwrap();
        let binding = serde_json::json!({"native":"fresh-on-peer"});
        let permit=crate::services::cluster::intake_router_hook::owner_record::input_start::prepare(&fx.pool,crate::services::cluster::intake_router_hook::owner_record::input_start::StartRequest{delivery_id:row.id,executor:"mac-b",claim_owner:"periodic-fixture",incarnation:"periodic-runtime",frame:"new source",native_binding:&binding}).await.unwrap();
        let count = std::sync::atomic::AtomicUsize::new(0);
        permit
            .submit("new source", &binding, |_| async {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .await
            .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn owner_cleanup_healthy_peer_advances_without_old_end_ack_pg() {
        if !cutover::test_override::isolated_binding_case(concat!(
            module_path!(),
            "::owner_cleanup_healthy_peer_advances_without_old_end_ack_pg"
        )) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let channel = 73369321;
        let fx = Fixture::with_native(channel, None).await;
        let config:crate::config::Config=serde_json::from_value(serde_json::json!({"server":{},"cluster":{"enabled":true,"instance_id":"mac-b","gateway_preferred_instance_id":"mac-a"},"agents":[{"id":"custody-agent","name":"owner","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],"tui_o":{"writer":{"channels":[channel]}}})).unwrap();
        let _selection =
            cutover::test_override::force_boot(BootChannels::validate(&config).unwrap());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            fx.pool.clone(),
        ));
        let row = fx.row().await;
        sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE id=$1").bind(row.id).execute(&fx.pool).await.unwrap();
        spawn(&shared, &ProviderKind::Claude);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if cutover::boot_snapshot()
                    .unwrap()
                    .execution(channel)
                    .is_some_and(|view| view.permit.is_ok_and(|permit| permit.executor == "mac-b"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("healthy peer must acquire execution without old Mac's worker or end ACK");
        let old = fx.row().await;
        assert_eq!(old.routing_cleanup_state, "pending");
        let deferred: bool = sqlx::query_scalar(
            "SELECT routing_cleanup_deferred FROM intake_session_owners WHERE id=$1",
        )
        .bind(old.id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert!(deferred);
        assert!(
            cleanup::due(&fx.pool, "claude", "mac-b")
                .await
                .unwrap()
                .is_empty(),
            "healthy peer must not obtain physical end work for the other Mac"
        );
    }
}
