use std::path::Path;
use std::time::Duration;

use tokio::time::{Instant, advance};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{ForceOutcome, HomeState};

const C: &str = "1490141479707086938";

fn written(holder: &str, epoch: i64, state: HomeState) -> HeldHome {
    HeldHome::for_test(C, holder, epoch, state)
}

fn applied(
    holder: &str,
    epoch: i64,
    state: HomeState,
) -> impl Future<Output = Result<HomeWrite<HeldHome>, HomeError>> {
    std::future::ready(Ok(HomeWrite::Applied(written(holder, epoch, state))))
}

fn owned(home_epoch: i64, gate_epoch: u64, intake: HomeIntake) -> HomeOwnership {
    HomeOwnership::Owned {
        home_epoch,
        gate_epoch,
        intake,
    }
}

fn admitted(home: &HomeGate) -> (Option<i64>, Option<u64>) {
    (home.admit(|epoch| epoch), home.gate().admit(|epoch| epoch))
}

#[tokio::test(start_paused = true)]
async fn a_renewal_that_stops_landing_closes_the_home_h_after_its_last_send() {
    let home = HomeGate::new(C, "mini");
    let t0 = Instant::now();
    let round = lease_round(&home, 2, t0, applied("mini", 2, HomeState::Worker)).await;
    let LeaseRound::Renewed(ownership) = round else {
        panic!("expected renewed: {round:?}");
    };
    assert_eq!(ownership, owned(2, 1, HomeIntake::Open));
    assert_eq!(admitted(&home), (Some(2), Some(1)));
    let round = lease_round(&home, 2, t0, applied("gw", 2, HomeState::Worker)).await;
    let LeaseRound::Refused(refused) = round else {
        panic!("expected refused: {round:?}");
    };
    assert_eq!(refused, ConfirmRefused::Foreign);

    advance(RENEW_EVERY).await;
    let failure = std::future::ready(Err(HomeError::Db(sqlx::Error::PoolTimedOut)));
    let round = lease_round(&home, 2, Instant::now(), failure).await;
    assert!(
        matches!(round, LeaseRound::Failed(HomeError::Db(_))),
        "{round:?}"
    );
    advance(HOLD_FOR - RENEW_EVERY - Duration::from_millis(1)).await;
    assert_eq!(home.ownership(), owned(2, 1, HomeIntake::Open));

    // A renewal still pending at the deadline does not keep the gate open.
    let hung = std::future::pending();
    let round = lease_round(&home, 2, Instant::now(), hung).await;
    assert!(matches!(round, LeaseRound::Expired), "{round:?}");
    assert_eq!(Instant::now() - t0, HOLD_FOR);
    assert_eq!(home.ownership(), HomeOwnership::Lost);
    assert_eq!(admitted(&home), (None, None));

    // A result sent before the lapse never reopens; a renewal sent after it does.
    advance(Duration::from_secs(1)).await;
    let late = home.confirm(&written("mini", 2, HomeState::Worker), t0 + RENEW_EVERY);
    assert_eq!(late, Err(ConfirmRefused::Late));
    let stale_send = Instant::now() - HOLD_FOR;
    let late = home.confirm(&written("mini", 2, HomeState::Worker), stale_send);
    assert_eq!(late, Err(ConfirmRefused::Late));
    let reopened = home.confirm(&written("mini", 2, HomeState::Worker), Instant::now());
    assert_eq!(reopened, Ok(owned(2, 2, HomeIntake::Open)));
    assert_eq!(admitted(&home), (Some(2), Some(2)));
}

