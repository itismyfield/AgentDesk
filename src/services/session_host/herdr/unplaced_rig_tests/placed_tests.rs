use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn s2b1_placed_command_reply_loss_adopts_the_provider_pg() {
    let rig = Rig::new().await;
    let host = rig.host();
    rig.state().unconfirmed_command = Some(true);
    rig.launch(&host).await.unwrap();
    let pending = rig.record().await;
    assert!(pending.location.is_some());
    assert!(pending.expected.is_none(), "command reply is unconfirmed");
    assert_eq!(
        rig.resolve(&host, &pending.execution_nonce).await,
        Resolution::Adopted
    );
    let adopted = rig.record().await;
    assert_eq!(adopted.location, pending.location);
    assert!(adopted.expected.is_some());
    assert_eq!(rig.state().creates, 1);
    assert!(rig.state().removed.is_empty());
    rig.end().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s2b1_placed_shell_is_disposed_only_while_pristine_pg() {
    for touched in [true, false] {
        let rig = Rig::new().await;
        let host = rig.host();
        rig.state().unconfirmed_command = Some(false);
        rig.launch(&host).await.unwrap();
        let pending = rig.record().await;
        assert!(pending.expected.is_none());
        let pane = pending.location.as_ref().unwrap().pane_id.clone();
        if touched {
            rig.state().split(&pane);
        }
        let result = rig.resolve(&host, &pending.execution_nonce).await;
        if touched {
            assert!(matches!(result, Resolution::Held(_)), "{result:?}");
            unchanged(&rig, &pending, "additional pane").await;
        } else {
            assert_eq!(result, Resolution::Retired);
            assert_eq!(rig.state().removed, vec![pane]);
            assert_eq!(rig.record().await.state, HostedState::Retired);
            let successor = rig.launch(&host).await.unwrap();
            assert!(matches!(
                successor,
                HerdrLaunchOutcome::Launched {
                    evidence: Ok(()),
                    ..
                }
            ));
            assert_eq!(rig.state().workspaces.len(), 1);
        }
        rig.end().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn s2b1_restart_retires_a_placed_pane_after_an_applied_close_pg() {
    for ack_lost in [false, true] {
        let rig = Rig::new().await;
        let host = rig.host();
        rig.state().unconfirmed_command = Some(false);
        rig.launch(&host).await.unwrap();
        let pending = rig.record().await;
        let pane = pending.location.as_ref().unwrap().pane_id.clone();
        let endpoint = launch_endpoint(&rig.server);
        let witness = host
            .unplaced_witness(&endpoint, &pending.execution_nonce)
            .unwrap();
        let read = host
            .channel_panes(&endpoint, &rig.owner.logical_key, &witness)
            .unwrap();
        let [read] = read.as_slice() else {
            panic!("one shell");
        };
        let crate::services::session_host::PaneOccupant::LoneShell { root, .. } = &read.occupant
        else {
            panic!("root shell");
        };
        rig.state().unconfirmed_close = ack_lost;
        host.dispose_residue(
            &endpoint,
            &pending.execution_nonce,
            &Residue {
                pane_id: pane.clone(),
                label: read.label.clone(),
                root: root.clone(),
                witness,
            },
        )
        .unwrap();
        assert!(rig.state().workspaces.is_empty());
        assert_eq!(rig.record().await, pending, "close precedes row retire");
        let mut socket_host = launch_host(&rig.server);
        socket_host.set_process_os_for_test(
            Box::new(RigOs(rig.herdr.clone())),
            Duration::from_millis(500),
        );
        let _boot = herdr_registry::force_for_test(registry(&rig).with_launch_host(socket_host));
        let boot = crate::services::claude::herdr_turn::boot_launch_host().unwrap();
        assert_eq!(
            rig.resolve(&boot, &pending.execution_nonce).await,
            Resolution::Retired
        );
        assert_eq!(rig.state().removed, vec![pane], "close was not re-sent");
        rig.end().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn s2b1_placed_missing_observation_cannot_retire_a_successor_pg() {
    let rig = Rig::new().await;
    let host = rig.host();
    rig.launch(&host).await.unwrap();
    let row = rig.record().await;
    assert!(row.expected.is_some());
    rig.state()
        .drop_pane(&row.location.as_ref().unwrap().pane_id);
    let mut successor = row.clone();
    successor.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    successor
        .source_ref
        .execution_nonce
        .clone_from(&successor.execution_nonce);
    successor.location = None;
    successor.expected = None;
    let replacement = serde_json::to_value(&successor).unwrap();
    let pool = rig.pool.clone();
    let runtime = tokio::runtime::Handle::current();
    rig.state().before_snapshot = Some(Box::new(move || {
        runtime.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                .bind(replacement)
                .bind(CHANNEL.to_string())
                .execute(&pool)
                .await
                .unwrap();
        });
    }));
    let resolution = rig.resolve(&host, &row.execution_nonce).await;
    assert!(matches!(resolution, Resolution::Held(_)), "{resolution:?}");
    assert_eq!(rig.record().await, successor, "stale snapshot ran its CAS");
    assert!(rig.state().removed.is_empty());
    rig.end().await;
}
