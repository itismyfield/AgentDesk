use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::provider::CancelToken;
use crate::services::tui_o::shadow::{SourceId, binding_reader::source_id_for};
use crate::services::tui_o::store::{InitOrigin, InitSource, OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{
    self, ActivationFacts,
    test_hook::{self, Step},
};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};
use crate::test_support::wait::{WAIT_CAP, await_within, join_within, recv_within};
use sha2::Digest;
use std::sync::mpsc;

struct Fixture {
    candidate: Candidate,
    token: CancelToken,
    admission: Option<FirstAdmission>,
    prepared: PreparedIncarnation,
    source: SourceId,
    store: Arc<OStore>,
    channel: u64,
    // The global lock outlives all source operations; the env guard outlives that lock.
    dedupe_guard: Option<std::sync::MutexGuard<'static, ()>>,
    root: Option<crate::config::TestRuntimeRootGuard>,
}

impl Fixture {
    fn new(channel: u64) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let dedupe_guard = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        dedupe::reset_state_for_tests();
        let runtime = crate::config::runtime_root().unwrap();
        let tmux = format!("first-proof-{channel}");
        let sessions = runtime.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let prepared = PreparedIncarnation::prepare_pinned(
            "codex",
            &tmux,
            Some(channel),
            None,
            false,
            Some(sessions.canonicalize().unwrap()),
            (Some(first_prompt_digest("hi")), Some("legacy".into())),
        )
        .unwrap();
        let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux,
            "spawn_nonce",
        ));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, &prepared.context.execution_nonce).unwrap();
        let path = sessions.join("rollout.jsonl");
        std::fs::write(
            &path,
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
        )
        .unwrap();
        let source = source_id_for("s", &path).unwrap();
        let candidate = Candidate::reserved_late();
        let token = CancelToken::new();
        let admission = candidate
            .admit_first(channel, token.turn_nonce().unwrap(), "hi", &tmux, || Ok(0))
            .unwrap();
        candidate.attach_first_execution(&prepared.context, "hi", token.turn_nonce());
        dedupe::register_tmux_channel(&tmux, channel);
        dedupe::register_tmux_runtime_binding(&tmux, binding(&source));
        let store = Arc::new(
            OStore::open_if_enabled(&StoreConfig { enabled: true }, &runtime)
                .unwrap()
                .unwrap(),
        );
        Self {
            candidate,
            token,
            admission: Some(admission),
            prepared,
            source,
            store,
            channel,
            dedupe_guard: Some(dedupe_guard),
            root: Some(root),
        }
    }
    fn proof(&self) -> FirstProof {
        let guard = self.candidate.lock();
        let proof = self.candidate.first_proof(&self.source).unwrap().unwrap();
        drop(guard);
        proof
    }
    fn activate(&self) -> Result<(), String> {
        activate(&self.store, self.channel, &self.candidate, &self.proof())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.admission.take());
        self.candidate.invalidate_first(None, None);
        dedupe::reset_state_for_tests();
        drop(self.dedupe_guard.take());
        drop(self.root.take());
    }
}

fn binding(source: &SourceId) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: CodexTui,
        output_path: source.path.display().to_string(),
        session_id: Some(source.session_id.clone()),
        relay_output_path: None,
        input_fifo_path: None,
        last_offset: 0,
        relay_last_offset: None,
    }
}
fn activate(
    store: &OStore,
    channel: u64,
    candidate: &Candidate,
    proof: &FirstProof,
) -> Result<(), String> {
    activation::activate_takeover_proved(
        store,
        channel,
        (ActivationFacts::default(), 0),
        || Ok(false),
        candidate,
        || {
            if candidate.first_proof(&proof.source)?.as_ref() != Some(proof) {
                return Err("admission proof changed".into());
            }
            Ok(vec![InitSource {
                source_id: proof.source.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
            }])
        },
        (InitOrigin::LateCodexTakeover, Some(proof)),
    )
}

