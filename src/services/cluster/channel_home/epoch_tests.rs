//! The recovery and command permits' epoch check, taken under the gate's own local lock.

use super::*;

#[tokio::test(start_paused = true)]
async fn s3act_b3a2_epoch_check_and_permit_share_lock() {
    let home = HomeGate::new("5340530", "mini");
    home.confirm(
        &HeldHome::for_test("5340530", "mini", 1, HomeState::Worker),
        Instant::now(),
    )
    .unwrap();
    for recovery in [false, true] {
        let mut competed = false;
        let permit = home
            .admit_at_epoch_checked("claude", 1, recovery, |epoch| {
                assert_eq!(epoch, 1, "permit must name the checked epoch");
                assert_eq!(
                    home.local
                        .try_lock()
                        .err()
                        .map(|e| matches!(e, std::sync::TryLockError::WouldBlock)),
                    Some(true),
                    "mint callback must still hold the local lock"
                );
                competed = std::thread::scope(|scope| {
                    scope
                        .spawn(|| home.local.try_lock().is_err())
                        .join()
                        .unwrap()
                });
            })
            .unwrap();
        assert!(
            competed,
            "actual competing thread must encounter the held lock"
        );
        assert_eq!(home.commands_in_flight(), 1);
        drop(permit);
        assert_eq!(home.commands_in_flight(), 0);
    }
    home.confirm(
        &HeldHome::for_test("5340530", "mini", 2, HomeState::Worker),
        Instant::now(),
    )
    .unwrap();
    for recovery in [false, true] {
        assert!(home.admit_at_epoch("claude", 1, recovery).is_none());
    }
    home.close_intake();
    assert!(home.admit_at_epoch("claude", 2, false).is_none());
    assert!(home.admit_at_epoch("claude", 2, true).is_some());
    home.close();
    assert!(home.admit_at_epoch("claude", 2, true).is_none());
}
