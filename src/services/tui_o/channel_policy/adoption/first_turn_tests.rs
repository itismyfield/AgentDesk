use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::provider::CancelToken;
use crate::services::tui_o::shadow::{SourceId, binding_reader::source_id_for};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};
use crate::test_support::wait::{WAIT_CAP, await_within};
// Publication tests need an enabled store, which Windows refuses without a directory fsync;
// the store's own Windows test holds that refusal.
#[cfg(unix)]
use {
    crate::services::tui_o::store::{InitOrigin, InitSource, OStore, StoreConfig},
    crate::services::tui_o::writer::activation::{
        self, ActivationFacts,
        test_hook::{self, Step},
    },
    crate::test_support::wait::{join_within, recv_within},
    sha2::Digest,
    std::sync::mpsc,
};

/// An admitted first turn with its incarnation, source and binding; it opens no store.
struct Admitted {
    candidate: Candidate,
    token: CancelToken,
    admission: Option<FirstAdmission>,
    prepared: PreparedIncarnation,
    source: SourceId,
    channel: u64,
    // The global lock outlives all source operations; the env guard outlives that lock.
    dedupe_guard: Option<std::sync::MutexGuard<'static, ()>>,
    root: Option<crate::config::TestRuntimeRootGuard>,
}

impl Admitted {
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
        Self {
            candidate,
            token,
            admission: Some(admission),
            prepared,
            source,
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
}

impl Drop for Admitted {
    fn drop(&mut self) {
        drop(self.admission.take());
        self.candidate.invalidate_first(None, None);
        dedupe::reset_state_for_tests();
        drop(self.dedupe_guard.take());
        drop(self.root.take());
    }
}

/// An admitted first turn plus the enabled store its publication writes to.
#[cfg(unix)]
struct Fixture {
    admitted: Admitted,
    store: Arc<OStore>,
}

#[cfg(unix)]
impl std::ops::Deref for Fixture {
    type Target = Admitted;
    fn deref(&self) -> &Admitted {
        &self.admitted
    }
}

#[cfg(unix)]
impl Fixture {
    fn new(channel: u64) -> Self {
        let admitted = Admitted::new(channel);
        let runtime = crate::config::runtime_root().unwrap();
        let store = Arc::new(
            OStore::open_if_enabled(&StoreConfig { enabled: true }, &runtime)
                .unwrap()
                .unwrap(),
        );
        Self { admitted, store }
    }
    fn activate(&self) -> Result<(), String> {
        activate(&self.store, self.channel, &self.candidate, &self.proof())
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
#[cfg(unix)]
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
    let f = Admitted::new(8_165_101);
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn only_new_input_reopens_a_refused_episode_and_old_tokens_cannot_end_it() {
    use super::output_disposition::{Identity, Output, Phase, Reason};
    let fixture = Fixture::new(8_165_981);
    let old_proof = fixture.proof();
    let old = Output::new(
        Identity::from_proof(&old_proof, old_proof.mark.order),
        tokio::time::Instant::now(),
    );
    assert!(old.reject(Reason::SourceRejected));
    assert_eq!(old.phase(), Phase::RejectRequested(Reason::SourceRejected));
    assert_eq!(
        old.finish_first(
            &fixture.candidate,
            &old_proof,
            &fixture.store,
            Reason::Deadline
        ),
        Err(Reason::SourceRejected)
    );
    let tmux = old_proof.context.tmux_session.as_str();
    assert!(
        fixture
            .candidate
            .admit_first(fixture.channel, "new", "hi", tmux, || Ok(0))
            .is_none()
    );
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        tmux,
        Some(fixture.channel),
        None,
        false,
        old_proof.context.provider_root.clone(),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    let input = old_proof.mark.order + 1;
    assert!(
        fixture
            .candidate
            .admit_after_rejection(
                &old,
                (old_proof.mark.order, "new", "hi", &prepared.context),
                || Ok(0)
            )
            .is_none()
    );
    assert!(
        fixture
            .candidate
            .admit_after_rejection(
                &old,
                (input, &old_proof.nonce, "hi", &prepared.context),
                || Ok(0)
            )
            .is_none()
    );
    assert!(
        fixture
            .candidate
            .admit_after_rejection(&old, (input, "new", "hi", &old_proof.context), || Ok(0))
            .is_none()
    );
    let new = fixture
        .candidate
        .admit_after_rejection(&old, (input, "new", "hi", &prepared.context), || Ok(0))
        .unwrap();
    assert!(fixture.candidate.first_proof(&fixture.source).is_err());
    let path = fixture.source.path.with_file_name("new-execution.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"new-native\"}}\n",
    )
    .unwrap();
    let source = source_id_for("new-native", &path).unwrap();
    let proof = fixture.candidate.first_proof(&source).unwrap().unwrap();
    assert_eq!(proof.context, prepared.context);
    fixture
        .candidate
        .invalidate_first(Some(&old_proof.nonce), Some(tmux));
    assert_eq!(
        fixture.candidate.first_proof(&source).unwrap().unwrap(),
        proof
    );
    fixture
        .candidate
        .attach_first_execution(&old_proof.context, "hi", Some("new"));
    assert!(fixture.candidate.first_proof(&source).is_err());
    assert_eq!(
        old.phase(),
        Phase::RejectPendingNotice(Reason::SourceRejected)
    );
    drop(new);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn missing_prompt_and_wakes_exhaust_one_normalization_deadline() {
    use super::output_disposition::{Identity, Output, Phase, Reason};
    let fixture = Fixture::new(8_165_982);
    // A header that proves the session with no prompt yet waits instead of refusing the source.
    std::fs::write(
        &fixture.source.path,
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"cwd\":\"/fixture\"}}\n",
    )
    .unwrap();
    let proof = fixture.proof();
    let output = Output::new(Identity::from_proof(&proof, 1), tokio::time::Instant::now());
    let result = crate::services::tui_o::writer::normalization::run(
        &output,
        &fixture.candidate,
        &proof,
        &fixture.store,
    )
    .await;
    assert_eq!(result.unwrap_err(), Reason::Deadline);
    assert_eq!(output.phase(), Phase::RejectPendingNotice(Reason::Deadline));
    assert!(output.remaining().is_zero());
    assert!(output.adopt(&proof, || Ok(())).is_err());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_unproven_session_header_rejects_the_source_before_the_deadline() {
    use super::output_disposition::{Identity, Output, Phase, Reason};
    let fixture = Fixture::new(8_165_983);
    let proof = fixture.proof();
    let output = Output::new(Identity::from_proof(&proof, 1), tokio::time::Instant::now());
    let result = crate::services::tui_o::writer::normalization::run(
        &output,
        &fixture.candidate,
        &proof,
        &fixture.store,
    )
    .await;
    assert_eq!(result.unwrap_err(), Reason::SourceRejected);
    assert_eq!(
        output.phase(),
        Phase::RejectPendingNotice(Reason::SourceRejected)
    );
    assert!(!output.remaining().is_zero());
}

#[cfg(unix)]
#[path = "atomic_tests.rs"]
mod atomic_tests;

/// The admitted launch's trust watch: one Enter on a selected positive menu in the managed cwd,
/// spent from the admission's own deadline and repair, never adoption evidence.
#[cfg(unix)]
mod trust_tests {
    use super::*;
    use crate::services::tui_o::channel_policy::FirstHold;
    use crate::services::tui_o::channel_policy::output_disposition::BUDGET;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const TRUST: &str = "Do you trust this folder?\n› 1. Yes, proceed\n  2. No, exit";
    const NEGATIVE: &str = "Do you trust this folder?\n  1. Yes, proceed\n› 2. No, exit";

    /// A tmux on PATH that shows `pane` in `cwd` and logs every key it is sent.
    struct FakeTmux {
        dir: tempfile::TempDir,
        _path: crate::config::TestEnvVarGuard,
    }

    impl FakeTmux {
        fn new(cwd: &Path, pane: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let (program, log) = (dir.path().join("tmux"), dir.path().join("keys.log"));
            // The managed-session check reads this runtime's identity before a key is sent.
            let identity = crate::services::session_identity_metadata::runtime_identity("trust")
                .unwrap()
                .environment_assignment()
                .unwrap();
            let identity = crate::services::process::shell_escape(&identity);
            let script = format!(
                "#!/bin/sh\ncase \" $* \" in\n*show-environment*) printf '%s\\n' {identity};;\n*display-message*) printf '0\\n{}\\n';;\n*capture-pane*) printf '%s' '{pane}';;\n*send-keys*) echo \"$*\" >> '{}';;\nesac\n",
                cwd.display(),
                log.display()
            );
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path =
                crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
            Self { dir, _path: path }
        }

        fn enters(&self) -> usize {
            let log = std::fs::read_to_string(self.dir.path().join("keys.log"));
            log.map_or(0, |log| log.lines().count())
        }
    }

    fn managed_cwd() -> PathBuf {
        let cwd = crate::config::runtime_root()
            .unwrap()
            .join("workspaces/project");
        std::fs::create_dir_all(&cwd).unwrap();
        cwd
    }

    /// Runs the watch on paused time with no other timer, so the clock moves only by its own looks
    /// and the admission's deadline bounds it.
    async fn watch(fixture: &Fixture, cwd: &Path) {
        let context = fixture.prepared.context.clone();
        let watch = crate::services::tui_o::writer::normalization::watch_trust(
            fixture.candidate.clone(),
            context,
            cwd.to_path_buf(),
        );
        watch.await;
    }

    #[tokio::test(start_paused = true)]
    async fn first_trust_menu_gets_one_enter_and_the_proof_keeps_that_budget() {
        let fixture = Fixture::new(7_651_401);
        let admitted = tokio::time::Instant::now();
        let cwd = managed_cwd();
        let tmux = FakeTmux::new(&cwd, TRUST);
        watch(&fixture, &cwd).await;
        assert_eq!(
            tmux.enters(),
            1,
            "one Enter on the selected positive option"
        );
        assert_eq!(
            fixture.candidate.peek(),
            Adoption::Deferred,
            "the action adopts nothing"
        );
        let context = &fixture.prepared.context;
        assert!(
            fixture.candidate.first_trust_window(context).is_none(),
            "the repair is spent"
        );
        // The rollout proves the admission later: its output keeps the admission's budget.
        tokio::time::advance(Duration::from_secs(5)).await;
        fixture.candidate.normalize_first(&fixture.source);
        let nonce = first_nonce_id(fixture.token.turn_nonce().unwrap());
        let Some(FirstHold::Output(held)) = fixture.candidate.first_hold(nonce) else {
            panic!("the proved rollout starts the normalization");
        };
        let (output, proof) = *held;
        assert!(
            !output.take_repair(),
            "the binding repair is the spent trust repair"
        );
        assert_eq!(
            output.remaining(),
            BUDGET.saturating_sub(admitted.elapsed())
        );
        assert!(
            output.adopt(&proof, || Ok(())).is_ok(),
            "the proved output is adopted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_unmanaged_changed_or_late_trust_menu_gets_no_enter() {
        for (channel, case) in (7_651_402..).zip(["negative", "outside", "changed", "late"]) {
            let fixture = Fixture::new(channel);
            let cwd = managed_cwd();
            let elsewhere = tempfile::tempdir().unwrap();
            let tmux = match case {
                "negative" => FakeTmux::new(&cwd, NEGATIVE),
                "outside" => FakeTmux::new(elsewhere.path(), TRUST),
                _ => FakeTmux::new(&cwd, TRUST),
            };
            match case {
                "changed" => {
                    let tmux = &fixture.prepared.context.tmux_session;
                    let marker =
                        crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
                    std::fs::write(marker, "another-execution").unwrap();
                }
                "late" => tokio::time::advance(BUDGET).await,
                _ => {}
            }
            watch(&fixture, &cwd).await;
            assert_eq!(tmux.enters(), 0, "{case}: no Enter");
            assert_eq!(fixture.candidate.peek(), Adoption::Deferred, "{case}");
        }
    }
}
