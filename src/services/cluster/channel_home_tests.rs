use std::path::Path;
use std::time::Duration;

use tokio::time::{Instant, advance};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{ForceOutcome, ForceWindow, HomeState};

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
    let round = lease_round(&home, 2, Instant::now(), hung);
    let round = tokio::time::timeout(HOLD_FOR, round).await;
    let round = round.expect("the deadline ends a hung renewal");
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
    let force = o_channel_homes::force_orphan(&pool, C, 2, ForceWindow::MIN, "op").await;
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
    let epoch = apply(o_channel_homes::delegate(&pool, C, "claude", "gw", "mini").await).epoch;
    assert!(epoch > 3, "a re-delegation never repeats an epoch: {epoch}");
    let renewal = o_channel_homes::renew(&pool, C, "gw", epoch);
    let sent = Instant::now();
    let round = lease_round(&held, epoch, sent, renewal).await;
    let LeaseRound::Renewed(ownership) = round else {
        panic!("expected renewed: {round:?}");
    };
    assert_eq!(ownership, owned(epoch, 1, HomeIntake::Closed));
    pool.close().await;
    let renewal = o_channel_homes::renew(&pool, C, "gw", epoch);
    let round = lease_round(&held, epoch, Instant::now(), renewal).await;
    assert!(
        matches!(round, LeaseRound::Failed(HomeError::Db(_))),
        "{round:?}"
    );
    let boot = boot_home(&pool, &held).await;
    assert!(
        matches!(boot, BootHome::Unreadable(HomeError::Db(_))),
        "{boot:?}"
    );
    assert_eq!(held.ownership(), owned(epoch, 1, HomeIntake::Closed));
    assert!(held.expire_if_due(sent + HOLD_FOR));
    assert_eq!(admitted(&held), (None, None));
    pg_db.drop().await;
}

/// `text` with comment, string and char literal bodies blanked, byte offsets unchanged.
fn mask_literals(text: &str) -> Vec<u8> {
    let (b, mut out) = (text.as_bytes(), text.as_bytes().to_vec());
    let ident = |at: usize| at < b.len() && (b[at].is_ascii_alphanumeric() || b[at] == b'_');
    let mut blank = |from: usize, to: usize| out[from..to].iter_mut().for_each(|c| *c = b' ');
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        let end = match b[i] {
            b'/' if next == Some(b'/') => b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |k| i + k),
            b'/' if next == Some(b'*') => {
                let (mut depth, mut k) = (0usize, i);
                while k + 1 < b.len() {
                    match (b[k], b[k + 1]) {
                        (b'/', b'*') => (depth, k) = (depth + 1, k + 2),
                        (b'*', b'/') => (depth, k) = (depth - 1, k + 2),
                        _ => k += 1,
                    }
                    if depth == 0 {
                        break;
                    }
                }
                k
            }
            b'r' if !ident(i.wrapping_sub(1))
                || (i >= 1 && b[i - 1] == b'b' && !ident(i.wrapping_sub(2))) =>
            {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + 1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let body = i + 2 + hashes;
                text[body..]
                    .find(&close)
                    .map_or(b.len(), |k| body + k + close.len())
            }
            b'"' => {
                let mut k = i + 1;
                while k < b.len() && b[k] != b'"' {
                    k += if b[k] == b'\\' { 2 } else { 1 };
                }
                k + 1
            }
            b'\'' if next == Some(b'\\') => b[i + 2..]
                .iter()
                .position(|&c| c == b'\'')
                .map_or(b.len(), |k| i + 3 + k),
            b'\'' => match text[i + 1..].chars().next() {
                Some(ch) if b.get(i + 1 + ch.len_utf8()) == Some(&b'\'') => i + 2 + ch.len_utf8(),
                _ => i + 1,
            },
            _ => i + 1,
        };
        let end = end.min(b.len());
        if end > i + 1 {
            blank(i, end);
        }
        i = end;
    }
    out
}

