use super::*;
use crate::services::tui_o::writer::test_support::FooterWriter;

async fn worker_turn(writer: &mut FooterWriter, id: &str) -> String {
    let _root = fixture_runtime_root(writer.root());
    writer.turn(id).await
}

#[test]
fn usage_footer_slash_reaches_remote_o_writer_after_restart_pg() {
    if crate::db::postgres::postgres_test_database_url_base().is_none() {
        return;
    }
    isolated(async {
        use crate::db::auto_queue::test_support::TestPostgresDb;

        let db = TestPostgresDb::create().await;
        let gateway_pool = db.connect_and_migrate().await;
        let writer_pool = crate::db::postgres::connect_test_pool(&db.database_url, "footer writer")
            .await
            .unwrap();
        let rig = Rig::new().await;
        let mut data = data(ProviderKind::Claude);
        data.shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            gateway_pool.clone(),
        ));
        data.shared.settings.write().await.owner_user_id = Some(42);
        let channel = discord_mock::CHANNEL_ID;
        let mut worker = FooterWriter::new(channel, writer_pool.clone());
        let mut other = FooterWriter::new(channel + 1, writer_pool.clone());
        assert_ne!(
            std::env::var_os("AGENTDESK_ROOT_DIR").as_deref(),
            Some(worker.root().as_os_str())
        );

        let change = |action, expected| {
            let (rig, data) = (&rig, &data);
            async move {
                rig.forget();
                let result = rig.invoke_inner(data, "usage-footer", action, false).await;
                assert_eq!(result["allowed"], true);
                assert_eq!(result["replies"][0]["type"], 5);
                assert_eq!(result["replies"][0]["data"]["flags"], 64);
                let reply = result["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(reply["content"], expected);
            }
        };
        let off = "이 채널의 턴 푸터 쿼터 표시를 껐어요.";
        change(Some(("action", "off")), off).await;
        let first = worker_turn(&mut worker, "remote-off").await;
        assert_eq!(first.lines().count(), 1, "{first}");
        change(None, "이 채널의 턴 푸터 쿼터 표시를 켰어요.").await;
        let second = worker_turn(&mut worker, "remote-on").await;
        assert!(second.contains("\n-# 5h 24% ("), "{second}");
        change(Some(("action", "off")), off).await;
        worker.restart();
        let restarted = worker_turn(&mut worker, "restarted-off").await;
        assert_eq!(restarted.lines().count(), 1, "{restarted}");
        let untouched = worker_turn(&mut other, "other-channel").await;
        assert!(untouched.contains("\n-# 5h 24% ("), "{untouched}");

        let toggle = crate::services::discord::runtime_store::usage_footer::update;
        let (a, b) = tokio::join!(
            toggle(Some(&gateway_pool), channel + 1, None),
            toggle(Some(&writer_pool), channel + 1, None),
        );
        assert_ne!(a.unwrap(), b.unwrap());
        assert!(
            worker_turn(&mut other, "after-two-toggles")
                .await
                .contains("\n-# 5h 24% (")
        );
    });
}

#[test]
fn usage_footer_without_shared_storage_reports_failure() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let response = rig
            .invoke_inner(&data, "usage-footer", Some(("action", "off")), false)
            .await;
        assert_eq!(response["allowed"], true);
        assert_eq!(
            response["messages"][0]["content"],
            "공유 저장소에 설정을 저장하지 못했어요. 턴 푸터에 적용됐는지 확인할 수 없어요."
        );
    });
}
