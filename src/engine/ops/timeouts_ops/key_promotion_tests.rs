//! Session-kill and repair test on the DL key-promotion rig.
use rquickjs::Object;
use serde_json::Value;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_promotion_current_primary_policy_kill_and_repair_have_effect_pg() {
    // A policy holding the current primary or the kept alias acts on the one live session.
    for (channel, alias) in [(81190305, false), (81190315, true)] {
        let rig =
            crate::services::discord::session_locator::tests::PromotionRig::new(channel).await;
        let before = rig.queue().await;
        let asked = rig.asked(alias).to_string();
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        let result: Value = context.with(|ctx| {
            ctx.globals()
                .set("agentdesk", Object::new(ctx.clone()).unwrap())
                .unwrap();
            crate::engine::ops::exec_ops::register_exec_ops(&ctx).unwrap();
            crate::engine::ops::exec_ops::register_session_command_ops_for_test(
                &ctx,
                Some(rig.pool.clone()),
                crate::engine::ops::exec_ops::SystemTmux,
            )
            .unwrap();
            let call = format!(
                "agentdesk.session.kill({})",
                serde_json::to_string(&asked).unwrap()
            );
            serde_json::from_str(&ctx.eval::<String, _>(call).unwrap()).unwrap()
        });
        assert!(
            rig.others().is_empty(),
            "alias={alias}: policy kill must end the new physical session: {result}"
        );
        let repaired = crate::engine::ops::timeouts_ops::host_repair::repair_stale_session_pg(
            &rig.pool,
            &asked,
            crate::engine::ops::timeouts_ops::host_repair::RepairRequest {
                session_id: rig.id,
                active_dispatch_id: None,
                active_turn_nonce: Some("new".into()),
                observed: "dead".into(),
                fail_dispatch: false,
                fail_reason: "isolated policy repair".into(),
                clear_active_dispatch_id: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(repaired["repaired"], true, "alias={alias}");
        rig.assert_queue(&before).await;
        rig.finish().await;
    }
}