/// Source text with every `#[cfg(test)]` item removed; an item that never closes fails.
fn production_text(text: &str) -> String {
    let masked = mask_literals(text);
    let find = |from: usize, needle: &[u8]| {
        masked[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|k| from + k)
    };
    let (mut out, mut kept) = (String::new(), 0);
    while let Some(at) = find(kept, b"#[cfg(test)]") {
        out.push_str(&text[kept..at]);
        let end = match (find(at, b";"), find(at, b"{")) {
            (Some(semi), Some(open)) if semi < open => semi + 1,
            (_, Some(open)) => {
                let mut depth = 0usize;
                let close = masked[open..].iter().position(|&c| {
                    match c {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                });
                open + 1 + close.expect("a #[cfg(test)] item never closes")
            }
            (Some(semi), None) => semi + 1,
            (None, None) => panic!("a #[cfg(test)] item never ends"),
        };
        kept = end;
    }
    out.push_str(&text[kept..]);
    out
}

// Dormant guard: outside the owners, production only reads rows and consults gates; the switched
// operator CLI may also start a delegate, a reclaim or a force. Nothing builds the drain port.
#[test]
fn nothing_outside_the_owners_writes_a_home_or_runs_its_gate() {
    const OWNERS: &[&str] = &[
        "src/db/o_channel_homes.rs",
        "src/services/cluster/channel_home.rs",
        "src/services/cluster/channel_home_boot.rs",
        "src/services/cluster/channel_home_drain.rs",
        "src/services/cluster/channel_home_port.rs",
    ];
    const OPERATOR: (&str, &[&str]) = (
        "src/cli/channel_home.rs",
        &["delegate", "begin_reclaim", "force_orphan"],
    );
    const FORBIDDEN: &[&str] = &[
        "delegate",
        "finish_release",
        "finish_reclaim",
        "adopt",
        "begin_reclaim",
        "remove_reclaimed",
        "renew",
        "force_orphan",
        "register",
        "run_lease",
        "lease_round",
        "boot_home",
        "confirm",
        "close_intake",
        "resume_drain",
        "note_drain",
        "unregister",
        "unregister_if_same",
        "run_drain",
        "drain_round",
        "finish_return",
        "ChannelHomePort",
    ];
    let probe = production_text(concat!(
        "fn a() {}\n#[cfg(test)]\nmod t { fn b() { c(\"{\", '{', r#\"}\"#); } // }\n }",
        "\nfn d<'x>(_: &'x str) { e('}') }"
    ));
    assert!(probe.contains("fn a()") && probe.contains("fn d<") && !probe.contains("fn b()"));
    // A non-owner reading the home table: the words it may not name, or `None` when it reads none.
    let outside = |relative: &str, code: &str| {
        // `confirm::` is the O writer's settle module; the gate's `confirm` is only ever called.
        let code = &code.replace("confirm::", "");
        let tokens: Vec<&str> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .collect();
        let readers = [
            "o_channel_homes",
            "channel_home",
            "channel_home_drain",
            "channel_home_port",
        ];
        if !readers.iter().any(|reader| tokens.contains(reader)) {
            return None;
        }
        let allowed = match OPERATOR {
            (operator, allowed) if relative == operator => allowed,
            _ => &[][..],
        };
        let mut found = Vec::new();
        if code.contains("HomeGate::new") {
            found.push(format!("{relative}: HomeGate::new"));
        }
        let named = FORBIDDEN
            .iter()
            .filter(|word| tokens.contains(word) && !allowed.contains(word));
        found.extend(named.map(|word| format!("{relative}: {word}")));
        Some(found)
    };
    let built = "use crate::services::cluster::channel_home_port::ChannelHomePort;\n\
                 fn boot(r: R) { let _ = ChannelHomePort::new(1, r); }";
    let built = outside("src/services/discord/runtime_bootstrap.rs", built);
    assert_eq!(
        built,
        Some(vec![
            "src/services/discord/runtime_bootstrap.rs: ChannelHomePort".to_string()
        ]),
        "the scan catches a production build of the drain port"
    );
    let opened = "use crate::services::cluster::channel_home;\n\
                  use super::confirm::{self, Verdict};\n\
                  fn open(h: &channel_home::HomeGate) { h.confirm(&w, t); confirm::settle(); }";
    let opened = outside("src/services/tui_o/writer/deliver.rs", opened);
    let opened_named = vec!["src/services/tui_o/writer/deliver.rs: confirm".to_string()];
    assert_eq!(
        opened,
        Some(opened_named),
        "a gate confirm beside the module"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stack = vec![root.join("src")];
    let (mut users, mut violations) = (0, Vec::new());
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
            let code = String::from_utf8(mask_literals(&production_text(&text))).unwrap();
            let tokens: Vec<&str> = code
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .collect();
            if !OWNERS.contains(&relative.as_str()) {
                if let Some(found) = outside(&relative, &code) {
                    users += 1;
                    violations.extend(found);
                }
                continue;
            }
            // Each loop is named once at its definition and once where the boot starts it.
            for (owner, start) in [
                ("src/services/cluster/channel_home.rs", "run_lease"),
                ("src/services/cluster/channel_home_drain.rs", "run_drain"),
            ] {
                let starter = relative == "src/services/cluster/channel_home_boot.rs";
                let expected = usize::from(relative == owner || starter);
                let named = tokens.iter().filter(|token| **token == start).count();
                if named != expected {
                    violations.push(format!("{relative}: {start} x{named}"));
                }
            }
        }
    }
    assert!(users > 0, "the scan found no reader of the home table");
    assert!(
        violations.is_empty(),
        "channel home writer outside its owners: {violations:?}"
    );
}

