//! A watcher claim on a Herdr pane commits only under the reconcile's admission, and a
//! withhold neither waits on nor undoes a claim that read the admission before it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;

use super::*;
use crate::services::tmux_common::{
    source_authority_contention_key_for_tests, with_tmux_source_authority,
};
use crate::services::tui_prompt_dedupe::{
    HOLD_CONTENDED, admit_herdr_execution, install_herdr_execution, withhold_herdr_execution,
};

fn handle(name: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: name.to_string(),
        output_path: format!("/tmp/{name}.jsonl"),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        )),
    }
}

fn claim(
    watchers: &TmuxWatcherRegistry,
    channel: u64,
    name: &str,
    host: WatchHost,
    replace: bool,
) -> Result<WatcherClaimAction, WatchWithheld> {
    let (channel, handle, provider) =
        (ChannelId::new(channel), handle(name), &ProviderKind::Claude);
    let claim = if replace {
        claim_or_replace_watcher_for_host(watchers, channel, handle, provider, "p8", None, host)
    } else {
        claim_or_reuse_watcher_for_host(watchers, channel, handle, provider, "p8", None, host)
    };
    claim.map(|claim| claim.action)
}

fn admitted(name: &str) {
    install_herdr_execution(name, "p8-n1");
    admit_herdr_execution(name, "p8-n1");
}

// Each claim funnel withholds a Herdr host the map does not list and a listed pane no reconcile
// admitted, whatever the host; a withhold after a committed claim leaves its watcher alone.
#[test]
fn a_herdr_claim_commits_only_under_its_admission_and_keeps_the_incumbent() {
    let watchers = TmuxWatcherRegistry::new();
    let names = [
        "p8c-legacy",
        "p8c-unverified",
        "p8c-db-herdr",
        "p8c-listed",
        "p8c-admitted",
    ];
    let [legacy, unverified, db_herdr, listed, admitted_name] = names;
    install_herdr_execution(listed, "p8-n1");
    admitted(admitted_name);
    let cases = [
        (
            legacy,
            WatchHost::Legacy,
            Ok(WatcherClaimAction::SpawnFresh),
        ),
        (
            unverified,
            WatchHost::Unverified,
            Ok(WatcherClaimAction::SpawnFresh),
        ),
        (db_herdr, WatchHost::Herdr, Err(WatchWithheld)),
        (listed, WatchHost::Legacy, Err(WatchWithheld)),
        (
            admitted_name,
            WatchHost::Herdr,
            Ok(WatcherClaimAction::SpawnFresh),
        ),
    ];
    for (channel, (name, host, expected)) in (5340_1..).zip(cases) {
        assert_eq!(
            claim(&watchers, channel, name, host, false),
            expected,
            "{name}"
        );
        let installed = watchers.owner_channel_for_tmux_session(name).is_some();
        assert_eq!(
            installed,
            expected.is_ok(),
            "{name}: a withheld claim installs nothing"
        );
        let try_claim = try_claim_watcher_for_host(
            &watchers,
            ChannelId::new(channel + 100),
            handle(name),
            None,
            None,
            host,
        );
        assert_eq!(
            try_claim.is_err(),
            expected.is_err(),
            "{name}: try_claim agrees"
        );
    }

    let incumbent = watchers
        .get(&ChannelId::new(5340_5))
        .expect("admitted claim installed");
    let (cancel, paused) = (incumbent.cancel.clone(), incumbent.paused.clone());
    drop(incumbent);
    withhold_herdr_execution(admitted_name, Some("p8-n1"));
    for replace in [false, true] {
        let again = claim(&watchers, 5340_5, admitted_name, WatchHost::Herdr, replace);
        assert_eq!(again, Err(WatchWithheld), "replace={replace}");
    }
    let kept = watchers
        .get(&ChannelId::new(5340_5))
        .expect("incumbent stays installed");
    assert!(Arc::ptr_eq(&kept.cancel, &cancel) && Arc::ptr_eq(&kept.paused, &paused));
    assert!(!cancel.load(Ordering::Relaxed) && !paused.load(Ordering::Relaxed));
}

// A claim parked after reading an admission still holds the map, so a withhold on another thread
// waits for the claim's commit and the next claim is withheld.
#[test]
fn a_withhold_waits_for_a_claim_that_read_the_admission() {
    let name = "p8c-race";
    admitted(name);
    let watchers = Arc::new(TmuxWatcherRegistry::new());
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let claimer = {
        let watchers = watchers.clone();
        std::thread::spawn(move || {
            CLAIM_PAUSE.set(Some((paused_tx, resume_rx)));
            claim(&watchers, 5340_11, name, WatchHost::Herdr, false)
        })
    };
    paused_rx
        .recv()
        .expect("claim parks after its admission read");

    let (events_tx, events_rx) = mpsc::channel();
    let withholder = std::thread::spawn(move || {
        HOLD_CONTENDED.set(Some(events_tx.clone()));
        withhold_herdr_execution(name, Some("p8-n1"));
        events_tx.send("hold done").unwrap();
    });
    assert_eq!(
        events_rx.recv().unwrap(),
        "hold contended",
        "withhold waits on the claim"
    );
    resume_tx.send(()).unwrap();
    assert_eq!(claimer.join().unwrap(), Ok(WatcherClaimAction::SpawnFresh));
    assert_eq!(events_rx.recv().unwrap(), "hold done");
    withholder.join().unwrap();

    assert_eq!(
        watchers.len(),
        1,
        "the claim that read the admission installed once"
    );
    let next = claim(&watchers, 5340_12, name, WatchHost::Herdr, true);
    assert_eq!(
        next,
        Err(WatchWithheld),
        "a claim after the withhold is withheld"
    );
    assert_eq!(
        watchers.owner_channel_for_tmux_session(name),
        Some(ChannelId::new(5340_11))
    );
}

// Admitted and withheld claims finish while another thread holds both panes' source authority:
// a claim never takes it.
#[test]
fn a_claim_takes_no_source_authority() {
    let [admitted_name, withheld] = ["p8c-noauth-admitted", "p8c-noauth-withheld"];
    admitted(admitted_name);
    install_herdr_execution(withheld, "p8-n1");
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        with_tmux_source_authority(admitted_name, |_| {
            with_tmux_source_authority(withheld, |_| {
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        })
    });
    held_rx.recv().unwrap();

    let watchers = TmuxWatcherRegistry::new();
    let mut results = Vec::new();
    let contended = source_authority_contention_key_for_tests(|| {
        results.push(claim(
            &watchers,
            5340_21,
            admitted_name,
            WatchHost::Herdr,
            false,
        ));
        results.push(claim(&watchers, 5340_22, withheld, WatchHost::Herdr, false));
    });
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(contended, None, "a claim must not wait on source authority");
    assert_eq!(
        results,
        [Ok(WatcherClaimAction::SpawnFresh), Err(WatchWithheld)]
    );
}
