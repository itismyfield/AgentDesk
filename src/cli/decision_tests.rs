//! The `decision` command line, and the order an ask keeps: capability on disk, then the request.
use super::*;
use serde_json::json;

#[derive(clap::Parser)]
struct Probe {
    #[command(flatten)]
    args: DecisionArgs,
}

#[test]
fn decision_cli_takes_ask_with_a_json_file_and_get_with_an_id() {
    use clap::Parser;
    let parsed =
        |args: &[&str]| Probe::try_parse_from(args.iter().copied()).map(|probe| probe.args.command);
    assert!(matches!(
        parsed(&["decision", "ask", "--json-file", "-"]),
        Ok(DecisionCommand::Ask { json_file }) if json_file == "-"
    ));
    assert!(matches!(
        parsed(&["decision", "get", "7b0c8a52-3f0e-4c57-9a4e-0d0a5b6c7d8e"]),
        Ok(DecisionCommand::Get { .. })
    ));
    assert!(parsed(&["decision", "ask"]).is_err());
}

/// The capability files under the cache, each with whether its file and directory are private.
fn cached(root: &std::path::Path) -> Vec<(String, bool)> {
    fn private(path: &std::path::Path, mode: u32) -> bool {
        #[cfg(unix)]
        let held = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        };
        #[cfg(not(unix))]
        let held = {
            let _ = path;
            mode
        };
        held == mode
    }
    let mut found = Vec::new();
    for dir in std::fs::read_dir(root.join("decision-owner")).unwrap() {
        let dir = dir.unwrap().path();
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            let text = std::fs::read_to_string(&file).unwrap();
            found.push((text, private(&dir, 0o700) && private(&file, 0o600)));
        }
    }
    found
}

/// The capability is on disk, private, before the request is sent; a retry of the same ask
/// presents the same one, another ask another one, and a body without its scope sends nothing.
#[test]
fn decision_ask_caches_its_capability_before_it_sends() {
    let root = tempfile::tempdir().unwrap();
    let body = |key: &str| json!({"agent_id": "asker", "target": {"channel_id": "7"}, "ref": key});
    let mut sent = Vec::new();
    for _ in 0..2 {
        let capability = ask(root.path(), &body("r1"), |capability| {
            assert_eq!(cached(root.path()), [(capability.to_string(), true)]);
            Err::<(), String>(format!("lost:{capability}"))
        });
        sent.push(capability.unwrap_err());
    }
    assert_eq!(sent[0], sent[1], "a retry presents the same capability");
    let first = sent[0].trim_start_matches("lost:").to_string();
    assert!(crate::services::discord::decisions::requester::well_formed(
        &first
    ));
    let other = ask(root.path(), &body("r2"), |capability| {
        Ok(capability.to_string())
    });
    assert_ne!(other.unwrap(), first);
    assert_eq!(cached(root.path()).len(), 2);

    let unscoped = json!({"target": {"channel_id": "7"}, "ref": "r3"});
    let refused = ask(root.path(), &unscoped, |_| Ok(()));
    assert!(refused.is_err());
    assert_eq!(
        cached(root.path()).len(),
        2,
        "no capability without a scope"
    );
}

/// A read presents the capability its request was asked under, taken from the private cache; a
/// request this host never asked is not sent at all.
#[test]
fn decision_get_presents_the_capability_its_request_was_asked_under() {
    let root = tempfile::tempdir().unwrap();
    let body = json!({"agent_id": "asker", "target": {"channel_id": "7"}, "ref": "r1"});
    let asked = ask(root.path(), &body, |capability| Ok(capability.to_string())).unwrap();
    let id = uuid::Uuid::new_v4();
    remember(root.path(), &json!({"request_id": id}), Some(&asked)).unwrap();
    let sent = get(root.path(), id, |path, capability| {
        Ok((path.to_string(), capability.to_string()))
    });
    assert_eq!(
        sent.unwrap(),
        (format!("/api/decisions/{id}"), asked.clone())
    );
    let stranger = get(root.path(), uuid::Uuid::new_v4(), |_, _| Ok(()));
    assert!(
        stranger.is_err(),
        "a request this host did not ask is not read"
    );
    assert!(remember(root.path(), &json!({"ok": false}), Some(&asked)).is_err());
}

/// A stored request is linked before its answer is shown, also when Discord refused it, and an
/// answer whose link cannot be written is never shown as a success.
#[test]
fn decision_ask_answer_is_shown_only_once_its_request_is_linked() {
    let root = tempfile::tempdir().unwrap();
    let (capability, id) = ("a".repeat(64), uuid::Uuid::new_v4());
    let reply = json!({"ok": true, "request_id": id});
    let mut shown = 0;
    let linked = settle(root.path(), 201, &reply, Some(&capability), |_| {
        assert!(owner_cache::of_request(root.path(), id).is_ok());
        shown += 1;
    });
    assert_eq!((linked, shown), (Ok(()), 1));
    let blocked = tempfile::NamedTempFile::new().unwrap();
    let unlinked = settle(blocked.path(), 202, &reply, Some(&capability), |_| {
        panic!("a success shown without its link")
    });
    assert!(unlinked.is_err());
    // A stored request Discord refused is still linked, so its refusal can be read back.
    let refused = (uuid::Uuid::new_v4(), json!("decision_post_forbidden"));
    let reply = json!({"ok": false, "error": refused.1, "request_id": refused.0});
    assert!(settle(root.path(), 502, &reply, Some(&capability), |_| {}).is_err());
    assert!(owner_cache::of_request(root.path(), refused.0).is_ok());
}

/// One channel is one scope however its id is spelled, and a name that is no id is refused
/// before any capability exists.
#[test]
fn decision_ask_scope_is_the_channel_id_not_its_spelling() {
    let root = tempfile::tempdir().unwrap();
    let body = |channel: &str| json!({"agent_id": "asker", "target": {"channel_id": channel}, "ref": "r1"});
    let send = |capability: &str| Ok::<_, String>(capability.to_string());
    let plain = ask(root.path(), &body("101"), send).unwrap();
    assert_eq!(ask(root.path(), &body("00101"), send).unwrap(), plain);
    assert!(ask(root.path(), &body("general"), send).is_err());
    assert_eq!(cached(root.path()).len(), 1);
}

/// Asks that start together for one new request all present the same capability: nobody reads
/// a capability file that is still being written.
#[test]
fn decision_asks_that_race_for_a_new_capability_agree_on_one() {
    let root = tempfile::tempdir().unwrap();
    for round in 0..64 {
        let key = format!("race-{round}");
        let body = json!({"agent_id": "asker", "target": {"channel_id": "7"}, "ref": key});
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let (start, root, body) = (start.clone(), root.path().to_path_buf(), body.clone());
                std::thread::spawn(move || {
                    start.wait();
                    ask(&root, &body, |capability| Ok(capability.to_string()))
                })
            })
            .collect();
        let cap = std::time::Duration::from_secs(30);
        let sent: Vec<Result<String, String>> = (racers.into_iter())
            .map(|racer| crate::test_support::wait::join_within(racer, cap))
            .collect();
        let first = sent[0].clone().expect("the first asker got a capability");
        assert!(
            sent.iter().all(|one| one.as_ref() == Ok(&first)),
            "{round}: {sent:?}"
        );
    }
}
