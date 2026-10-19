//! Terminal observations keep their exact Pending sequence through O projection.

use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::tmux_common as tc;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_prompt_dedupe::{
    self as dedupe,
    binding_context::{BindingContext, PreparedIncarnation},
};
use p5::codex::{Claim, ClaimEvidence, Decision, Fold};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

const CHANNEL: u64 = 706_101;
const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const OTHER: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";

pub(crate) struct Fixture {
    pub(crate) context: BindingContext,
    pub(crate) source: SourceId,
    root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _shared: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        p5::set_test_root(Some(root.path()));
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: format!("terminal-{}", uuid::Uuid::new_v4().simple()),
            channel_id: Some(CHANNEL),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some(ID.into()),
            launch_mode: "resume".into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: None,
            source_policy: Some("verified".into()),
        };
        let path = context
            .provider_root
            .as_ref()
            .unwrap()
            .join("rollout.jsonl");
        fs::write(&path, b"native source\n").unwrap();
        let source = source_id_for(ID, &path).unwrap();
        Self::install(&context);
        dedupe::register_tmux_channel(&context.tmux_session, CHANNEL);
        Self {
            context,
            source,
            root,
            _env: env,
            _dedupe: lock,
            _shared: shared,
        }
    }

    fn install(context: &BindingContext) {
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = tc::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &context.execution_nonce).unwrap();
    }

    fn native(&self, event: &str, source: Option<&str>) -> Claim {
        Claim {
            session_id: ID.into(),
            path: Some(self.source.path.clone()),
            evidence: ClaimEvidence::NativeHook {
                event: event.into(),
                source: source.map(str::to_owned),
                first_prompt_digest: None,
            },
        }
    }

    fn commit(&self, claim: &Claim, decision: Decision) -> Fold {
        tc::with_tmux_source_authority(&self.context.tmux_session, |_| {
            p5::codex::commit_claim(&self.context, claim, decision, || Ok(())).unwrap()
        })
    }

    pub(crate) fn pending(&self) -> (u64, u64) {
        self.commit(
            &self.native("session_start", Some("resume")),
            Decision::Pending,
        );
        let fold = self.commit(&self.native("user_prompt_submit", None), Decision::Pending);
        assert_eq!(fold.pending.len(), 2);
        assert!(fold.verified.is_none());
        (fold.pending[0].seq, fold.pending[1].seq)
    }

    pub(crate) fn settle(&self) -> (u64, u64) {
        let (anchor, ups) = self.pending();
        self.commit(
            &self.native("user_prompt_submit", None),
            Decision::Rejected("superseded_by_executed_resume".into()),
        );
        let claim = Claim {
            session_id: ID.into(),
            path: Some(self.source.path.clone()),
            evidence: ClaimEvidence::ExplicitResume {
                executed_session_id: ID.into(),
            },
        };
        let fold = tc::with_tmux_source_authority(&self.context.tmux_session, |_| {
            p5::codex::commit_executed_resume_published(
                &self.context,
                &claim,
                self.source.clone(),
                || Ok(()),
                |_| Ok(()),
            )
            .unwrap()
        });
        assert!(fold.pending.is_empty() && !fold.conflicted);
        assert_eq!(fold.verified.unwrap().ownership.pending_seq, Some(anchor));
        (anchor, ups)
    }

    pub(crate) fn events(&self) -> Vec<BindingEvent> {
        ChannelBindingLog::new(CHANNEL, ShadowProvider::Codex)
            .binding_events_since(CHANNEL, 0)
            .unwrap()
    }

    fn log_path(&self) -> PathBuf {
        self.root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{CHANNEL}.log"))
    }

    fn remove_marker(&self) {
        fs::remove_file(tc::session_temp_path(
            &self.context.tmux_session,
            "spawn_nonce",
        ))
        .unwrap();
    }

    fn next_incarnation(&self) -> SourceId {
        let context = BindingContext {
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            ..self.context.clone()
        };
        Self::install(&context);
        let path = context.provider_root.as_ref().unwrap().join("next.jsonl");
        fs::write(&path, b"next native source\n").unwrap();
        let source = source_id_for(OTHER, &path).unwrap();
        let claim = Claim {
            session_id: OTHER.into(),
            path: Some(source.path.clone()),
            evidence: ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("startup".into()),
                first_prompt_digest: None,
            },
        };
        let fold = tc::with_tmux_source_authority(&context.tmux_session, |_| {
            p5::codex::commit_claim(&context, &claim, Decision::Verified(source.clone()), || {
                Ok(())
            })
            .unwrap()
        });
        assert_eq!(fold.verified.unwrap().source, source);
        source
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        p5::forget_channel_for_tests(CHANNEL);
        p5::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}

