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
    type Stored = std::io::Result<BTreeMap<u64, Adoption>>;
    let untouched = |_: &BTreeSet<u64>, _: bool| -> Stored { panic!("store read") };
    let found = |_: &BTreeSet<u64>, _: bool| Ok(BTreeMap::from([(41, Adoption::Committed)]));
    let off = json!({});
    let boot =
        |channels: &[u64], cluster| BootChannels::validate(&writer_config(channels, cluster));
    fn seed(
        boot: Result<BootChannels>,
        enabled: bool,
        stored: impl FnOnce(&BTreeSet<u64>, bool) -> Stored,
    ) -> BootChannels {
        let config = writer_config(&[41], serde_json::json!({}));
        boot.unwrap().seeded(enabled, &config, stored).unwrap()
    }
    let seeded = seed(boot(&[41], off.clone()), false, untouched);
    assert!(seeded.candidate(41).is_none(), "writer off");
    let seeded = seed(boot(&[], off.clone()), true, untouched);
    assert!(seeded.candidate(41).is_none(), "empty list");
    let seeded = seed(boot(&[41], off), true, found);
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
        seed(Ok(foreign), true, found).candidate(41).is_none(),
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

/// Runs `test` once per role, each in a child process with its own runtime root, and returns
/// `None`; inside such a child returns its role.
fn boot_role(test: &str, roles: &[&str]) -> Option<String> {
    const ROLE: &str = "ADK_TEST_WRITER_BOOT_ROLE";
    if let Ok(role) = std::env::var(ROLE) {
        return Some(role);
    }
    for role in roles {
        let root = tempfile::tempdir().unwrap();
        let name = format!("services::tui_o::channel_policy::tests::{test}");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(ROLE, role)
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{role}: {stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed; 0 failed; 0 ignored"),
            "{role}: {stdout}"
        );
    }
    None
}

/// A config registering each channel as a Claude TUI and selecting `selected`.
fn boot_config(selected: &[u64], registered: &[u64], cluster: serde_json::Value) -> Config {
    let agent = |id: &u64| {
        let binding = serde_json::json!({"claude": {"id": id.to_string(), "runtime": "tui"}});
        serde_json::json!({"id": format!("a{id}"), "name": "A", "channels": binding})
    };
    let agents: Vec<_> = registered.iter().map(agent).collect();
    serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": cluster, "agents": agents,
        "tui_o": {"writer": {"channels": selected}},
    }))
    .unwrap()
}

/// Commits `era` through the era and `later` by a first `init` after it, in this child's root.
fn commit_on_disk(era: &[u64], later: &[u64]) -> std::path::PathBuf {
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    let root = crate::config::runtime_root().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let at = chrono::Utc::now();
    let init = |channel| Initialized {
        channel,
        sources: Vec::new(),
        initial_anchor: 0,
        build_digest: "b".into(),
        at,
    };
    store.begin_era(era, at, |c| Ok(init(c))).unwrap();
    for &channel in later {
        store.init_channel(&init(channel)).unwrap();
    }
    root.join(crate::services::tui_o::store::STORE_DIR_NAME)
}

fn selection_missing() -> Vec<String> {
    let reasons = crate::services::tui_o::alarm::health_reasons();
    let missing = reasons
        .into_iter()
        .filter(|r| r.contains(":selection_missing:"));
    missing.collect()
}

fn o_owns(channel: u64) -> bool {
    let owns = super::super::cutover::o_owns_tui_output_for_channel;
    owns(channel, Some(RuntimeHandoffKind::ClaudeTui)).unwrap()
}

/// On the home, a channel committed on disk stays O's when the selection drops it, with one alarm
/// per such channel; a full list and the empty list boot as before.
#[test]
fn a_committed_channel_left_out_of_the_selection_stays_o_on_the_home() {
    let test = "a_committed_channel_left_out_of_the_selection_stays_o_on_the_home";
    let roles = ["dropped", "listed", "empty", "unregistered"];
    let Some(role) = boot_role(test, &roles) else {
        return;
    };
    let store = commit_on_disk(&[41, 42], &[43]);
    std::fs::create_dir(store.join("44")).unwrap();
    std::fs::write(store.join("44/init"), b"{").unwrap();
    std::fs::create_dir(store.join("045")).unwrap();
    std::fs::write(store.join("045/init"), b"{").unwrap();
    let registered = [41, 42, 43, 44, 46];
    let selected: &[u64] = match role.as_str() {
        "dropped" | "unregistered" => &[41],
        "listed" => &[41, 42, 43, 44],
        _ => &[],
    };
    let registered = if role == "unregistered" {
        &registered[..2]
    } else {
        &registered[..]
    };
    let config = boot_config(selected, registered, serde_json::json!({}));
    if role == "unregistered" {
        let error = install(&config).unwrap_err();
        assert!(
            format!("{error:#}").contains("o_store: channel 43 is not registered"),
            "{error:#}"
        );
        return;
    }
    install(&config).unwrap();
    let owned: Vec<_> = [41, 42, 43, 44, 46]
        .into_iter()
        .filter(|&c| o_owns(c))
        .collect();
    let held = boot().unwrap().candidate(44).map(Candidate::peek);
    if role == "empty" {
        assert_eq!((owned, held), (vec![], None), "the empty list is O off");
    } else {
        assert_eq!((owned, held), (vec![41, 42, 43, 44], Some(Adoption::Held)));
    }
    let missing = match role.as_str() {
        "dropped" => vec![42, 43, 44],
        _ => vec![],
    };
    let missing: Vec<_> = missing
        .into_iter()
        .map(|c| format!("tui_o:selection_missing:{c}"))
        .collect();
    assert_eq!(selection_missing(), missing);
    install(&config).unwrap();
    let changes = crate::config_live_reload::restart_required_changes(&config, &config);
    assert!(!changes.contains(&"tui_o.writer.channels"), "{changes:?}");
}

/// The unsupported two-node drift: the home keeps its committed channel with an alarm, while a
/// standby whose selection also dropped it places it as an ordinary Legacy channel.
#[test]
fn a_channel_both_nodes_dropped_stays_o_on_the_home_and_turns_legacy_on_the_standby() {
    use crate::services::tui_o::cutover::intake_route::{IntakeRoute, route_for_placement};
    let test = "a_channel_both_nodes_dropped_stays_o_on_the_home_and_turns_legacy_on_the_standby";
    let Some(role) = boot_role(test, &["home", "standby"]) else {
        return;
    };
    // The standby's store is a leftover it must not act on.
    commit_on_disk(&[41], &[42]);
    let local = if role == "home" { "a" } else { "b" };
    let cluster = serde_json::json!({
        "enabled": true, "instance_id": local, "gateway_preferred_instance_id": "a",
    });
    install(&boot_config(&[41], &[41, 42], cluster)).unwrap();
    let placed = route_for_placement("claude", 42);
    if role == "home" {
        assert!(o_owns(42));
        assert!(matches!(placed, IntakeRoute::Hold(_)), "{placed:?}");
        assert_eq!(selection_missing(), ["tui_o:selection_missing:42"]);
    } else {
        assert!(!o_owns(42));
        assert_eq!(placed, IntakeRoute::Unselected);
        assert!(selection_missing().is_empty());
        let held = route_for_placement("claude", 41);
        assert!(matches!(held, IntakeRoute::Hold(d) if d.contains("O home a")));
    }
}
