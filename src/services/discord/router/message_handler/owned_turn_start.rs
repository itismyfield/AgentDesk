//! The managed birth paired with an owned queue attempt; neither callback follows the channel head.
use crate::services::claude_tui::{busy_inject::Outcome, owner_input::NotSentProof};
use crate::services::discord::inflight::{self, InflightTurnState, ManagedSubmission};
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use serde_json::Value;

/// Where the PG owner start permit is spent once its junction lands: `prepare` before the leaf,
/// `submit(frame, native_binding, raw)` around its first payload. [`NoStartPermit`] runs as main.
pub(crate) trait StartPermit {
    fn submit<T>(self, frame: &str, native_binding: Option<&Value>, raw: impl FnOnce() -> T) -> T;
}

/// No owner start junction: the leaf runs unwrapped.
pub(crate) struct NoStartPermit;

impl StartPermit for NoStartPermit {
    fn submit<T>(self, _frame: &str, _native_binding: Option<&Value>, raw: impl FnOnce() -> T) -> T {
        raw()
    }
}

pub(crate) struct OwnedTurnStart {
    submission: ManagedSubmission,
    key: Value,
    ran: bool,
    ended: bool,
}

impl OwnedTurnStart {
    pub(crate) fn install(state: &mut InflightTurnState, host: &TurnHost, key: Value) -> Result<Self, String> {
        if key["identity"]["primary"].as_u64() != Some(state.user_msg_id)
            || key["attempt"]["nonce"].as_str() != state.turn_nonce.as_deref()
            || key["attempt"]["provider"].as_str() != Some(state.provider.as_str())
            || key["attempt"]["target"]["owner"].as_str() != state.tmux_session_name.as_deref()
        {
            return Err("queue attempt does not name this managed birth".into());
        }
        let submission = inflight::install_managed_submission_boundary(state, host)
            .ok_or("this turn has no managed submission boundary")?;
        Ok(Self { submission, key, ran: false, ended: false })
    }

    pub(crate) fn key(&self) -> &Value {
        &self.key
    }

    pub(crate) fn fence(&self) -> std::sync::Arc<dyn crate::services::claude_tui::submission_fence::SubmissionFence> {
        self.submission.fence()
    }

    /// The actual leaf supplies the result; only its live no-content proof ends this birth.
    pub(crate) fn run(
        &mut self,
        permit: impl StartPermit,
        frame: &str,
        leaf: impl FnOnce() -> Outcome,
    ) -> Result<Outcome, String> {
        if self.ran {
            return Err("owned start already spent".into());
        }
        self.ran = true;
        let (outcome, proof) = self.submission.run_leaf(|| permit.submit(frame, None, leaf));
        if let Some(proof) = proof {
            self.submission.conclude_not_submitted(proof)?;
            self.ended = true;
        }
        Ok(outcome)
    }

    /// The leaf has returned its writer before this handoff. Both durable attempts keep the
    /// original; an incomplete handoff returns no capability and never re-admits it.
    pub(crate) async fn successor(
        &self,
        handle: &ChannelMailboxHandle,
        proof: NotSentProof,
    ) -> Result<Self, String> {
        if !self.ended {
            return Err("owned start has not ended".into());
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let next_nonce = nonce.clone();
        let key = self.key.clone();
        let next = handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no input owner")?;
            let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
            let successor = owner.successor_not_sent(&key, proof, next_nonce)?;
            serde_json::to_value(successor).map_err(|e| e.to_string())
        }).await?;
        let submission = self.submission.successor(nonce)?;
        Ok(Self { submission, key: next, ran: false, ended: false })
    }
}

#[cfg(unix)]
mod tests {
    use super::*;
    use crate::services::claude_tui::busy_inject::{Veto, WriteFailure};
    use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
    use crate::services::claude_tui::owner_input::{self, Commit};
    use crate::services::discord::queue_io::input_admission::{self as admission, AdmissionTicket, TurnBind};
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
    use poise::serenity_prelude::ChannelId;
    use serde_json::json;
    use std::sync::Arc;

    const CHANNEL: u64 = 727_461;
    const NAME: &str = "s4e1-successor";

    fn screen() -> String {
        crate::services::claude_tui::input::submission::tests::screen("")
    }

