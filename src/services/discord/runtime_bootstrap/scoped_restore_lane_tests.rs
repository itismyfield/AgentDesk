use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::discord::recovery_engine::install_inflight_scoped;

#[tokio::test]
async fn s3act_b2_settlement_is_not_installation_completion() {
    for observation in [
        InflightObservation::Settled {
            kind: NativeTerminalKind::Aborted,
        },
        InflightObservation::Retained {
            kind: NativeTerminalKind::Completed,
            reason: "delivery unconfirmed".into(),
        },
    ] {
        let lane = ScopedRestoreLane::new(5340301);
        let row = super::scoped_inflight_restore_tests::home(5340301, HomeState::Worker);
        let view = lane.clone();
        lane.run(
            RestoreScope::new(&row, "claude", "mini").unwrap(),
            1,
            move |mut attempt| async move {
                attempt.acknowledge(InstallStage::Queue)?;
                attempt.observe(observation);
                assert_eq!(
                    view.witness().status,
                    RestoreStatus::Restoring,
                    "settlement or retention never acknowledges installation"
                );
                assert!(
                    attempt.finish().is_err(),
                    "marker and persistence acknowledgements are required"
                );
                Ok(())
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(lane.witness().status, RestoreStatus::Blocked(_)));
    }
}

#[tokio::test]
async fn s3act_b2_owned_task_serializes_reentry_and_fresh_empty_witness() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 5340311;
    let lane = ScopedRestoreLane::new(channel);
    assert_eq!(lane.witness().status, RestoreStatus::Pending);
    let row = super::scoped_inflight_restore_tests::home(channel, HomeState::Worker);
    let scope = RestoreScope::new(&row, "claude", "mini").unwrap();
    let old_ready = Arc::new(tokio::sync::Notify::new());
    let release_old = Arc::new(tokio::sync::Notify::new());
    let new_ready = Arc::new(tokio::sync::Notify::new());
    let release_new = Arc::new(tokio::sync::Notify::new());
    let ready = old_ready.clone();
    let release = release_old.clone();
    let first = lane.run(scope.clone(), 1, move |mut attempt| async move {
        attempt.acknowledge(InstallStage::Queue)?;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("test-token"));
        let installed =
            install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &attempt.context)
                .await?;
        attempt.observe(installed.observation);
        ready.notify_one();
        release.notified().await;
        for stage in [
            InstallStage::Inflight,
            InstallStage::Marker,
            InstallStage::Placeholder,
        ] {
            attempt.acknowledge(stage)?;
        }
        attempt.finish()?;
        Ok(())
    });
    old_ready.notified().await;
    drop(first); // The caller leaves; its owned task still holds the channel reservation.
    let ready = new_ready.clone();
    let release = release_new.clone();
    let second = lane.run(scope.clone(), 2, move |mut attempt| async move {
        attempt.acknowledge(InstallStage::Queue)?;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("test-token"));
        let installed =
            install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &attempt.context)
                .await?;
        assert_eq!(
            installed.observation,
            InflightObservation::Empty,
            "empty completion must be explicit"
        );
        attempt.observe(installed.observation);
        ready.notify_one();
        release.notified().await;
        for stage in [
            InstallStage::Inflight,
            InstallStage::Marker,
            InstallStage::Placeholder,
        ] {
            attempt.acknowledge(stage)?;
        }
        attempt.finish()?;
        Ok(())
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(40), new_ready.notified())
            .await
            .is_err()
    );
    assert_eq!(
        (lane.witness().generation, lane.witness().status),
        (1, RestoreStatus::Restoring)
    );
    release_old.notify_one();
    new_ready.notified().await;
    assert_eq!(
        (lane.witness().generation, lane.witness().status),
        (2, RestoreStatus::Restoring),
        "new generation never inherits the old completion"
    );
    release_new.notify_one();
    second.await.unwrap().unwrap();
    assert_eq!(
        (lane.witness().generation, lane.witness().status),
        (2, RestoreStatus::Restored)
    );
    let before = lane.witness();
    let stale = lane
        .run(scope, 1, |_| async {
            panic!("stale work must not execute")
        })
        .await
        .unwrap();
    assert!(stale.is_err());
    assert_eq!(lane.witness(), before);
}

#[tokio::test]
async fn s3act_b2_installation_stages_are_ordered() {
    let lane = ScopedRestoreLane::new(5340321);
    let row = super::scoped_inflight_restore_tests::home(5340321, HomeState::Worker);
    lane.run(
        RestoreScope::new(&row, "claude", "mini").unwrap(),
        1,
        |mut attempt| async move {
            attempt.acknowledge(InstallStage::Queue)?;
            assert!(attempt.acknowledge(InstallStage::Marker).is_err());
            assert!(attempt.finish().is_err());
            Ok(())
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(lane.witness().status, RestoreStatus::Blocked(_)));
}

#[test]
fn s3act_b2_scope_requires_local_identity_and_tracks_all_axes() {
    let mut row = super::scoped_inflight_restore_tests::home(5340331, HomeState::Worker);
    let scope = RestoreScope::new(&row, "claude", "mini").unwrap();
    assert!(scope.matches(&row));
    for axis in 0..6 {
        let mut changed = row.clone();
        match axis {
            0 => changed.channel_id = "5340332".into(),
            1 => changed.provider = "codex".into(),
            2 => changed.epoch += 1,
            3 => changed.state = HomeState::Reclaiming,
            4 => changed.holder = Some("other".into()),
            _ => changed.target = Some("other".into()),
        }
        assert!(!scope.matches(&changed), "identity axis {axis}");
    }
    row.holder = Some("other".into());
    row.target = Some("mini".into());
    assert!(
        RestoreScope::new(&row, "claude", "mini").is_err(),
        "target-only observer installs nothing"
    );
    row.state = HomeState::Released;
    row.holder = None;
    assert!(
        !RestoreScope::new(&row, "claude", "mini")
            .unwrap()
            .held_locally(),
        "Released is install-only"
    );
    row.channel_id = "0".into();
    assert!(RestoreScope::new(&row, "claude", "mini").is_err());
}