#[tokio::test]
async fn admission_holds_until_refusal_and_old_tokens_do_not_release_a_new_incarnation() {
    let f = Fixture::new(8_165_101);
    let mut waiting = Box::pin(
        f.candidate
            .wait_first(f.token.turn_nonce().map(first_nonce_id)),
    );
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(waiting.as_mut(), &mut context).is_pending());
    f.candidate.invalidate_first(Some("old-token"), None);
    assert!(std::future::Future::poll(waiting.as_mut(), &mut context).is_pending());
    f.candidate.invalidate_first(f.token.turn_nonce(), None);
    await_within(waiting, WAIT_CAP).await;
    assert!(f.candidate.first_proof(&f.source).is_err());
    let token = CancelToken::new();
    let next = f
        .candidate
        .admit_first(
            f.channel,
            token.turn_nonce().unwrap(),
            "hi",
            &f.prepared.context.tmux_session,
            || Ok(9),
        )
        .unwrap();
    let successor = PreparedIncarnation::prepare_pinned(
        "codex",
        &f.prepared.context.tmux_session,
        Some(f.channel),
        None,
        false,
        f.prepared.context.provider_root.clone(),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    assert_ne!(
        successor.context.execution_nonce,
        f.prepared.context.execution_nonce
    );
    f.candidate
        .attach_first_execution(&successor.context, "hi", f.token.turn_nonce());
    assert!(f.candidate.first_proof(&f.source).is_err());
    f.candidate
        .attach_first_execution(&successor.context, "hi", token.turn_nonce());
    f.candidate.invalidate_first(f.token.turn_nonce(), None);
    let proof = f.proof();
    assert_eq!(
        proof.context.execution_nonce,
        successor.context.execution_nonce
    );
    assert_eq!(proof.nonce, token.turn_nonce().unwrap());
    assert_eq!(proof.mark.seq, 9);
    drop(next);
    await_within(
        f.candidate
            .wait_first(token.turn_nonce().map(first_nonce_id)),
        WAIT_CAP,
    )
    .await;
}

#[test]
fn execution_and_prompt_proof_fail_closed_before_publication() {
    for change in ["prompt", "nonce", "mapping", "source-id", "incarnation"] {
        let f = Fixture::new(8_165_102);
        let proof = f.proof();
        match change {
            "prompt" => f.candidate.attach_first_execution(
                &f.prepared.context,
                "different",
                f.token.turn_nonce(),
            ),
            "nonce" => {
                std::fs::write(
                    crate::services::tmux_common::session_temp_path(
                        &f.prepared.context.tmux_session,
                        "spawn_nonce",
                    ),
                    "retired",
                )
                .unwrap();
            }
            "mapping" => {
                assert!(dedupe::evict_dead_tmux_mirror(
                    &f.prepared.context.tmux_session
                ));
            }
            "source-id" => {
                let mut wrong = binding(&f.source);
                wrong.session_id = Some("other-session".into());
                dedupe::register_tmux_runtime_binding(&f.prepared.context.tmux_session, wrong);
            }
            "incarnation" => {
                std::fs::remove_file(&f.prepared.path).unwrap();
            }
            _ => unreachable!(),
        }
        let result = activate(&f.store, f.channel, &f.candidate, &proof);
        assert!(result.is_err(), "{change}");
        assert!(f.store.read_init(f.channel).unwrap().is_none(), "{change}");
    }
}

