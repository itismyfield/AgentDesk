use super::*;

#[test]
fn a_deferred_activation_waits_for_an_actual_legacy_body_send() {
    let channel = Channel::new(67371001);
    let store = channel.store();
    let snapshot = channel.pin().unwrap();
    assert!(channel.candidate.defer(channel.channel));
    let claimed = channel.candidate.claim_body(channel.channel);
    assert!(!claimed.owned);
    let send = claimed.send.expect("Legacy owns a real body reservation");
    assert_eq!(channel.candidate.sends(), (1, 0));

    let detail = channel
        .activate(&store, &snapshot)
        .expect_err("an in-flight Legacy body prevents init publication");
    assert_eq!(detail, "Legacy body send in flight");
    assert_eq!(channel.candidate.peek(), Adoption::Deferred);
    assert!(!store.has_channel_dir(channel.channel));
    assert_eq!(store.read_init(channel.channel).unwrap(), None);

    drop(send);
    assert_eq!(channel.candidate.sends(), (1, 1));
    channel.activate(&store, &snapshot).unwrap();
    assert_eq!(channel.candidate.peek(), Adoption::Committed);
    let init = store.read_init(channel.channel).unwrap().unwrap();
    assert_eq!(init.sources.len(), 1);
    assert_eq!(init.sources[0].source_id, channel.source);
    assert_eq!(init.sources[0].delivery_start, snapshot.start());
}

#[test]
fn a_pending_body_claim_preserves_the_already_released_refusal() {
    let channel = Channel::new(67371002);
    let store = channel.store();
    let snapshot = channel.pin().unwrap();
    assert_eq!(channel.candidate.peek(), Adoption::Pending);
    let claimed = channel.candidate.claim_body(channel.channel);
    assert!(!claimed.owned);
    let send = claimed.send.expect("the body claim releases Pending");
    assert_eq!(channel.candidate.peek(), Adoption::Released);
    assert_eq!(channel.candidate.sends(), (1, 0));

    let detail = channel.activate(&store, &snapshot).unwrap_err();
    assert_eq!(detail, "adoption is already Released");
    assert_eq!(channel.candidate.peek(), Adoption::Released);
    assert!(!store.has_channel_dir(channel.channel));
    assert_eq!(store.read_init(channel.channel).unwrap(), None);
    drop(send);
    assert_eq!(channel.candidate.sends(), (1, 1));
}

#[test]
fn a_past_stall_recheck_relaxes_only_the_running_tail() {
    let (channel, _) = rotated(67371003, 2);
    let snapshot = channel.pin().unwrap();
    let expected = snapshot
        .recheck(&channel.legacy, &channel.log, channel.channel)
        .unwrap();
    assert_eq!(expected.len(), 3);
    channel.legacy.tail.store(true, Ordering::Release);
    let normal = snapshot
        .recheck(&channel.legacy, &channel.log, channel.channel)
        .unwrap_err();
    assert_eq!(normal.to_string(), "a Legacy response tail is running");
    let actual = snapshot
        .recheck_past_stall(&channel.legacy, &channel.log, channel.channel)
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(channel.candidate.peek(), Adoption::Pending);
}

fn past_stall_refused(channel: &Channel, snapshot: &Snapshot, detail: &str) -> Refused {
    channel.legacy.tail.store(true, Ordering::Release);
    let refused = snapshot
        .recheck_past_stall(&channel.legacy, &channel.log, channel.channel)
        .expect_err("a past-stall recheck keeps source and binding safety checks");
    let actual = refused.to_string();
    assert!(actual.contains(detail), "expected {detail}, got {actual}");
    refused
}

#[test]
fn a_past_stall_recheck_refuses_a_moved_binding_seq() {
    let mut channel = Channel::new(67371004);
    let snapshot = channel.pin().unwrap();
    channel.log.0.push(bound(
        2,
        channel.channel,
        Some(&channel.source),
        &channel.source,
    ));
    let refused = past_stall_refused(&channel, &snapshot, "binding log moved past seq 1");
    assert_eq!(refused.hold, Hold::Binding);
}

fn replace_source(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::metadata(path).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, &bytes).unwrap();
    std::fs::rename(replacement, path).unwrap();
    let after = std::fs::metadata(path).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_ne!((before.dev(), before.ino()), (after.dev(), after.ino()));
}

#[test]
fn a_past_stall_recheck_refuses_current_source_replacement() {
    let channel = Channel::new(67371005);
    let snapshot = channel.pin().unwrap();
    replace_source(&channel.source.path);
    past_stall_refused(&channel, &snapshot, "was replaced");
}

#[test]
fn a_past_stall_recheck_refuses_past_source_replacement() {
    let (channel, past) = rotated(67371006, 1);
    let snapshot = channel.pin().unwrap();
    replace_source(&past[0].path);
    past_stall_refused(&channel, &snapshot, "was replaced");
}

#[test]
fn a_past_stall_recheck_refuses_current_source_growth() {
    let channel = Channel::new(67371007);
    let snapshot = channel.pin().unwrap();
    append(&channel.source.path, &turn("late-current"));
    past_stall_refused(&channel, &snapshot, "length moved");
}

#[test]
fn a_past_stall_recheck_refuses_past_source_growth() {
    let (channel, past) = rotated(67371008, 1);
    let snapshot = channel.pin().unwrap();
    append(&past[0].path, &turn("late-past"));
    past_stall_refused(&channel, &snapshot, "length moved");
}

#[test]
fn a_past_stall_recheck_keeps_named_sources_empty() {
    let mut channel = Channel::new(67371009);
    let path = channel.dir.path().join("named.jsonl");
    std::fs::write(&path, b"").unwrap();
    let named = source_id_for("named", &path).unwrap();
    let BindingRecord::Bound { parent_hint, .. } = &mut channel.log.0[0].record else {
        panic!("fixture binds its current source");
    };
    *parent_hint = Some(named);
    let snapshot = channel.pin().unwrap();
    append(&path, &turn("named-became-live"));
    let refused = past_stall_refused(&channel, &snapshot, "already holds");
    assert_eq!(refused.hold, Hold::Final);
}

#[test]
fn body_start_is_published_while_the_candidate_lock_is_held() {
    let channel = 67371010;
    let candidate = Candidate::new(Adoption::Deferred);
    let observer = candidate.clone();
    test_hook::set(channel, test_hook::Step::BeforeLegacyStarted, move || {
        let locked = std::thread::spawn(move || observer.locked_for_test())
            .join()
            .unwrap();
        assert!(
            locked,
            "the body claim must hold the candidate lock before publishing started"
        );
        Ok(())
    });

    let claimed = candidate.claim_body(channel);
    assert!(!claimed.owned);
    let send = claimed.send.expect("Deferred holds an actual body send");
    assert_eq!(candidate.peek(), Adoption::Deferred);
    assert_eq!(candidate.sends(), (1, 0));
    drop(send);
    assert_eq!(candidate.sends(), (1, 1));
}
