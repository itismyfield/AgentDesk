use super::*;

#[test]
fn ownership_uses_flag_membership_and_tui_kind_together() {
    let channels = BTreeSet::from([41]);
    for enabled in [false, true] {
        for (channel, selected) in [(41, true), (42, false)] {
            for (kind, tui) in [
                (Some(RuntimeHandoffKind::ClaudeTui), true),
                (Some(RuntimeHandoffKind::CodexTui), true),
                (Some(RuntimeHandoffKind::LegacyTmuxWrapper), false),
                (Some(RuntimeHandoffKind::ProcessBackend), false),
                (Some(RuntimeHandoffKind::ClaudeEAdapter), false),
                (None, false),
            ] {
                assert_eq!(
                    owns_output(enabled, &channels, channel, kind),
                    enabled && selected && tui
                );
                assert!(!owns_output(enabled, &BTreeSet::new(), channel, kind));
            }
        }
    }
}

#[test]
fn boot_membership_survives_reload_until_restart() {
    const CHILD: &str = "ADK_TEST_WRITER_BOOT_SNAPSHOT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "services::tui_o::channel_policy::tests::boot_membership_survives_reload_until_restart", "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", tempfile::tempdir().unwrap().path())
            .output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // A filter that matches nothing also exits 0; require the child to have run this test.
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let write = |channels: &str| {
        std::fs::write(&path, format!(
            "server: {{}}\nagents:\n  - id: fixture\n    name: Fixture\n    channels:\n      claude: {{id: '41', runtime: tui}}\n      codex: {{id: '42', runtime: tui}}\ntui_o:\n  writer:\n    channels: {channels}\n"
        )).unwrap();
    };
    write("[41]");
    let original = crate::config::load_from_path(&path).unwrap();
    install(&original).unwrap();
    crate::config_live_reload::install(original);
    write("[42]");
    for _ in 0..2 {
        let outcome = crate::config_live_reload::reload_from_path(&path);
        assert!(
            matches!(outcome, crate::config_live_reload::ReloadOutcome::Applied { restart_required } if restart_required.contains(&"tui_o.writer.channels"))
        );
        let snapshot = boot().unwrap();
        assert!(owns_output(
            true,
            snapshot.channels(),
            41,
            Some(RuntimeHandoffKind::ClaudeTui)
        ));
        assert!(!owns_output(
            true,
            snapshot.channels(),
            42,
            Some(RuntimeHandoffKind::CodexTui)
        ));
    }
    assert!(install(&crate::config::load_from_path(&path).unwrap()).is_err());
    write("[41]");
    assert!(
        matches!(crate::config_live_reload::reload_from_path(&path), crate::config_live_reload::ReloadOutcome::Applied { restart_required } if !restart_required.contains(&"tui_o.writer.channels"))
    );
    write("[0]");
    assert!(matches!(
        crate::config_live_reload::reload_from_path(&path),
        crate::config_live_reload::ReloadOutcome::Rejected { .. }
    ));
    assert!(owns_output(
        true,
        boot().unwrap().channels(),
        41,
        Some(RuntimeHandoffKind::ClaudeTui)
    ));
}

fn writer_config(channels: &[u64], cluster: serde_json::Value) -> Config {
    serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": cluster, "tui_o": {"writer": {"channels": channels}},
        "agents": [{"id": "w", "name": "W", "channels": {"claude": {"id": "41", "runtime": "tui"}}}],
    }))
    .unwrap()
}

