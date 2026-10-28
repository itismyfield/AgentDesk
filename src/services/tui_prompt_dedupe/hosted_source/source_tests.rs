use std::path::PathBuf;
use std::sync::MutexGuard;

use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::tui_prompt_dedupe as dedupe;
use dedupe::binding_events::{CauseSource, Proposal};

pub(crate) const CHANNEL: u64 = 7_186_401;
pub(crate) const LOGICAL: &str = "AgentDesk-claude-source-carrier";
pub(crate) const N1: &str = "0123456789abcdef0123456789abcdef";
pub(crate) const N2: &str = "1123456789abcdef0123456789abcdef";

pub(crate) struct BindingRoot(Option<PathBuf>);

impl BindingRoot {
    pub(crate) fn at(root: &Path) -> Self {
        let previous = binding_events::test_root();
        binding_events::set_test_root(Some(root));
        Self(previous)
    }
}

impl Drop for BindingRoot {
    fn drop(&mut self) {
        binding_events::set_test_root(self.0.as_deref());
    }
}

pub(crate) struct Fixture {
    _state: MutexGuard<'static, ()>,
    _env: TestEnvVarGuard,
    pub(crate) root: tempfile::TempDir,
    previous: Option<PathBuf>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let state = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        dedupe::reset_state_for_tests();
        let previous = binding_events::test_root();
        binding_events::set_test_root(Some(root.path()));
        dedupe::register_tmux_channel(LOGICAL, CHANNEL);
        Self {
            _state: state,
            _env: env,
            root,
            previous,
        }
    }

    pub(crate) fn source(&self, nonce: &str, session: &str) -> SourceId {
        let path = self.root.path().join(format!("{session}.jsonl"));
        std::fs::write(
            &path,
            format!("{{\"type\":\"user\",\"sessionId\":\"{session}\"}}\n"),
        )
        .unwrap();
        let source = source_id_for(session, &path).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, nonce).unwrap();
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: LOGICAL,
            session_id: Some(session),
            path: path.to_str().unwrap(),
            replaced: None,
            cause: CauseSource::Observed,
            hook: None,
        };
        binding_events::record_verified(&proposal, &source).unwrap();
        source
    }

    fn context(&self, nonce: &str, session: &str) -> binding_context::BindingContext {
        let context = binding_context::BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.into(),
            tmux_session: LOGICAL.into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some(session.into()),
            launch_mode: "resume".into(),
            provider_root: Some(self.root.path().into()),
            first_prompt_digest: None,
            source_policy: None,
            claude_resume: None,
        };
        binding_context::PreparedIncarnation::create(context.clone()).unwrap();
        context
    }

    fn recorded_execution_source(&self, nonce: &str, source: &SourceId) {
        use std::io::Write;
        let now = chrono::Utc::now();
        let seq = binding_events::records_strict(CHANNEL)
            .unwrap()
            .unwrap()
            .len() as u64
            + 1;
        let event = binding_events::BindingEvent {
            seq,
            channel_id: CHANNEL,
            provider: "claude".into(),
            tmux_session: LOGICAL.into(),
            execution_nonce: Some(nonce.into()),
            old: None,
            new: BindingTarget::Source(source.clone()),
            cause: binding_events::BindingCause::Resume,
            parent_hint: None,
            evidence: binding_events::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: now,
            },
            committed_at: now,
        };
        let path = self
            .root
            .path()
            .join(binding_events::BINDING_EVENTS_DIR)
            .join(format!("{CHANNEL}.log"));
        let mut log = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        log.sync_all().unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce"),
            nonce,
        )
        .unwrap();
        assert_eq!(
            binding_events::records_strict(CHANNEL)
                .unwrap()
                .unwrap()
                .last()
                .unwrap()
                .execution_nonce
                .as_deref(),
            Some(nonce)
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::set_test_root(self.previous.as_deref());
        dedupe::reset_state_for_tests();
    }
}

#[test]
fn a_source_carrier_keeps_the_original_nonce_after_a_new_source_is_logged() {
    let fx = Fixture::new();
    let original = fx.source(N1, "native-one");
    let pin = capture(
        "claude",
        LOGICAL,
        CHANNEL,
        &original.path,
        Some("native-one"),
    )
    .unwrap();
    assert_eq!(pin.execution_nonce, N1);
    assert_eq!(pin.source, original);
    let next = fx.source(N2, "native-two");
    let restored = restore(
        "claude",
        LOGICAL,
        CHANNEL,
        &(original.path.display().to_string(), 0),
        ("native-one", N1),
    );
    assert_eq!(restored, Some(pin));
    assert!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(original.path.display().to_string(), 0),
            ("native-one", "unrecorded-execution"),
        )
        .is_none()
    );
    assert_eq!(
        capture("claude", LOGICAL, CHANNEL, &next.path, None)
            .unwrap()
            .execution_nonce,
        N2
    );
    assert!(capture("claude", LOGICAL, CHANNEL, &next.path, Some("native-one")).is_none());
}

