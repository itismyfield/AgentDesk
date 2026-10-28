//! Durable resume ACK is the only start evidence a restarted Pending consumes.

use super::*;
use crate::services::claude_tui::hook_server::HookServerState;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_prompt_dedupe as dedupe;
use dedupe::binding_context::{
    self, BindingContext, CapturedContext, ClaudeResumeSource, PreparedIncarnation,
};
use dedupe::binding_events::{
    self, BindingCause, BindingTarget, CauseSource, HookSignal, Proposal, SourceId,
};

const OLD_NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct ResumePending {
    fx: Fixture,
    context: BindingContext,
    old_context: BindingContext,
    started: Started,
    pending: HostedExecution,
    source: SourceId,
    payload: Value,
}

fn envelope(context: &BindingContext) -> String {
    HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: Default::default(),
    }
    .encode()
    .unwrap()
}

impl ResumePending {
    fn new(tag: &str) -> Self {
        let mut fx = Fixture::new(tag, None);
        let session = uuid();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            None,
        )
        .unwrap();
        append(
            &path,
            &[
                json!({"type":"user", "uuid":"old-user", "sessionId":session,
                "timestamp":"2026-10-01T00:00:00Z",
                "message":{"role":"user", "content":"old prompt"}}),
                json!({"type":"assistant", "sessionId":session,
                "timestamp":"2026-10-01T00:00:01Z", "message":{"role":"assistant",
                "content":[{"type":"text", "text":"old answer"}]}}),
                json!({"type":"system", "subtype":"turn_duration", "sessionId":session,
                "timestamp":"2026-10-01T00:00:02Z"}),
            ],
        );
        let source = source_id_for(&session, &path).unwrap();
        let old_context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: OLD_NONCE.into(),
            tmux_session: fx.logical().into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: Some(NODE.into()),
            expected_native_session_id: Some(session.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(fx._home.1.path().join("projects")),
            first_prompt_digest: None,
            source_policy: None,
            claude_resume: None,
        };
        PreparedIncarnation::create(old_context.clone()).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "spawn_nonce");
        std::fs::write(&marker, OLD_NONCE).unwrap();
        dedupe::register_tmux_channel(fx.logical(), CHANNEL);
        dedupe::register_provider_session("claude", &session, fx.logical());
        dedupe::register_tmux_runtime_binding(fx.logical(), claude(&path, &session));
        let old_payload = json!({"source":"startup", "session_id":session, "transcript_path":path});
        let old_hook = HookSignal::from_payload("session_start", &old_payload);
        let path_text = path.to_str().unwrap();
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: "claude",
            tmux_session: fx.logical(),
            session_id: Some(&session),
            path: path_text,
            replaced: None,
            cause: CauseSource::Hook(BindingCause::Startup),
            hook: Some(&old_hook),
        };
        binding_events::record_verified(&proposal, &source).unwrap();
        let context = BindingContext {
            execution_nonce: NONCE.into(),
            launch_mode: "resume".into(),
            claude_resume: Some(ClaudeResumeSource {
                source: source.clone(),
                previous_nonce: OLD_NONCE.into(),
            }),
            ..old_context.clone()
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        std::fs::write(marker, NONCE).unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(fx.logical(), "host_kind"),
            "herdr",
        )
        .unwrap();
        dedupe::install_herdr_execution(fx.logical(), NONCE);
        let location = HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: fx.rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        };
        let pending = pending_on(&fx.owner, NONCE, Some(location), Some(expected(NONCE)));
        fx.store(&pending);
        fx.rig.run_provider(&context_of(NONCE), false);
        fx.ingress.enable_resume_start(context.clone());
        let payload = json!({"source":"resume", "session_id":session, "transcript_path":path});
        let started = Started {
            session,
            path,
            envelope: envelope(&context),
        };
        Self {
            fx,
            context,
            old_context,
            started,
            pending,
            source,
            payload,
        }
    }

    fn ack(&self, payload: &Value, context: Option<&BindingContext>, request: &str) -> u16 {
        let encoded = context.map(envelope);
        self.fx
            .ingress
            .send_envelope(
                &format!(
                    "/hooks/claude/SessionStart?session_id={}",
                    self.started.session
                ),
                payload,
                Some(request),
                encoded.as_deref(),
            )
            .0
    }

    fn starts(&self) -> Vec<binding_events::StrictBindingRecord> {
        binding_events::records_strict_verified(CHANNEL)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|record| {
                record.event.tmux_session == self.fx.logical()
                    && record.event.execution_nonce.as_deref() == Some(NONCE)
                    && record.event.cause == BindingCause::Resume
                    && record.event.evidence.hook_event.as_deref() == Some("session_start")
            })
            .collect()
    }

    fn assert_one_start(&self) {
        let starts = self.starts();
        assert_eq!(
            starts.len(),
            1,
            "one exact durable Resume, including after repeated attach"
        );
        assert!(starts[0].verified);
        assert!(
            matches!(&starts[0].event.new, BindingTarget::Source(source) if source == &self.source)
        );
    }

    fn lose_process_memory(&mut self) {
        dedupe::reset_state_for_tests();
        binding_events::forget_channel_for_tests(CHANNEL);
        self.fx.ingress.state = HookServerState::new();
        self.fx.ingress.enable_resume_start(self.context.clone());
        assert!(dedupe::runtime_binding_for_tmux_session(self.fx.logical()).is_none());
        assert_eq!(self.fx.row(), Some(HostedState::Pending));
        assert_eq!(
            binding_context::execution_context("claude", NONCE).unwrap(),
            self.context
        );
    }
}

