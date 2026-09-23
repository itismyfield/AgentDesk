use super::*;
use crate::services::discord::gateway::{DiscordGateway, HeadlessGateway};

#[test]
fn worker_rest_transport_delivers_without_gateway_queue_chaining() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway = DiscordGateway::new(
        Arc::new(serenity::Http::new("test-token")),
        shared,
        ProviderKind::Codex,
        None,
    );
    assert!(!gateway.can_chain_locally());
    assert!(gateway.can_deliver_directly());
    assert!(!HeadlessGateway.can_chain_locally());
    assert!(!HeadlessGateway.can_deliver_directly());
}

#[tokio::test]
async fn rest_worker_terminal_edits_once_without_a_headless_duplicate() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    driver.gateway = Arc::new(DriverGateway {
        chain_locally: false,
        marker: driver.marker.clone(),
        observations: driver.observations.clone(),
        completed_publications: driver.completed_publications.clone(),
        published_bodies: driver.published_bodies.clone(),
        replace: ReplaceBehaviour::Edited,
        yields_per_call: 1,
    });
    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("REST terminal delivery must settle");
    assert!(output.terminal_delivery_committed);
    assert!(output.status_panel_terminal_committed);
    assert!(!output.preserve_inflight_for_cleanup_retry);
    assert_eq!(driver.completed_publications(), 1);
    assert_eq!(driver.publish_entries().len(), 1);
    assert!(
        driver
            .observations()
            .iter()
            .all(|o| o.call != DriverCall::Send)
    );
}