#[test]
fn a_source_carrier_refuses_replaced_files_and_unreadable_binding_history() {
    let fx = Fixture::new();
    let original = fx.source(N1, "native-one");
    assert!(capture("claude", LOGICAL, CHANNEL, &original.path, None).is_some());
    let displaced = fx.root.path().join("displaced.jsonl");
    std::fs::rename(&original.path, displaced).unwrap();
    std::fs::write(&original.path, b"new file").unwrap();
    assert!(capture("claude", LOGICAL, CHANNEL, &original.path, None).is_none());
    let source = fx.source(N2, "native-two");
    let log = fx
        .root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(log)
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    assert!(capture("claude", LOGICAL, CHANNEL, &source.path, None).is_none());
}

#[test]
fn a_source_reused_by_two_executions_does_not_choose_the_latest_nonce() {
    let fx = Fixture::new();
    let source = fx.source(N1, "native-one");
    let original = capture("claude", LOGICAL, CHANNEL, &source.path, None).unwrap();
    fx.recorded_execution_source(N2, &source);
    assert!(capture("claude", LOGICAL, CHANNEL, &source.path, None).is_none());
    assert_eq!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(source.path.display().to_string(), 0),
            ("native-one", N1),
        ),
        Some(original.clone())
    );
    assert!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(source.path.display().to_string(), 0),
            ("native-one", "unrecorded-execution"),
        )
        .is_none()
    );
    assert_eq!(original.execution_nonce, N1);
}

#[test]
fn a_resume_scanner_requires_its_captured_context_and_the_native_record_time() {
    let fx = Fixture::new();
    let source = fx.source(N1, "native-one");
    let original = capture("claude", LOGICAL, CHANNEL, &source.path, None).unwrap();
    fx.recorded_execution_source(N2, &source);
    assert!(capture("claude", LOGICAL, CHANNEL, &source.path, None).is_none());
    let context = fx.context(N2, "native-one");
    let pin = capture_in_context("claude", LOGICAL, CHANNEL, &source.path, None, &context)
        .expect("the producer captured N2's original launch context");
    assert_eq!(pin.execution_nonce, N2);
    assert!(validate(&pin));
    for created_at in [
        None,
        Some(context.created_at - chrono::TimeDelta::seconds(1)),
    ] {
        assert!(
            capture_scanned(
                "claude",
                LOGICAL,
                CHANNEL,
                &source.path,
                Some(&context),
                created_at
            )
            .is_none()
        );
    }
    assert_eq!(
        capture_scanned(
            "claude",
            LOGICAL,
            CHANNEL,
            &source.path,
            Some(&context),
            Some(context.created_at)
        ),
        Some(pin.clone())
    );
    assert_eq!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(source.path.display().to_string(), 0),
            ("native-one", N1),
        ),
        Some(original)
    );
    assert_eq!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(source.path.display().to_string(), 0),
            ("native-one", N2),
        ),
        Some(pin)
    );
    assert!(
        restore(
            "claude",
            LOGICAL,
            CHANNEL,
            &(source.path.display().to_string(), 0),
            ("native-one", "unrecorded-execution"),
        )
        .is_none()
    );
    let old_context = fx.context(N1, "native-one");
    assert!(
        capture_scanned(
            "claude",
            LOGICAL,
            CHANNEL,
            &source.path,
            Some(&old_context),
            Some(context.created_at)
        )
        .is_none()
    );
}