/// Replacing a channel's gate withdraws the old one: it closes, never reopens and its lease ends
/// before any write; unregistering returns the channel to the gateway rules.
#[tokio::test(start_paused = true)]
async fn a_replaced_or_unregistered_gate_closes_for_good_and_its_lease_ends() {
    let first = Arc::new(HomeGate::new(C, "mini"));
    register(Arc::clone(&first));
    first
        .confirm(&written("mini", 4, HomeState::Worker), Instant::now())
        .expect("opens");
    assert_eq!(intake_hold(C, Some(4)), None);
    let second = Arc::new(HomeGate::new(C, "mini"));
    register(Arc::clone(&second));
    assert_eq!(admitted(&first), (None, None));
    advance(Duration::from_millis(1)).await;
    let reopened = first.confirm(&written("mini", 5, HomeState::Worker), Instant::now());
    assert_eq!(reopened, Err(ConfirmRefused::Retired));
    let nowhere = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/none");
    let lease = run_lease(nowhere.expect("lazy pool"), Arc::clone(&first), 4);
    let lease = tokio::time::timeout(RENEW_EVERY, lease).await;
    lease.expect("a withdrawn gate's lease ends at once");
    let current = registered(C).expect("the new gate");
    assert!(Arc::ptr_eq(&current, &second));
    assert!(
        intake_hold(C, Some(4)).is_some(),
        "the new gate has not opened"
    );

    let removed = unregister(C).expect("registered");
    assert!(Arc::ptr_eq(&removed, &second) && removed.withdrawn());
    assert!(registered(C).is_none() && !any_registered());
    assert_eq!(intake_hold(C, None), None, "the gateway rules again");
}

#[tokio::test(start_paused = true)]
async fn a_final_close_after_a_lapse_still_retires_the_last_epoch() {
    let home = HomeGate::new(C, "mini");
    home.confirm(&written("mini", 6, HomeState::Worker), Instant::now())
        .expect("opens");
    advance(HOLD_FOR).await;
    assert_eq!(home.ownership(), HomeOwnership::Lost);
    home.close();
    advance(Duration::from_millis(1)).await;
    let late = home.confirm(&written("mini", 6, HomeState::Worker), Instant::now());
    assert_eq!(late, Err(ConfirmRefused::Retired));
    assert_eq!(admitted(&home), (None, None));
    let next = home.confirm(&written("mini", 7, HomeState::Worker), Instant::now());
    assert_eq!(next, Ok(owned(7, 2, HomeIntake::Open)));
}

/// Routing answers for one channel and an unparseable destination on the current thread.
fn routing(channel: u64) -> impl PartialEq + std::fmt::Debug {
    use crate::services::tui_o::cutover::{boot_ownership, intake_route};
    let ownership: Vec<_> = boot_ownership()
        .into_iter()
        .map(|(channel, kind, candidate)| (channel, kind, candidate.map(|c| c.peek())))
        .collect();
    (
        intake_route::route("claude", channel),
        intake_route::route_text("claude", "x"),
        intake_route::route_for_placement("claude", channel),
        intake_route::held_channels("claude"),
        ownership,
    )
}