#[tokio::test(start_paused = true)]
async fn only_this_holders_current_renewals_keep_the_home() {
    let home = HomeGate::new(C, "mini");
    let now = Instant::now();
    let foreign = [
        written("gw", 3, HomeState::Worker),
        HeldHome::for_test("9", "mini", 3, HomeState::Worker),
    ];
    for write in &foreign {
        assert_eq!(home.confirm(write, now), Err(ConfirmRefused::Foreign));
    }
    assert_eq!(home.ownership(), HomeOwnership::Lost);

    let opened = home.confirm(&written("mini", 3, HomeState::Worker), now);
    assert_eq!(opened, Ok(owned(3, 1, HomeIntake::Open)));
    let older = home.confirm(&written("mini", 2, HomeState::Worker), now);
    assert_eq!(older, Err(ConfirmRefused::Superseded));
    home.renewal_stale(2);
    assert_eq!(home.ownership(), owned(3, 1, HomeIntake::Open));
    home.renewal_stale(3);
    assert_eq!(home.ownership(), HomeOwnership::Lost);
    assert_eq!(admitted(&home), (None, None));

    advance(Duration::from_millis(1)).await;
    home.confirm(&written("mini", 4, HomeState::Worker), Instant::now())
        .expect("new epoch opens");
    home.close();
    advance(Duration::from_millis(1)).await;
    let retired = home.confirm(&written("mini", 4, HomeState::Worker), Instant::now());
    assert_eq!(retired, Err(ConfirmRefused::Retired));
    assert_eq!(admitted(&home), (None, None));
    let next = home.confirm(&written("mini", 5, HomeState::Worker), Instant::now());
    assert_eq!(next, Ok(owned(5, 3, HomeIntake::Open)));
}

#[tokio::test(start_paused = true)]
async fn a_draining_home_admits_owed_pieces_but_never_reopens_intake_in_its_epoch() {
    let home = HomeGate::new(C, "mini");
    home.confirm(&written("mini", 2, HomeState::Worker), Instant::now())
        .expect("opens");
    home.close_intake();
    assert_eq!(home.ownership(), owned(2, 1, HomeIntake::Closed));
    assert_eq!(admitted(&home), (Some(2), Some(1)));
    // A worker-state renewal that committed before the reclaim lands after the drain began.
    let renewed = home.confirm(&written("mini", 2, HomeState::Worker), Instant::now());
    assert_eq!(renewed, Ok(owned(2, 1, HomeIntake::Closed)));

    let gateway = HomeGate::new(C, "gw");
    let releasing = gateway.confirm(&written("gw", 1, HomeState::Releasing), Instant::now());
    assert_eq!(releasing, Ok(owned(1, 1, HomeIntake::Closed)));
    assert_eq!(gateway.admit(|epoch| epoch), Some(1));
}