#[test]
fn a_context_scanner_reports_untimed_records_once_without_assigning_an_execution() {
    const CHILD: &str = "ADK_TEST_UNTIMED_SOURCE_LOG_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::tui_prompt_dedupe::hosted_source::tests::a_context_scanner_reports_untimed_records_once_without_assigning_an_execution",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the isolated untimed source contract failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fx = Fixture::new();
    let source = fx.source(N2, "untimed-native-source");
    let context = fx.context(N2, "untimed-native-source");
    let expected = capture_in_context("claude", LOGICAL, CHANNEL, &source.path, None, &context)
        .expect("the original context and durable N2 source are valid before the untimed scan");
    assert_eq!(
        capture_scanned(
            "claude",
            LOGICAL,
            CHANNEL,
            &source.path,
            Some(&context),
            Some(context.created_at),
        ),
        Some(expected.clone())
    );
    let log_path = fx.root.path().join("untimed-source.log");
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(std::fs::File::create(&log_path).unwrap())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..3 {
            assert!(
                capture_scanned(
                    "claude",
                    LOGICAL,
                    CHANNEL,
                    &source.path,
                    Some(&context),
                    None,
                )
                .is_none(),
                "missing native time cannot assign the current execution to this record"
            );
        }
        assert!(
            capture_scanned(
                "claude",
                LOGICAL,
                CHANNEL,
                &source.path,
                Some(&context),
                Some(context.created_at - chrono::TimeDelta::seconds(1)),
            )
            .is_none()
        );
    });
    let logs = std::fs::read_to_string(log_path).unwrap();
    assert_eq!(
        logs.matches("hosted_source_scanned_record_untimed").count(),
        1,
        "repeated untimed scanner polls emit one diagnostic per process"
    );
    for field in [
        "claude",
        &CHANNEL.to_string(),
        N2,
        "untimed-native-source.jsonl",
    ] {
        assert!(logs.contains(field), "the untimed diagnostic lost {field}");
    }
    assert_eq!(
        capture_scanned("claude", LOGICAL, CHANNEL, &source.path, None, None),
        Some(expected)
    );
}

#[test]
fn a_resume_scanner_rejects_pretime_records_when_only_the_current_execution_is_logged() {
    let fx = Fixture::new();
    let source = fx.source(N2, "native-resumed-only");
    let context = fx.context(N2, "native-resumed-only");
    let history = binding_events::records_strict(CHANNEL).unwrap().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].execution_nonce.as_deref(), Some(N2));
    let pin = capture_in_context("claude", LOGICAL, CHANNEL, &source.path, None, &context).unwrap();
    for created_at in [
        context.created_at,
        context.created_at + chrono::TimeDelta::seconds(1),
    ] {
        assert_eq!(
            capture_scanned(
                "claude",
                LOGICAL,
                CHANNEL,
                &source.path,
                Some(&context),
                Some(created_at),
            ),
            Some(pin.clone()),
            "a normal N2 record remains owned by its captured incarnation"
        );
    }
    assert_eq!(
        capture("claude", LOGICAL, CHANNEL, &source.path, None),
        Some(pin),
        "path-only capture keeps its existing behavior"
    );
    assert!(
        capture_scanned(
            "claude",
            LOGICAL,
            CHANNEL,
            &source.path,
            Some(&context),
            Some(context.created_at - chrono::TimeDelta::seconds(1)),
        )
        .is_none(),
        "an input older than N2 cannot adopt N2 when N1 left no source history"
    );
}

#[test]
fn a_hook_pin_uses_native_payload_source_and_the_supported_session_alias() {
    let fx = Fixture::new();
    let source = fx.source(N1, "native-one");
    fx.source(N2, "native-two");
    let payload = serde_json::json!({"session_id":" ", "sessionId":"native-one",
        "thread_id":"native-two", "transcript_path":source.path});
    let pin = capture_hook("claude", LOGICAL, CHANNEL, &payload, None).unwrap();
    assert_eq!(pin.execution_nonce, N1);
    assert_eq!(pin.source, source);
    assert_eq!(
        capture_hook(
            "claude",
            LOGICAL,
            CHANNEL,
            &serde_json::json!({"session_id":"native-two"}),
            None,
        )
        .unwrap()
        .execution_nonce,
        N2
    );
    assert!(
        capture_hook(
            "claude",
            LOGICAL,
            CHANNEL,
            &serde_json::json!({"thread_id":"native-two"}),
            None,
        )
        .is_none()
    );
}

#[test]
fn a_parked_observation_retains_its_source_until_the_handoff_drains() {
    let fx = Fixture::new();
    let source = fx.source(N1, "native-one");
    let payload =
        serde_json::json!({"session_id": source.session_id, "transcript_path": source.path});
    let reservation = dedupe::reserve_discord_handoff("claude", LOGICAL, "discord origin");
    let mut observed = dedupe::subscribe_observed_prompts();
    let answer = observe_hook_at(
        "claude",
        LOGICAL,
        "parked input",
        (None, None),
        chrono::Utc::now(),
        &payload,
        None,
    );
    assert_eq!(answer, dedupe::PromptObservation::ParkedForHandoff);
    assert!(observed.try_recv().is_err());
    fx.source(N2, "native-two");
    drop(reservation);
    let prompt = observed
        .try_recv()
        .expect("the real handoff drained its parked input");
    let pin = prompt.hosted_source.unwrap();
    assert_eq!(pin.execution_nonce, N1);
    assert_eq!(pin.source, source);
}