/// A node off the O home keeps a selected channel's store as standby from boot, so the first
/// delegation needs no restart; standby alone changes no routing, and only an open gate counts.
#[test]
fn standby_from_boot_changes_nothing_until_this_nodes_home_gate_takes_intake() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
    use crate::services::tui_o::cutover::intake_route::{IntakeRoute, test_probe};
    use crate::services::tui_o::cutover::test_override;
    const SELECTED: u64 = 4_380_501;
    let _ready = test_probe::answer_with(|_| true);
    use crate::services::tui_o::cutover::intake_route;
    let foreign = {
        let _plain = test_override::force_foreign(&[(SELECTED, ClaudeTui)], "gw");
        routing(SELECTED)
    };
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": "mini", "gateway_preferred_instance_id": "gw"},
        "tui_o": {"writer": {"channels": [SELECTED]}},
        "agents": [{"id": "w", "name": "W", "channels": {"claude": {"id": SELECTED.to_string(), "runtime": "tui"}}}],
    }))
    .unwrap();
    let stored = |channels: &std::collections::BTreeSet<u64>, committed: bool| {
        assert!(!committed, "off the home only local state is read");
        Ok(channels.iter().map(|&c| (c, Adoption::Committed)).collect())
    };
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(true, &config, stored).unwrap();
    assert!(
        boot.candidate(SELECTED).is_none(),
        "nothing adopted off the home"
    );
    let standby = boot.standby(SELECTED).cloned().expect("kept as standby");
    let _booted = test_override::force_boot(boot);
    assert_eq!(routing(SELECTED), foreign, "no home row: as before");

    // The first delegation in this process: a gate that has not opened holds the channel.
    let channel = SELECTED.to_string();
    let gate = std::sync::Arc::new(HomeGate::new(&channel, "mini"));
    register(std::sync::Arc::clone(&gate));
    assert!(matches!(
        intake_route::route("claude", SELECTED),
        IntakeRoute::Hold(_)
    ));
    assert!(matches!(
        intake_route::route_for_placement("claude", SELECTED),
        IntakeRoute::Hold(_)
    ));
    assert_eq!(intake_route::held_channels("claude"), [channel.clone()]);

    let renewal = HeldHome::for_test(&channel, "mini", 9, HomeState::Worker);
    gate.confirm(&renewal, Instant::now()).expect("opens");
    assert_eq!(
        intake_route::route_for_placement("claude", SELECTED),
        IntakeRoute::Gateway
    );
    assert!(intake_route::held_channels("claude").is_empty());
    assert_eq!(
        standby.peek(),
        Adoption::Committed,
        "the boot store, no restart"
    );
    // Intake closing between the owned read and the gate read names the channel once.
    let closing = std::sync::Arc::clone(&gate);
    let closing = test_probe::answer_with(move |_| {
        closing.close_intake();
        false
    });
    assert_eq!(intake_route::held_channels("claude"), [channel.clone()]);
    drop(closing);
    gate.close_intake();
    assert!(matches!(
        intake_route::route("claude", SELECTED),
        IntakeRoute::Hold(_)
    ));
    unregister(&channel);
    assert_eq!(routing(SELECTED), foreign, "row gone: as before");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn command_permit_linearizes_close_and_preserves_children() {
    let home = Arc::new(HomeGate::new(C, "mini"));
    home.confirm(&written("mini", 1, HomeState::Worker), Instant::now())
        .unwrap();
    register(home.clone());
    let permit = admit_command(C, "claude").unwrap().unwrap();
    home.close_intake();
    assert!(home.admit_command("claude").is_none());
    command_scope(Some(permit.clone()), async {
        let nested = admit_command(C, "claude").unwrap().unwrap();
        assert!(admit_command(C, "codex").is_err());
        assert_eq!(home.commands_in_flight(), 1);
        drop(nested);
    })
    .await;
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let child = tokio::spawn(command_scope(Some(permit.clone()), async move {
        entered.send(()).unwrap();
        finished.await.unwrap();
    }));
    entry.await.unwrap();
    drop(permit);
    assert_eq!(
        home.commands_in_flight(),
        1,
        "child owns the admitted execution"
    );
    home.close();
    assert!(home.admit_recovery("claude").is_none());
    finish.send(()).unwrap();
    child.await.unwrap();
    assert_eq!(home.commands_in_flight(), 0);
    unregister(C);
}

