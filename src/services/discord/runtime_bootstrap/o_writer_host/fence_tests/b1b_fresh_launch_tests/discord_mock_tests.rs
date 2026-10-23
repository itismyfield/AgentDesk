use super::*;

#[tokio::test]
async fn discord_mock_ignores_foreign_gets_around_history() {
    let output = Discord::start().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let history = format!("/api/v10/channels/{CANARY_CHANNEL}/messages");
    let foreign = [
        "/".to_owned(),
        "/api/v10/channels/1".to_owned(),
        "/api/v10/channels/1/pins".to_owned(),
        "/api/v10/channels//messages".to_owned(),
        "/api/v10/channels/not-a-channel/messages".to_owned(),
        "/api/v10/channels/1/threads/2/messages".to_owned(),
        "/channels/1/messages".to_owned(),
        format!("{history}/900001"),
        format!("{history}/"),
    ];
    for expected in [0, 1] {
        for path in &foreign {
            let response = client
                .get(format!("{}{path}", output.url))
                .send()
                .await
                .unwrap();
            assert_eq!(
                output.history_reads.load(Ordering::SeqCst),
                expected,
                "foreign GET {path} must not count as history"
            );
            assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        }
        output.http.get_current_user().await.unwrap();
        output.assert_history_reads(expected);
        if expected == 0 {
            client
                .post(format!("{}{history}", output.url))
                .json(&json!({"content": FIRST_REPLY}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            output.assert_history_reads(0);
            let messages = output
                .http
                .get_messages(serenity::ChannelId::new(CANARY_CHANNEL), None, Some(100))
                .await
                .unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content, FIRST_REPLY);
            output.assert_history_reads(1);
        }
    }
}

#[tokio::test]
async fn discord_mock_duplicate_history_fails_the_restart_assertion() {
    let output = Discord::start().await;
    for channel in [CANARY_CHANNEL, CANARY_CHANNEL + 1] {
        let messages = output
            .http
            .get_messages(serenity::ChannelId::new(channel), None, Some(1))
            .await
            .unwrap();
        assert!(messages.is_empty());
    }
    assert_eq!(
        output.history_reads.load(Ordering::SeqCst),
        2,
        "every real history GET must be counted"
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        output.assert_history_reads(1);
    }))
    .expect_err("two history reads must fail the restart's one-read assertion");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("only the open Prepared performs one history read-back"));
}