#[test]
fn source_authority_spans_final_check_write_and_committed_for_each_publisher() {
    for publisher in ["rebind", "incarnation", "evict"] {
        let f = Fixture::new(8_165_103);
        let proof = f.proof();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        test_hook::set(f.channel, Step::BeforeWrite, move || {
            entered_tx.send(()).unwrap();
            recv_within(&release_rx, WAIT_CAP);
            Ok(())
        });
        let (candidate, store, channel) = (f.candidate.clone(), f.store.clone(), f.channel);
        let activating = std::thread::spawn(move || activate(&store, channel, &candidate, &proof));
        recv_within(&entered_rx, WAIT_CAP);
        let tmux = f.prepared.context.tmux_session.clone();
        let protected =
            crate::services::tmux_common::try_with_tmux_source_authority(&tmux, |_| ()).is_none();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let context = f.prepared.context.clone();
        let source = f.source.clone();
        let changing = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            match publisher {
                "evict" => {
                    assert!(dedupe::evict_dead_tmux_mirror(&tmux));
                }
                "rebind" => {
                    let path = source.path.with_file_name("successor.jsonl");
                    std::fs::write(
                        &path,
                        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"successor\"}}\n",
                    )
                    .unwrap();
                    let next = source_id_for("successor", &path).unwrap();
                    dedupe::register_tmux_runtime_binding(&tmux, binding(&next));
                    assert_eq!(
                        dedupe::runtime_binding_for_tmux_session(&tmux)
                            .unwrap()
                            .session_id
                            .as_deref(),
                        Some("successor")
                    );
                }
                "incarnation" => {
                    crate::services::tmux_common::with_tmux_source_authority(&tmux, |_| {
                        std::fs::write(
                            crate::services::tmux_common::session_temp_path(
                                &context.tmux_session,
                                "spawn_nonce",
                            ),
                            "successor",
                        )
                        .unwrap()
                    })
                }
                _ => unreachable!(),
            }
        });
        recv_within(&attempt_rx, WAIT_CAP);
        release_tx.send(()).unwrap();
        join_within(activating, WAIT_CAP).unwrap();
        join_within(changing, WAIT_CAP);
        assert!(
            protected,
            "{publisher}: authority must still be held at BeforeWrite"
        );
        assert_eq!(f.candidate.peek(), Adoption::Committed);
        assert_eq!(
            f.store.read_init(f.channel).unwrap().unwrap().sources[0].source_id,
            f.source
        );
    }
}

#[test]
fn restart_after_publication_restores_the_init_without_a_second_write() {
    for step in [Step::BeforeWrite, Step::AfterWrite] {
        let f = Fixture::new(8_165_104);
        test_hook::set(f.channel, step, || {
            Err("crash at publication boundary".into())
        });
        let result = f.activate();
        let recovered = super::stored(
            Some(&crate::config::runtime_root().unwrap()),
            &BTreeSet::from([f.channel]),
        );
        if step == Step::BeforeWrite {
            assert!(result.is_err());
            assert_eq!(recovered[&f.channel], Adoption::Pending);
            assert!(f.store.read_init(f.channel).unwrap().is_none());
        } else {
            result.unwrap();
            assert_eq!(recovered[&f.channel], Adoption::Committed);
            let before =
                serde_json::to_vec(&f.store.read_init(f.channel).unwrap().unwrap()).unwrap();
            assert!(f.activate().is_err());
            assert_eq!(
                serde_json::to_vec(&f.store.read_init(f.channel).unwrap().unwrap()).unwrap(),
                before
            );
        }
    }
}

#[tokio::test]
async fn admission_boundary_survives_a_later_host_baseline_and_unknown_bodies_keep_legacy_custody()
{
    let candidate = Candidate::reserved_late();
    let token = CancelToken::new();
    let admission = candidate
        .admit_first(
            8_165_105,
            token.turn_nonce().unwrap(),
            "hi",
            "fresh",
            || {
                let guard = candidate.lock();
                candidate.baseline_sends();
                drop(guard);
                Ok(7)
            },
        )
        .unwrap();
    assert_eq!(candidate.first_mark().unwrap().seq, 7);
    assert!(candidate.first_ready());
    await_within(candidate.wait_first(None), WAIT_CAP).await;
    let old = candidate.claim_body(8_165_105);
    assert!(!old.owned);
    assert!(old.send.is_some());
    drop(old);
    assert_eq!(candidate.sends(), (1, 1));
    drop(admission);
    await_within(
        candidate.wait_first(token.turn_nonce().map(first_nonce_id)),
        WAIT_CAP,
    )
    .await;
}

#[test]
fn a_fenced_queued_body_refuses_the_proved_first_init() {
    let f = Fixture::new(8_165_106);
    let proof = f.proof();
    let result = activation::activate_takeover_proved(
        &f.store,
        f.channel,
        (ActivationFacts::default(), 1),
        || Ok(false),
        &f.candidate,
        || {
            Ok(vec![InitSource {
                source_id: f.source.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
            }])
        },
        (InitOrigin::LateCodexTakeover, Some(&proof)),
    );
    assert!(result.unwrap_err().contains("queued Legacy bodies"));
    assert!(f.store.read_init(f.channel).unwrap().is_none());
    assert_eq!(f.candidate.peek(), Adoption::Deferred);
}