#[test]
fn rejected_closes_keeps_legacy_serde_shape() {
    let old = r#"{"rejected":{"detail":"legacy"}}"#;
    let record: BindingRecord = serde_json::from_str(old).unwrap();
    assert_eq!(
        record,
        BindingRecord::Rejected {
            detail: "legacy".into(),
            closes: None
        }
    );
    assert_eq!(serde_json::to_string(&record).unwrap(), old);
    let terminal = BindingRecord::Rejected {
        detail: "terminal".into(),
        closes: Some(119),
    };
    let encoded = serde_json::to_string(&terminal).unwrap();
    assert_eq!(
        serde_json::from_str::<BindingRecord>(&encoded).unwrap(),
        terminal
    );
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyRecord {
        Rejected { detail: String },
    }
    let LegacyRecord::Rejected { detail } = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        detail, "terminal",
        "an old reader ignores the optional field"
    );
}

fn assert_terminal_events(events: &[BindingEvent], anchor: u64, ups: u64) {
    assert_eq!(events.len(), 4);
    assert!(
        matches!(events[2].record, BindingRecord::Rejected { closes: Some(seq), .. } if seq == ups)
    );
    assert!(
        matches!(events[3].record, BindingRecord::Rejected { closes: Some(seq), .. } if seq == anchor)
    );
}

#[test]
fn p5_rejected_projects_exact_typed_terminal_without_current_nonce() {
    let h = Fixture::new();
    let (anchor, ups) = h.settle();
    let current = h.events();
    assert!(
        matches!(current[2].record, BindingRecord::Rejected { closes: Some(seq), .. } if seq == ups)
    );
    assert!(
        matches!(current[3].record, BindingRecord::Resolved { resolves_seq, .. } if resolves_seq == anchor)
    );
    h.remove_marker();
    assert_terminal_events(&h.events(), anchor, ups);
    let next = h.next_incarnation();
    let events = h.events();
    assert_terminal_events(&events[..4], anchor, ups);
    assert!(
        matches!(&events[4].record, BindingRecord::Bound { new: BindingTarget::Source(source), .. } if source == &next)
    );
    let (bound, named) = crate::services::tui_o::writer::adoption::logged(&events).unwrap();
    assert_eq!(bound, [&next]);
    assert!(named.is_empty());
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 3).unwrap(),
        Some(ups)
    );
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 4).unwrap(),
        None
    );
    let mut denied_source = p5::binding_events_since(CHANNEL, 0).unwrap()[3].clone();
    denied_source.new = p5::BindingTarget::Source(h.source.clone());
    assert!(matches!(
        from_p5(denied_source, false).unwrap().record,
        BindingRecord::Rejected { closes: None, .. }
    ));
}

#[test]
fn typed_terminal_audit_and_corrupt_history_remain_distinct() {
    let h = Fixture::new();
    let (_, ups) = h.pending();
    let audit = Claim {
        session_id: OTHER.into(),
        path: None,
        evidence: ClaimEvidence::NativeHook {
            event: "user_prompt_submit".into(),
            source: None,
            first_prompt_digest: None,
        },
    };
    let fold = h.commit(&audit, Decision::Rejected("audit only".into()));
    assert_eq!(fold.pending.len(), 2);
    let rows: Vec<Value> = fs::read_to_string(h.log_path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(rows[2]["codex_ownership"]["pending_seq"].is_null());
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 3).unwrap(),
        None
    );
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 1).unwrap(),
        None
    );
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 999).unwrap(),
        None
    );
    assert!(matches!(
        h.events()[2].record,
        BindingRecord::Rejected { closes: None, .. }
    ));

    let mut legacy = rows[2].clone();
    legacy["seq"] = json!(4);
    legacy.as_object_mut().unwrap().remove("codex_ownership");
    let mut text = fs::read_to_string(h.log_path()).unwrap();
    text.push_str(&format!("{legacy}\n"));
    fs::write(h.log_path(), text).unwrap();
    p5::forget_channel_for_tests(CHANNEL);
    assert_eq!(
        p5::codex::terminal_pending_at_seq(CHANNEL, 4).unwrap(),
        None
    );
    assert!(matches!(
        h.events()[3].record,
        BindingRecord::Rejected { closes: None, .. }
    ));

    let mut corrupt = rows;
    corrupt[2]["codex_ownership"]["pending_seq"] = json!(ups);
    let text: String = corrupt.iter().map(|row| format!("{row}\n")).collect();
    fs::write(h.log_path(), &text).unwrap();
    p5::forget_channel_for_tests(CHANNEL);
    assert!(p5::codex::terminal_pending_at_seq(CHANNEL, 3).is_err());
    assert!(
        ChannelBindingLog::new(CHANNEL, ShadowProvider::Codex)
            .binding_events_since(CHANNEL, 0)
            .is_err()
    );
    assert_eq!(fs::read_to_string(h.log_path()).unwrap(), text);

    let claude: p5::BindingEvent = serde_json::from_value(legacy).unwrap();
    let claude = p5::BindingEvent {
        provider: "claude".into(),
        ..claude
    };
    assert!(matches!(
        from_p5(claude, false).unwrap().record,
        BindingRecord::Rejected { closes: None, .. }
    ));
}
