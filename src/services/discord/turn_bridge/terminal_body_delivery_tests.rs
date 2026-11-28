use crate::services::discord::make_shared_data_for_tests;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::Arc;

#[tokio::test(flavor = "current_thread")]
async fn terminal_body_delivery_keeps_its_site_without_waiting_for_journal() {
    use crate::services::discord::outbound::raw_output::tests as observation;
    use observation::{CHANNEL, Mock, reply};
    if observation::isolated(
        concat!(
            module_path!(),
            "::terminal_body_delivery_keeps_its_site_without_waiting_for_journal"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let log = observation::journal().await;
    let mock = Mock::start(vec![
        observation::message(9901),
        reply("204 No Content", &[], ""),
    ])
    .await;
    let shared = make_shared_data_for_tests();
    let gateway =
        crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(
            Arc::new(observation::client(&mock)),
            shared.clone(),
        );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(log.parent().unwrap().join("writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    let delivery = crate::services::discord::turn_bridge::terminal_delivery::send_ordered_long_terminal_response(
        &shared,
        gateway.as_ref(),
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
        MessageId::new(9900),
        None,
        "provider answer",
        None,
        None,
        None,
    );
    tokio::pin!(delivery);
    let before = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut delivery).await;
    let completed = before.is_ok();
    let wire = mock.requests().len();
    drop(lock);
    let result = match before {
        Ok(result) => result,
        Err(_) => delivery.await,
    };
    assert!(result.is_ok());
    assert!(
        completed && wire == 2,
        "observation delayed delivery: completed={completed}, wire={wire}"
    );
    observation::flushed().await;
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &serde_json::Value| row["kind"] == "target")
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "one POST begin/result; cleanup DELETE is excluded"
    );
    for row in rows {
        assert_eq!(row["census"]["site"], "W10d");
    }
}