#[test]
fn only_an_enabled_home_with_a_list_reads_the_store_and_adopts() {
    use serde_json::json;
    let untouched = |_: &BTreeSet<u64>| -> BTreeMap<u64, Adoption> { panic!("store read") };
    let found = |_: &BTreeSet<u64>| BTreeMap::from([(41, Adoption::Committed)]);
    let off = json!({});
    let boot =
        |channels: &[u64], cluster| BootChannels::validate(&writer_config(channels, cluster));
    let seeded = boot(&[41], off.clone()).unwrap().seeded(false, untouched);
    assert!(seeded.candidate(41).is_none(), "writer off");
    let seeded = boot(&[], off.clone()).unwrap().seeded(true, untouched);
    assert!(seeded.candidate(41).is_none(), "empty list");
    let seeded = boot(&[41], off).unwrap().seeded(true, found);
    assert_eq!(
        seeded.candidate(41).map(Candidate::peek),
        Some(Adoption::Committed)
    );

    let home = json!({"enabled": true, "instance_id": "a", "gateway_preferred_instance_id": "a"});
    let home = boot(&[41], home).unwrap();
    assert_eq!(
        (home.site(), home.configured_id()),
        (&Site::Home, Some("a"))
    );
    let foreign =
        json!({"enabled": true, "instance_id": "b", "gateway_preferred_instance_id": "a"});
    let foreign = boot(&[41], foreign).unwrap();
    assert_eq!(foreign.site(), &Site::Foreign { home: "a".into() });
    assert!(
        foreign.seeded(true, found).candidate(41).is_none(),
        "a non-home node adopts nothing"
    );
    for unnamed in [
        json!({"enabled": true, "instance_id": "a"}),
        json!({"enabled": true, "gateway_preferred_instance_id": "a"}),
    ] {
        assert!(boot(&[41], unnamed.clone()).is_err(), "{unnamed}");
        assert!(boot(&[], unnamed).is_ok(), "no list needs no home");
    }
}

#[test]
fn a_selected_channel_starts_committed_only_over_a_readable_init() {
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    let root = tempfile::tempdir().unwrap();
    let channels = BTreeSet::from([1, 2, 3, 4, 5]);
    let all = |state| {
        channels
            .iter()
            .map(|&c| (c, state))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        adoption::stored(Some(root.path()), &channels),
        all(Adoption::Pending),
        "no store"
    );
    assert_eq!(
        adoption::stored(None, &channels),
        all(Adoption::Held),
        "no runtime root"
    );
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path());
    let at = chrono::Utc::now();
    let init = |channel| {
        let (sources, initial_anchor, build_digest) = (Vec::new(), 0, "b".into());
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    store
        .unwrap()
        .unwrap()
        .begin_era(&[1, 2], at, init)
        .unwrap();
    let dir = root.path().join("o_store");
    std::fs::remove_file(dir.join("2/init")).unwrap();
    std::fs::create_dir(dir.join("3")).unwrap();
    std::fs::create_dir(dir.join("5")).unwrap();
    std::fs::write(dir.join("5/init"), b"{").unwrap();
    let expected = BTreeMap::from([
        (1, Adoption::Committed),
        (2, Adoption::Held),
        (3, Adoption::Held),
        (4, Adoption::Pending),
        (5, Adoption::Held),
    ]);
    assert_eq!(adoption::stored(Some(root.path()), &channels), expected);
}

#[test]
fn a_body_releases_a_pending_adoption_while_a_peek_or_another_channel_leaves_it() {
    use crate::services::tui_o::cutover::{self, IdentityError, test_override};
    use RuntimeHandoffKind::{ClaudeTui, CodexTui};
    let _candidates = test_override::force_candidates(&[(41, ClaudeTui)]);
    let state = || test_override::with_channels(|boot| boot.unwrap().candidate(41).unwrap().peek());
    assert_eq!(
        cutover::peek_o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(false)
    );
    assert_eq!(cutover::o_owns_tui_output_for_channel(42, None), Ok(false));
    let mismatch = cutover::o_owns_tui_output_for_channel(41, Some(CodexTui));
    assert!(matches!(mismatch, Err(IdentityError::KindMismatch { .. })));
    assert_eq!(state(), Adoption::Pending, "no body was judged for 41");
    assert_eq!(
        cutover::o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(false)
    );
    assert_eq!(state(), Adoption::Released);
    assert_eq!(
        cutover::peek_o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(false)
    );

    let _committed = test_override::force_channels(&[(41, ClaudeTui)]);
    assert_eq!(
        cutover::o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(true)
    );
    assert_eq!(
        cutover::peek_o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(true)
    );
    let _foreign = test_override::force_foreign(&[(41, ClaudeTui)], "a");
    assert_eq!(
        cutover::o_owns_tui_output_for_channel(41, Some(ClaudeTui)),
        Ok(false)
    );
}