    fn row() -> InflightTurnState {
        let mut row = InflightTurnState::new(
            ProviderKind::Claude, CHANNEL, None, 7, CHANNEL + 1, CHANNEL + 2,
            "same raw request".into(), None, Some(NAME.into()), None, None, 0,
        );
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        row.turn_nonce = Some("first-birth".into());
        row
    }

    async fn snapshot(handle: &ChannelMailboxHandle) -> Value {
        handle.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap()
    }

    /// The real Claude input leaf runs on the isolated tmux server and produces both proofs.
    async fn leaf(
        handle: &ChannelMailboxHandle,
        mut start: OwnedTurnStart,
        target: Value,
    ) -> (OwnedTurnStart, Outcome, Option<NotSentProof>) {
        let actor = handle.clone();
        let key = start.key.clone();
        let lease = handle.v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
            owner.arm_bootstrap(&key, 1)?.consume(&key)
        }).await.unwrap();
        let key = start.key.clone();
        let runtime = tokio::runtime::Handle::current();
        let fence = start.fence();
        let commit: Commit = Arc::new(move |_| {
            fence.before_first_payload().map_err(|error| error.to_string())?;
            let key = key.clone();
            runtime.block_on(actor.v2_request(move |slot| {
                let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
                slot.as_mut().unwrap().payload_started(&key, 2)
            }))
        });
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let _spy = SpyGuard::install(SpyState {
                captures: std::iter::repeat_n(Some(screen()), 24).collect(), ..Default::default()
            });
            let mut owner_proof = None;
            let result = start.run(NoStartPermit, "same raw request", || {
                let (result, proof) = owner_input::capture_scope(
                    "claude".into(), target, CHANNEL, commit,
                    Arc::new(|_, _| Ok(())), Arc::new(|_| Ok(())),
                    || crate::services::claude_tui::input::send_fresh_prompt(
                        NAME, "same raw request", Some(&CancelToken::new()),
                    ),
                );
                owner_proof = proof;
                match result {
                    Ok(()) => Outcome::Injected,
                    Err(_) if owner_proof.is_some() => Outcome::NotSent(Veto::LoadFailed),
                    Err(error) => { eprintln!("actual leaf: {error}"); Outcome::Failed(WriteFailure::Paste) },
                }
            }).unwrap();
            (start, result, owner_proof)
        }).await.unwrap()
    }

    #[tokio::test]
    async fn load_failure_same_ticket_new_birth_runs_once_and_old_callbacks_cannot_touch_it() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let fixture = Fixture::after_shared_test_env_lock();
        fixture.session_with_screen(NAME, &screen());
        let shim = fixture.root.path().join("tmux");
        let trace = fixture.root.path().join("trace");
        let fail = fixture.root.path().join("fail-load");
        let script = std::fs::read_to_string(&shim).unwrap();
        let prefix = format!("printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in *load-buffer*) [ ! -f '{}' ] || exit 1 ;; esac\n", trace.display(), fail.display());
        std::fs::write(&shim, script.replacen("for arg", &(prefix + "for arg"), 1)).unwrap();
        std::fs::write(&fail, "fail").unwrap();
        let execution = "72724617272461727246172724617272";
        let owner_root = crate::services::tmux_common::current_tmux_owner_marker();
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
                "execution_nonce":execution,"tmux_session":NAME,"channel_id":CHANNEL,
                "owner_runtime_root":owner_root,"host":"test","expected_native_session_id":null,
                "launch_mode":"fresh","provider_root":null})).unwrap(),
        ).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(NAME, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, execution).unwrap();
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(CHANNEL));
        let path = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap().join("successor.json");
        handle.v2_adopt(path.clone(), "successor".into()).await.unwrap();
        let ticket = AdmissionTicket::new();
        let original: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7,"message_id":CHANNEL+1,"channel_id":CHANNEL,
            "queued_generation":5,"source_message_ids":[CHANNEL+1],"text":"same raw request"
        })).unwrap();
        assert!(matches!(admission::admit(&handle, &original, ticket.clone()).await, admission::Admission::Accepted(_)));
        let target = json!({"host":"tmux","owner":NAME,"root":owner_root,"execution_nonce":execution});
        let attempted = json!({"nonce":"first-birth","provider":"claude","target":target,
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
        let key = handle.v2_request(move |slot| {
            let attempt = serde_json::from_value(attempted).map_err(|e| e.to_string())?;
            let payload = serde_json::from_value(json!({"frame":"same raw request","digest":"exact","attribution":{}})).unwrap();
            serde_json::to_value(slot.as_mut().unwrap().reserve(attempt, payload)?).map_err(|e| e.to_string())
        }).await.unwrap();
        let mut born = row();
        let start = OwnedTurnStart::install(&mut born, &TurnHost::Tmux, key.clone()).unwrap();
        inflight::save_inflight_state_create_new(&born).unwrap();
        let before = snapshot(&handle).await;
        let mut scoped = target.clone(); scoped["_attempt"] = key.clone();
        let (mut first, result, proof) = leaf(&handle, start, scoped).await;
        assert_eq!(result, Outcome::NotSent(Veto::LoadFailed));
        assert!(proof.is_some());
        let ended = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        assert_eq!(ended.managed_submission.as_ref().unwrap().phase, inflight::SubmissionPhase::NotSubmitted);
        assert!(first.fence().before_first_payload().is_err());
        assert!(first.run(NoStartPermit, "", || panic!("old start cannot type")).is_err());
        std::fs::remove_file(&fail).unwrap();
        let next = first.successor(&handle, proof.unwrap()).await.unwrap();
        let next_key = next.key().clone();
        assert_ne!(next_key["attempt"]["nonce"], key["attempt"]["nonce"]);
        let reserved = snapshot(&handle).await;
        assert_eq!(reserved["items"], before["items"]);
        assert_eq!(reserved["admission_receipts"], before["admission_receipts"]);
        assert_eq!(reserved["admission_sequence"], before["admission_sequence"]);
        let successor_row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        assert_eq!(successor_row.turn_nonce.as_deref(), next_key["attempt"]["nonce"].as_str());
        for old in [&born, &ended] {
            assert_eq!(inflight::save_inflight_state_if_identity_unchanged(old, "owned_start_stale"), inflight::GuardedSaveOutcome::SuccessorOwned);
        }
        assert!(first.fence().before_first_payload().is_err());
        let stale = key.clone();
        assert!(handle.v2_request(move |slot| {
            let old = serde_json::from_value(stale).unwrap();
            slot.as_mut().unwrap().payload_started(&old, 50)
        }).await.is_err());
        assert_eq!(snapshot(&handle).await["head_injection"], reserved["head_injection"]);
        let later: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7,"message_id":CHANNEL+9,"channel_id":CHANNEL,
            "queued_generation":5,"source_message_ids":[CHANNEL+9],"text":"arrived after"
        })).unwrap();
        assert!(matches!(admission::admit(&handle, &later, AdmissionTicket::new()).await, admission::Admission::Accepted(_)));
        let ahead = json!({"nonce":"overtake","provider":"claude","target":target.clone(),
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
        assert!(handle.v2_request(move |slot| {
            let attempt = serde_json::from_value(ahead).map_err(|e| e.to_string())?;
            slot.as_mut().unwrap().reserve_unit(attempt, |_| Some((String::new(), String::new()))).map(drop)
        }).await.is_err(), "nothing runs ahead of the standing successor");
        let mut scoped = target; scoped["_attempt"] = next_key.clone();
        let (_next, result, proof) = leaf(&handle, next, scoped).await;
        assert_eq!(result, Outcome::Injected);
        assert!(proof.is_none());
        let trace = std::fs::read_to_string(trace).unwrap();
        assert_eq!(trace.lines().filter(|line| line.contains("paste-buffer")).count(), 1, "one native content transaction");
        assert_eq!(trace.lines().filter(|line| line.contains(" Enter")).count(), 1);
        assert_eq!(snapshot(&handle).await["admission_sequence"], 2);
        let armed = |bind: TurnBind| matches!(bind, TurnBind::Armed(turn) if turn.attempt == next_key["attempt"]["nonce"].as_str().unwrap());
        assert!(armed(admission::exact_turn(&handle, &ticket, &original).await.unwrap()), "the ticket follows the successor");
        handle.v2_adopt(path, "successor".into()).await.unwrap();
        assert!(armed(admission::exact_turn(&handle, &ticket, &original).await.unwrap()), "and keeps it across a restart");
    }
}
