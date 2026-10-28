use super::*;
use std::io::Write;

const CHANNEL: u64 = 7308;
const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        binding_events_root(Some(root.path()));
        Self { root }
    }
    fn path(&self) -> PathBuf {
        self.root.path().join(BINDING_EVENTS_DIR).join("7308.log")
    }
    fn write(&self, bytes: &[u8]) {
        fs::create_dir_all(self.path().parent().unwrap()).unwrap();
        fs::write(self.path(), bytes).unwrap();
    }
}
fn binding_events_root(root: Option<&Path>) {
    super::super::super::set_test_root(root);
}
impl Drop for Fixture {
    fn drop(&mut self) {
        super::super::super::forget_channel_for_tests(CHANNEL);
        binding_events_root(None);
    }
}
fn event(seq: u64, pane: &str) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: "codex".into(),
        tmux_session: pane.into(),
        execution_nonce: Some("a".repeat(32)),
        old: None,
        new: BindingTarget::Pending {
            payload_session_id: ID.into(),
            payload_transcript_path: None,
        },
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    }
}
fn line(event: &BindingEvent) -> Vec<u8> {
    let mut line = serde_json::to_vec(event).unwrap();
    line.push(b'\n');
    line
}

#[test]
fn whole_channel_observation_binds_full_tail_and_refuses_record_budget_truncation() {
    let h = Fixture::new();
    assert!(observe_channel(CHANNEL, 10).is_err());
    let first = event(1, "pane-current");
    let mut bytes = line(&first);
    h.write(&bytes);
    let before = observe_channel(CHANNEL, 1).unwrap();
    assert_eq!(before.events, vec![first]);
    assert_eq!(before.tail, 1);
    bytes.extend(line(&event(2, "pane-other")));
    h.write(&bytes);
    let after = observe_channel(CHANNEL, 2).unwrap();
    assert_eq!(after.events.len(), 2);
    assert_eq!(after.tail, 2);
    assert_ne!(
        after.digest, before.digest,
        "another pane above the old cutoff changes identity"
    );
    assert!(
        observe_channel(CHANNEL, 1).is_err(),
        "a truncated log cannot be Complete"
    );
    assert_eq!(fs::read(h.path()).unwrap(), bytes);
}

#[test]
fn whole_channel_observation_refuses_torn_corrupt_mismatched_and_over_byte_budget_logs() {
    let h = Fixture::new();
    let valid = line(&event(1, "pane-current"));
    let mut bad_channel = event(1, "pane-current");
    bad_channel.channel_id += 1;
    for bytes in [
        valid[..valid.len() - 1].to_vec(),
        b"{}\n".to_vec(),
        line(&event(2, "pane-current")),
        line(&bad_channel),
    ] {
        h.write(&bytes);
        assert!(observe_channel(CHANNEL, 10).is_err());
    }
    let mut exact = valid[..valid.len() - 1].to_vec();
    exact.resize(BYTE_LIMIT as usize - 1, b' ');
    exact.push(b'\n');
    h.write(&exact);
    assert_eq!(observe_channel(CHANNEL, 1).unwrap().tail, 1);
    fs::OpenOptions::new()
        .append(true)
        .open(h.path())
        .unwrap()
        .write_all(b" ")
        .unwrap();
    assert!(observe_channel(CHANNEL, 1).is_err());
}

#[test]
fn whole_channel_observation_validates_typed_ownership_envelopes_alongside_legacy_rows() {
    let h = Fixture::new();
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: "a".repeat(32),
        tmux_session: "pane-current".into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: h.root.path().display().to_string(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(h.root.path().join("sessions")),
        first_prompt_digest: None,
        source_policy: Some("verified".into()),
        claude_resume: None,
    };
    let claim = Claim {
        session_id: ID.into(),
        path: None,
        evidence: ClaimEvidence::NativeHook {
            event: "session_start".into(),
            source: Some("startup".into()),
            first_prompt_digest: None,
        },
    };
    commit_claim(&context, &claim, Decision::Pending, || Ok(())).unwrap();
    let mut bytes = fs::read(h.path()).unwrap();
    bytes.extend(line(&event(2, "pane-other")));
    h.write(&bytes);
    assert_eq!(observe_channel(CHANNEL, 2).unwrap().tail, 2);
    let mut invalid: serde_json::Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    invalid["codex_ownership"]["schema"] = serde_json::json!(2);
    let mut changed = serde_json::to_vec(&invalid).unwrap();
    changed.push(b'\n');
    changed.extend(line(&event(2, "pane-other")));
    h.write(&changed);
    assert!(observe_channel(CHANNEL, 2).is_err());
}