#[test]
fn r11_ack_then_restart_consumes_the_same_pending_without_create_and_prompts_once_pg() {
    let mut resume = ResumePending::new("r11-resume-pending");
    let request = uuid();
    assert_eq!(
        resume.ack(&resume.payload, Some(&resume.context), &request),
        202
    );
    resume.fx.started.store(true, Ordering::SeqCst);
    resume.assert_one_start();
    resume.lose_process_memory();
    // Recovery consumes the pinned source, even when the next turn has another cwd.
    resume.fx.cwd = tempfile::tempdir().unwrap();
    // A fresh receipt ledger must recover from the durable log instead of appending a second start.
    assert_eq!(
        resume.ack(&resume.payload, Some(&resume.context), &request),
        202
    );
    resume.assert_one_start();
    let launcher = Arc::new(Launcher::default());
    let ports = resume.fx.ports(&launcher);
    let row = resume.fx.record();
    let (result, messages) = resume
        .fx
        .turn(&row, &ports, || resume.fx.answer(&resume.started));
    assert_eq!(result, Ok(()));
    assert_eq!(resume.fx.row(), Some(HostedState::Bound));
    assert_eq!(
        resume.fx.rig.sends(),
        prompt_sends(),
        "exactly one paste and Enter after restart"
    );
    assert_eq!(*ports.attaches.lock().unwrap(), [(0, true)]);
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        0,
        "never create another pane for Pending"
    );
    assert!(
        launcher.nonces.lock().unwrap().is_empty(),
        "no launch/evidence retry"
    );
    assert!(messages.iter().any(|message| matches!(message,
        StreamMessage::Init { session_id, raw_session_id: Some(raw), .. }
        if session_id == &resume.started.session && raw == &resume.started.session)));
    let binding = dedupe::runtime_binding_for_tmux_session(resume.fx.logical()).unwrap();
    assert_eq!(
        binding.session_id.as_deref(),
        Some(resume.started.session.as_str())
    );
    assert_eq!(Path::new(&binding.output_path), resume.source.path);
    resume.assert_one_start();
}

#[test]
fn r4_resume_attach_reuses_the_exact_event_and_preserves_a_live_cursor_twice_pg() {
    attach_twice(false);
}

#[test]
fn an_expired_exact_binding_is_republished_with_its_cursor_after_resume_start_pg() {
    attach_twice(true);
}

struct ResetDedupeClock;
impl Drop for ResetDedupeClock {
    fn drop(&mut self) {
        dedupe::reset_state_for_tests();
        dedupe::Instant::reset();
    }
}

fn attach_twice(expired: bool) {
    let resume = ResumePending::new("r4-repeat-resume-attach");
    let _reset = ResetDedupeClock;
    assert_eq!(
        resume.ack(&resume.payload, Some(&resume.context), &uuid()),
        202
    );
    resume.assert_one_start();
    let mut live = claude(&resume.started.path, &resume.started.session);
    live.last_offset = 7;
    live.relay_last_offset = Some(3);
    dedupe::register_tmux_runtime_binding(resume.fx.logical(), live.clone());
    if expired {
        dedupe::Instant::advance(Duration::from_secs(24 * 60 * 60 + 1));
        crate::services::tmux_common::with_tmux_source_authority(
            resume.fx.logical(),
            |authority| {
                assert!(
                    matches!(
                        dedupe::runtime_observation_for_kind_under_source_authority(authority, crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
                        dedupe::RuntimeObservation::Expired(ref observed) if observed == &live
                    ),
                    "the actual cache must be expired before attach"
                );
            },
        );
    }
    let launcher = Arc::new(Launcher::default());
    let ports = resume.fx.ports(&launcher);
    let _runtime = resume.fx.rt.enter();
    let _registry = resume.fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let target = herdr_turn::gate(&resume.pending).unwrap();
    let request = AttachRequest {
        owner: &resume.fx.owner,
        record: &resume.pending,
        session_id: &resume.started.session,
        transcript: &resume.started.path,
        target: &target,
        resume_source: Some(&resume.source),
    };
    assert_eq!(
        ports.attach(&request),
        Ok(true),
        "an exact ACKed resume must preserve the live or expired cursor"
    );
    assert_eq!(ports.attach(&request), Ok(true));
    assert_eq!(resume.fx.row(), Some(HostedState::Bound));
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(resume.fx.logical()),
        Some(live)
    );
    assert!(resume.fx.rig.sends().is_empty());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    resume.assert_one_start();
}

#[test]
fn r5_unproved_session_starts_never_wake_pending_attach_or_prompt_pg() {
    let resume = ResumePending::new("r5-unproved-start");
    let mut forged = resume.context.clone();
    forged.tmux_session.push_str("-other");
    let mut wrong_path = resume.payload.clone();
    wrong_path["transcript_path"] = json!(resume.fx.cwd.path().join("wrong.jsonl"));
    let mut wrong_reason = resume.payload.clone();
    wrong_reason["source"] = "startup".into();
    for (payload, context) in [
        (&resume.payload, None),
        (&resume.payload, Some(&resume.old_context)),
        (&resume.payload, Some(&forged)),
        (&wrong_path, Some(&resume.context)),
        (&wrong_reason, Some(&resume.context)),
    ] {
        assert_eq!(resume.ack(payload, context, &uuid()), 425);
        assert!(resume.starts().is_empty());
        let launcher = Arc::new(Launcher::default());
        let ports = resume.fx.ports(&launcher);
        let (result, _) = resume.fx.turn(&resume.fx.record(), &ports, || {
            resume.fx.answer(&resume.started)
        });
        assert!(
            result.is_err(),
            "Pending must retain input until the exact durable SessionStart"
        );
        assert!(ports.attaches.lock().unwrap().is_empty());
        assert!(resume.fx.rig.sends().is_empty());
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
        assert_eq!(resume.fx.row(), Some(HostedState::Pending));
    }
}