/// Q2 link and the boot read: a row naming this node opens nothing until its own renewal
/// write lands, and the opened gate carries the row epoch, not the local acquisition count.
#[tokio::test]
async fn a_read_never_opens_the_home_and_force_leaves_nobody_holding_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let apply = |write: Result<HomeWrite<ChannelHome>, HomeError>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("expected applied: {other:?}"),
    };
    apply(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await);
    apply(o_channel_homes::finish_release(&pool, C, "gw", 1).await);
    apply(o_channel_homes::adopt(&pool, C, "mini", 2).await);

    let home = Arc::new(HomeGate::new(C, "mini"));
    let boot = boot_home(&pool, &home).await;
    assert!(matches!(boot, BootHome::Row(_)), "{boot:?}");
    assert_eq!(boot.lease_epoch("mini"), Some(2));
    assert_eq!(home.ownership(), HomeOwnership::Lost);
    assert_eq!(admitted(&home), (None, None));

    let lease = tokio::spawn(run_lease(pool.clone(), Arc::clone(&home), 2));
    for _ in 0..250 {
        if home.ownership() != HomeOwnership::Lost {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(home.ownership(), owned(2, 1, HomeIntake::Open));
    assert_eq!(admitted(&home), (Some(2), Some(1)));

    sqlx::query("UPDATE o_channel_homes SET renewed_at = NOW() - INTERVAL '250 seconds'")
        .execute(&pool)
        .await
        .expect("age lease");
    let force = o_channel_homes::force_orphan(&pool, C, 2, FORCE_AFTER, "op").await;
    assert!(matches!(force, Ok(ForceOutcome::Orphaned(_))), "{force:?}");
    // The lease loop's next renewal finds the row gone from it and ends with the gate closed.
    let ended = tokio::time::timeout(RENEW_EVERY * 3, lease).await;
    assert!(matches!(ended, Ok(Ok(()))), "lease loop kept running");
    assert_eq!(admitted(&home), (None, None));
    // The orphaned row opens no gate anywhere: not the target, not at the forced epoch.
    for (holder, epoch) in [("mini", 3), ("gw", 3), ("gw", 2)] {
        let other = HomeGate::new(C, holder);
        let boot = boot_home(&pool, &other).await;
        assert_eq!(boot.lease_epoch(holder), None);
        let renewal = o_channel_homes::renew(&pool, C, holder, epoch);
        let round = lease_round(&other, epoch, Instant::now(), renewal).await;
        assert!(matches!(round, LeaseRound::Stale), "{round:?}");
        assert_eq!(admitted(&other), (None, None));
    }

    // A failed renewal keeps the hold only until H past its last landed send.
    let held = HomeGate::new(C, "gw");
    sqlx::query("DELETE FROM o_channel_homes")
        .execute(&pool)
        .await
        .expect("clear");
    apply(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await);
    let renewal = o_channel_homes::renew(&pool, C, "gw", 1);
    let sent = Instant::now();
    let round = lease_round(&held, 1, sent, renewal).await;
    let LeaseRound::Renewed(ownership) = round else {
        panic!("expected renewed: {round:?}");
    };
    assert_eq!(ownership, owned(1, 1, HomeIntake::Closed));
    pool.close().await;
    let renewal = o_channel_homes::renew(&pool, C, "gw", 1);
    let round = lease_round(&held, 1, Instant::now(), renewal).await;
    assert!(
        matches!(round, LeaseRound::Failed(HomeError::Db(_))),
        "{round:?}"
    );
    let boot = boot_home(&pool, &held).await;
    assert!(
        matches!(boot, BootHome::Unreadable(HomeError::Db(_))),
        "{boot:?}"
    );
    assert_eq!(held.ownership(), owned(1, 1, HomeIntake::Closed));
    assert!(held.expire_if_due(sent + HOLD_FOR));
    assert_eq!(admitted(&held), (None, None));
    pg_db.drop().await;
}

/// Source text with every `#[cfg(test)]` item removed.
fn production_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..at]);
        let item = &rest[at..];
        let end = match (item.find(';'), item.find('{')) {
            (Some(semi), Some(open)) if semi < open => semi + 1,
            (_, Some(open)) => {
                let mut depth = 0usize;
                let close = item[open..].char_indices().find_map(|(offset, ch)| {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(open + offset)
                });
                close.map_or(item.len(), |close| close + 1)
            }
            (Some(semi), None) => semi + 1,
            (None, None) => item.len(),
        };
        rest = &item[end..];
    }
    out.push_str(rest);
    out
}

// Dormant guard: no production code outside the two owners names the home table or gate,
// and the owners never start the lease loop.
#[test]
fn channel_home_items_have_no_production_caller() {
    const OWNERS: &[&str] = &[
        "src/db/o_channel_homes.rs",
        "src/services/cluster/channel_home.rs",
    ];
    const REGISTRATIONS: &[(&str, &str)] = &[
        ("src/db/mod.rs", "pub(crate) mod o_channel_homes;"),
        (
            "src/services/cluster/mod.rs",
            "pub(crate) mod channel_home;",
        ),
    ];
    const NEEDLES: &[&str] = &[
        "o_channel_homes",
        "channel_home",
        "HomeGate",
        "HeldHome",
        "boot_home",
        "lease_round",
        "run_lease",
    ];
    let probe = production_text("fn a() {}\n#[cfg(test)]\nmod t { fn b() { c(); } }\nfn d() {}");
    assert!(probe.contains("fn a()") && probe.contains("fn d()") && !probe.contains("fn b()"));

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![root.join("src")];
    let mut violations = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("source dir") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.ends_with(".rs") || name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("source file");
            let mut prod = production_text(&text);
            if OWNERS.contains(&relative.as_str()) {
                let starts = prod.matches("run_lease(").count();
                if relative.ends_with("channel_home.rs") && starts != 1 {
                    violations.push(format!(
                        "{relative}: run_lease( x{starts}, only its definition"
                    ));
                }
                continue;
            }
            if let Some((_, line)) = REGISTRATIONS.iter().find(|(file, _)| *file == relative) {
                prod = prod.replacen(line, "", 1);
            }
            violations.extend(
                NEEDLES
                    .iter()
                    .filter(|needle| prod.contains(**needle))
                    .map(|needle| format!("{relative}: {needle}")),
            );
        }
    }
    assert!(
        violations.is_empty(),
        "channel home production caller: {violations:?}"
    );
}