#[tokio::test(flavor = "current_thread")]
async fn command_permit_drop_panic_and_blocking_abort_are_not_unknown() {
    let home = register_for_test(9200000000000102, Some(HomeState::Worker));
    let permit = home.admit_command("claude").unwrap();
    let dropped = command_scope(Some(permit), std::future::pending::<()>());
    drop(dropped);
    assert_eq!(
        home.commands_in_flight(),
        0,
        "unpolled future drops its root"
    );
    let permit = home.admit_command("claude").unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = permit;
        panic!("command panic");
    }));
    assert!(result.is_err());
    assert_eq!(home.commands_in_flight(), 0, "unwind has no Unknown state");
    let permit = home.admit_command("claude").unwrap();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let _held = command_worker_scope(Some(permit));
        entered.send(()).unwrap();
        finished.recv().unwrap();
    });
    entry.await.unwrap();
    worker.abort();
    assert_eq!(
        home.commands_in_flight(),
        1,
        "abort is not a running closure join"
    );
    finish.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(home.commands_in_flight(), 0);
    unregister(home.channel_id());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn command_registry_unregister_is_immediate_and_reuses_pending_count() {
    let channel = 9200000000000103;
    let old = register_for_test(channel, Some(HomeState::Worker));
    let permit = old.admit_command("claude").unwrap();
    assert!(unregister_if_same(&old));
    assert!(
        registered(old.channel_id()).is_none(),
        "watch removal must not leave a permanent refusal"
    );
    assert!(
        admit_command(old.channel_id(), "claude").unwrap().is_none(),
        "Legacy executes without restart"
    );
    let new = register_for_test(channel, Some(HomeState::Worker));
    assert_eq!(
        new.commands_in_flight(),
        1,
        "replacement sees old execution"
    );
    new.close_intake();
    assert!(new.admit_command("claude").is_none());
    let recovery = new.admit_recovery("claude").unwrap();
    new.close();
    assert_eq!(new.commands_in_flight(), 2);
    drop(permit);
    assert_eq!(new.commands_in_flight(), 1);
    drop(recovery);
    assert_eq!(new.commands_in_flight(), 0);
    assert!(
        new.admit_command("claude").is_none(),
        "drop never grants ownership"
    );
    unregister(new.channel_id());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn command_replacement_and_resume_keep_counter_and_intake_closed() {
    let old = Arc::new(HomeGate::new(C, "mini"));
    old.confirm(&written("mini", 1, HomeState::Worker), Instant::now())
        .unwrap();
    register(old.clone());
    let permit = old.admit_command("claude").unwrap();
    let new = Arc::new(HomeGate::new(C, "mini"));
    register(new.clone());
    assert!(old.admit_command("claude").is_none());
    new.confirm(&written("mini", 2, HomeState::Reclaiming), Instant::now())
        .unwrap();
    new.close();
    advance(Duration::from_nanos(1)).await;
    new.resume_drain(&written("mini", 2, HomeState::Reclaiming), Instant::now())
        .unwrap();
    assert!(new.admit_command("claude").is_none());
    assert_eq!(new.commands_in_flight(), 1);
    assert!(new.admit_recovery("claude").is_some());
    drop(permit);
    assert_eq!(new.commands_in_flight(), 0);
    unregister(C);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn command_admission_expires_at_the_renewal_deadline() {
    for elapsed in [
        HOLD_FOR - Duration::from_nanos(1),
        HOLD_FOR,
        HOLD_FOR + Duration::from_nanos(1),
    ] {
        let home = HomeGate::new(C, "mini");
        home.confirm(&written("mini", 1, HomeState::Worker), Instant::now())
            .unwrap();
        advance(elapsed).await;
        assert_eq!(home.admit_command("claude").is_some(), elapsed < HOLD_FOR);
        assert_eq!(home.commands_in_flight(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn command_dormant_admission_does_not_lookup_or_start_a_task() {
    assert!(
        with_homes(|homes| homes.get().is_none()),
        "fresh thread has no registry"
    );
    COMMAND_LOOKUPS.with(|calls| calls.set(0));
    for _ in 0..3 {
        let permit = admit_command(C, "claude").unwrap();
        assert!(permit.is_none());
        assert_eq!(command_scope(permit, async { 7 }).await, 7);
    }
    COMMAND_LOOKUPS.with(|calls| assert_eq!(calls.get(), 0));
}

#[tokio::test(flavor = "current_thread")]
async fn command_registration_never_discards_a_preexisting_local_execution() {
    let old = register_for_test(9200000000000202, Some(HomeState::Worker));
    let new = Arc::new(HomeGate::new(old.channel_id(), "mini"));
    let write = HeldHome::for_test(old.channel_id(), "mini", 2, HomeState::Worker);
    new.confirm(&write, Instant::now()).unwrap();
    let permit = new.admit_recovery("claude").unwrap();
    register(new.clone());
    assert_eq!(new.commands_in_flight(), 1);
    assert!(
        Arc::ptr_eq(&registered(old.channel_id()).unwrap(), &old),
        "unsafe replacement is not published"
    );
    drop(permit);
    register(new.clone());
    assert!(Arc::ptr_eq(&registered(old.channel_id()).unwrap(), &new));
    unregister(old.channel_id());
}