/// The check the site tests rely on: a bodiless claim, a body before its claim, and the body on
/// another channel each fail it; the body on the watched channel after its claim settles it.
#[test]
fn the_body_check_fails_a_bodiless_claim_and_a_body_before_its_claim() {
    use crate::services::tui_o::channel_policy::SinkOp::{Patch, Post};
    use crate::services::tui_o::cutover::{self, test_override};
    use RuntimeHandoffKind::ClaudeTui;
    let settled = |check: &BodyCheck| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check.assert_settled())).is_ok()
    };
    let claim = |channel| cutover::o_owns_tui_output_for_channel(channel, Some(ClaudeTui));
    let channels = [(41, ClaudeTui), (43, ClaudeTui), (45, ClaudeTui)];
    let _candidates = test_override::force_candidates(&channels);
    let (claimed, early, elsewhere) = (
        BodyCheck::watch(41, "answer"),
        BodyCheck::watch(43, "answer"),
        BodyCheck::watch(45, "answer"),
    );
    assert_eq!(claim(41), Ok(false));
    claimed.sink(41, Post, "a notice without it");
    assert!(!settled(&claimed), "released with no body shown");
    claimed.sink(41, Patch, "the answer, banner and all");
    assert!(settled(&claimed));

    early.sink(43, Post, "the answer");
    assert_eq!(claim(43), Ok(false));
    assert!(
        !settled(&early),
        "the body left while the adoption was pending"
    );

    assert_eq!(claim(45), Ok(false));
    elsewhere.sink(46, Post, "the answer");
    assert!(
        elsewhere.bodiless_release(),
        "another channel's body settles nothing"
    );
    elsewhere.sink_request("PATCH", "/api/v10/channels/45/messages/7", "the answer");
    assert!(!elsewhere.bodiless_release());
    assert!(
        !settled(&elsewhere),
        "the body went to another channel first"
    );
}

#[test]
fn a_placement_is_held_off_the_home_and_ends_a_pending_adoption_on_it() {
    use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
    use crate::services::tui_o::cutover::test_override;
    use RuntimeHandoffKind::ClaudeTui;
    let held = |route| matches!(route, IntakeRoute::Hold(detail) if detail.contains("O home a"));
    let _foreign = test_override::force_foreign(&[(41, ClaudeTui)], "a");
    assert!(held(intake_route::route_for_placement("claude", 41)));
    assert!(held(intake_route::route_text_for_placement(
        "claude",
        "not-a-channel"
    )));
    assert_eq!(
        intake_route::route_for_placement("claude", 42),
        IntakeRoute::Unselected
    );
    assert_eq!(
        intake_route::route("claude", 41),
        IntakeRoute::Unselected,
        "claims are not held"
    );
    assert!(intake_route::held_channels("claude").is_empty());

    let _candidates = test_override::force_candidates(&[(41, ClaudeTui)]);
    let check = BodyCheck::watch(41, "placed turn");
    assert_eq!(
        intake_route::route_for_placement("claude", 42),
        IntakeRoute::Unselected
    );
    check.assert_settled();
    assert_eq!(
        intake_route::route_for_placement("claude", 41),
        IntakeRoute::Unselected
    );
    assert_eq!(check.adoption(), Adoption::Released);
    // The one release with no body by design: the placed Legacy turn takes the channel.
    assert!(check.bodiless_release());
}

#[test]
fn a_placement_never_waits_on_another_channels_adoption_in_progress() {
    use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute, test_probe};
    use crate::services::tui_o::cutover::test_override;
    use RuntimeHandoffKind::ClaudeTui;
    use std::sync::mpsc;
    let _candidates = test_override::force_candidates(&[(41, ClaudeTui), (43, ClaudeTui)]);
    let candidate = |channel| {
        test_override::with_channels(|boot| boot.unwrap().candidate(channel).cloned()).unwrap()
    };
    assert!(candidate(43).confirm_store());
    // 41's activation holds its lock (init I/O) until every route below has returned.
    let (locked_tx, locked) = mpsc::channel();
    let (done, done_rx) = mpsc::channel::<()>();
    let adopting = candidate(41);
    let holder = std::thread::spawn(move || {
        let _held = adopting.lock();
        locked_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok()
    });
    locked.recv().unwrap();
    let _ready = test_probe::answers(&[true]);
    let routes = [
        intake_route::route_for_placement("claude", 42),
        intake_route::route("claude", 42),
        intake_route::route_for_placement("claude", 43),
    ];
    done.send(()).ok();
    assert!(holder.join().unwrap(), "a route waited for 41's lock");
    assert_eq!(
        routes,
        [
            IntakeRoute::Unselected,
            IntakeRoute::Unselected,
            IntakeRoute::Gateway
        ]
    );
    assert_eq!(candidate(41).peek(), Adoption::Pending);
}
