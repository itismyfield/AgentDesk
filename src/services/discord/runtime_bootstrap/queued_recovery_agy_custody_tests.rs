//! A worker boot, reaper first, keeps AGY custody rows in place through every restore step.

use super::*;
use crate::services::discord::health::test_support::age_file;
use crate::services::discord::inflight::agy_end_authority_tests::{row, write};

// B1, B2: rows written before the reaper; the reaper runs and ends only the Legacy twin.
#[tokio::test(flavor = "current_thread")]
async fn worker_boot_keeps_custody_rows_from_reaper_to_restore() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Antigravity;
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let mut old_generation = row(6_325_610_023, true);
    old_generation.restart_generation = Some(1);
    let custody = [
        row(6_325_610_021, true),
        old_generation,
        row(6_325_610_024, true),
    ];
    let custody: Vec<_> = custody
        .iter()
        .map(|state| {
            let path = write(&root, state);
            (path.clone(), std::fs::read(path).unwrap())
        })
        .collect();
    age_file(&custody[0].0, 3_600);
    let stale_legacy = write(&root, &row(6_325_610_022, false));
    age_file(&stale_legacy, 3_600);
    let kept = || {
        custody
            .iter()
            .all(|(path, bytes)| std::fs::read(path).ok().as_ref() == Some(bytes))
    };

    let shared = crate::services::discord::make_shared_data_for_tests();
    let mock = crate::services::discord::shared_state::test_rest::recording_mock;
    let (_requests, _rest) = mock(6_325_610_900, 6_325_610_021).await;
    assert!(prepare_native_boot(&shared, &provider).await);
    assert!(!stale_legacy.exists(), "the reaper ran");
    assert!(kept());
    restore_worker_queues(&shared, &provider).await;
    assert!(kept());
}
