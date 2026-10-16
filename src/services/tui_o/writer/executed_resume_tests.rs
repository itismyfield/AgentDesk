//! The production cold resume launch owns its executed source before O delivers from it.

use super::*;
use crate::services::claude_tui::hook_server::{
    adoption_retry::DurableKind, observation_ingress::IngressOutcome,
};
use crate::services::codex_tui::tail_launch::{BEFORE_RESUME_PROOF, SELECTION_FOR_TESTS};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    HookFirst,
    ProofFirst,
    SpawnFailure,
    MetadataFailure,
}

struct Cold {
    root: tempfile::TempDir,
    rollout: PathBuf,
    tmux: &'static str,
    channel: u64,
    _guards: Vec<TestEnvVarGuard>,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _endpoint: std::sync::MutexGuard<'static, ()>,
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _shared: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Cold {
    fn new(mode: Mode) -> Self {
        let shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
        // A canonical short parent keeps claim paths and tmux temp paths stable.
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let parent = if parent.as_os_str().len() <= 24 {
            parent
        } else {
            fs::canonicalize("/tmp").unwrap()
        };
        let mut guards = vec![TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "TMPDIR", &parent,
        )];
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let endpoint = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        guards.extend(env);
        dedupe::reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        let hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let home = root.path().join("codex-home");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join(format!("rollout-{ID}.jsonl"));
        let at = (Utc::now() - Duration::minutes(5)).to_rfc3339();
        let meta = json!({"type":"session_meta","timestamp":at,"payload":{"id":ID,"timestamp":at,
            "cwd":root.path(),"source":"cli","originator":"codex_cli_rs"}});
        fs::write(&rollout, format!("{meta}\n")).unwrap();
        response(&rollout, "old", "old history");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let program = |name: &str, body: String| {
            fs::write(bin.join(name), body).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        };
        program("codex", "#!/bin/sh\necho 'codex-cli 0.157.1'\n".to_owned());
        let state = root.path().join("tmux-state");
        fs::create_dir(&state).unwrap();
        if mode == Mode::SpawnFailure {
            fs::write(state.join("refuse"), "").unwrap();
        }
        // The pane never runs Codex: the fixture owns every rollout byte and hook.
        program(
            "tmux",
            format!(
                "#!/bin/bash\nd='{}'\n[ \"$1\" = -u ] && shift\nprintf '%s\\n' \"$*\" >> \"$d/calls\"\ncase \"$1\" in\n\
                 has-session) [ -f \"$d/created\" ] ;;\n\
                 new-session) [ -f \"$d/refuse\" ] && {{ echo refused >&2; exit 1; }}; : > \"$d/created\" ;;\n\
                 list-panes) printf '0\\n' ;;\n\
                 display-message) case \"${{@: -1}}\" in '#{{pane_dead}}') printf '0\\n' ;; '#{{pane_pid}}') ;; *) printf '80 24\\n' ;; esac ;;\n\
                 *) exit 0 ;;\nesac\n",
                state.display()
            ),
        );
        guards.push(TestEnvVarGuard::prepend_path_after_shared_test_env_lock(
            &bin,
        ));
        guards.push(TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CODEX_PATH",
            &bin.join("codex"),
        ));
        guards.push(TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CODEX_HOME",
            &home,
        ));
        guards.push(TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
            std::ffi::OsStr::new("0"),
        ));
        Self {
            root,
            rollout,
            tmux: crate::services::codex_tui::canary::CANARY_TMUX,
            channel: crate::services::codex_tui::canary::CANARY_CHANNEL,
            _guards: guards,
            _hosts: hosts,
            _endpoint: endpoint,
            _dedupe: dedupe_lock,
            _shared: shared,
        }
    }

    fn log_bytes(&self) -> Vec<u8> {
        fs::read(
            self.root
                .path()
                .join(binding_events::BINDING_EVENTS_DIR)
                .join(format!("{}.log", self.channel)),
        )
        .unwrap_or_default()
    }

    fn temp(&self, ext: &str) -> PathBuf {
        PathBuf::from(tc::session_temp_path(self.tmux, ext))
    }

    /// No artifact of this execution's ownership may exist after a refused launch.
    fn assert_no_explicit_proof(&self, log: &[u8], stage: &str) {
        assert!(!self.temp("spawn_nonce").exists(), "{stage}: spawn nonce");
        assert!(
            !self.temp(tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT).exists(),
            "{stage}: owned rollout marker"
        );
        assert!(
            dedupe::runtime_binding_for_tmux_session(self.tmux).is_none(),
            "{stage}: runtime binding"
        );
        assert_eq!(self.log_bytes(), log, "{stage}: durable log bytes");
        assert!(
            binding_events::binding_events_since(self.channel, 0)
                .unwrap_or_default()
                .iter()
                .all(|event| !matches!(
                    event.new,
                    binding_events::BindingTarget::Source(_)
                        | binding_events::BindingTarget::Resolved { .. }
                )),
            "{stage}: Source/Resolved rows"
        );
        assert!(
            !String::from_utf8_lossy(&self.log_bytes()).contains("explicit_resume"),
            "{stage}: ExplicitResume ownership"
        );
    }
}

impl Drop for Cold {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(self.channel);
        binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}

fn resume_hook(rollout: &Path, context: &BindingContext) -> IngressOutcome {
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let payload = json!({"session_id":ID,"transcript_path":rollout,"source":"resume"});
    observe_binding_hook(
        "codex",
        "session-start",
        Some(ID),
        Some(ID),
        &payload,
        &headers,
    )
}

/// O starts from the durable rows after the launch, as a restarted writer would.
async fn deliver(
    cold: &Cold,
    initial: &[u8],
) -> (
    OStore,
    Arc<owed::tests::Port>,
    watch::Sender<bool>,
    tokio::task::JoinHandle<Option<super::super::deliver::StopCause>>,
) {
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, cold.root.path())
        .unwrap()
        .unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for(ID, &cold.rollout).unwrap();
    store
        .begin_era(&[cold.channel], Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: source.clone(),
                    delivery_start: initial.len() as u64,
                    prefix_hash: hex::encode(Sha256::digest(initial)),
                }],
                initial_anchor: 100,
                build_digest: "executed-resume".into(),
                at: Utc::now(),
            })
        })
        .unwrap();
    let opened = store
        .open_channel(&store.read_era().unwrap().unwrap(), cold.channel)
        .unwrap()
        .unwrap();
    let (writer, port) = owed_writer(opened);
    let (stop, stopped) = watch::channel(false);
    let (_command_port, receiver) = actor::codex_submission::Port::new(1);
    let task = actor::codex_submission::spawning(receiver, || {
        actor::spawn_projecting(
            &WriterConfig { enabled: true },
            None,
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(cold.channel, ShadowProvider::Codex)),
            (
                stopped,
                watch::channel(false).0,
                watch::channel(None).0,
                actor::Owing::default(),
            ),
        )
    })
    .unwrap();
    polls().await;
    (store, port, stop, task)
}

async fn cold_resume_case(mode: Mode) {
    let cold = Cold::new(mode);
    let initial = fs::read(&cold.rollout).unwrap();
    if matches!(mode, Mode::SpawnFailure | Mode::MetadataFailure) {
        // A wrongly recorded proof would deliver this unit, so absence is observable.
        response(&cold.rollout, "resumed", "resumed body");
    }
    let log_before = cold.log_bytes();
    type Seen = Option<(BindingContext, IngressOutcome, binding_events::codex::Fold)>;
    let boundary: Arc<Mutex<Seen>> = Arc::default();
    let expired = Arc::new(AtomicBool::new(false));
    let worker = {
        let (boundary, expired) = (Arc::clone(&boundary), Arc::clone(&expired));
        let (root, rollout) = (cold.root.path().to_path_buf(), cold.rollout.clone());
        let (tmux, channel) = (cold.tmux, cold.channel);
        tokio::task::spawn_blocking(move || {
            use crate::services::codex_tui::session::source_observation::{
                CodexSourceMode, SOURCE_MODE_TEST,
            };
            SOURCE_MODE_TEST.with(|slot| slot.set(Some(CodexSourceMode::Verified)));
            binding_events::set_test_root(Some(&root));
            if mode == Mode::MetadataFailure {
                SELECTION_FOR_TESTS.with_borrow_mut(|slot| {
                    *slot = Some(Box::new(|selection| selection.rollout_path = None));
                });
            }
            let seen = Arc::clone(&boundary);
            BEFORE_RESUME_PROOF.with_borrow_mut(|slot| {
                *slot = Some(Box::new(move |context| {
                    let outcome = if mode == Mode::HookFirst {
                        resume_hook(&rollout, context)
                    } else {
                        IngressOutcome::Durable(DurableKind::Pending)
                    };
                    let fold = binding_events::codex::read_ownership(context).unwrap();
                    response(&rollout, "resumed", "resumed body");
                    *seen.lock().unwrap() = Some((context.clone(), outcome, fold));
                }));
            });
            let token = Arc::new(CancelToken::new());
            let (stopper, finished) = (Arc::clone(&token), Arc::new(AtomicBool::new(false)));
            let done = Arc::clone(&finished);
            let (tx, rx) =
                std::sync::mpsc::channel::<crate::services::agent_protocol::StreamMessage>();
            // Releases the held turn once the tail emitted the resumed unit; expiry fails the case.
            let canceller = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut frames = Vec::new();
                while !done.load(Ordering::Acquire) {
                    if let Ok(frame) = rx.recv_timeout(std::time::Duration::from_millis(5)) {
                        frames.push(format!("{frame:?}"));
                    }
                    if frames.iter().any(|frame| frame.contains("resumed body")) {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        expired.store(true, Ordering::Release);
                        break;
                    }
                }
                stopper.publish_cancel("fixture observed the resumed unit");
                frames
            });
            let result = crate::services::codex::actual_submit_tests::cold_driver(
                &root,
                ID,
                (tmux, channel),
                token,
                tx,
            );
            finished.store(true, Ordering::Release);
            let frames = canceller.join().unwrap();
            SOURCE_MODE_TEST.with(|slot| slot.set(None));
            SELECTION_FOR_TESTS.with_borrow_mut(|slot| slot.take());
            BEFORE_RESUME_PROOF.with_borrow_mut(|slot| slot.take());
            (result, frames)
        })
    };
    while !worker.is_finished() {
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (result, frames) = worker.await.unwrap();
    match mode {
        Mode::HookFirst | Mode::ProofFirst => {
            assert_eq!(result, Ok(()), "{mode:?}");
            assert!(
                !expired.load(Ordering::Acquire),
                "{mode:?}: tail deadline {frames:?}"
            );
            assert!(
                frames.iter().all(|frame| !frame.contains("old history")),
                "the tail starts at the committed offset: {frames:?}"
            );
            let (context, outcome, early) =
                boundary.lock().unwrap().take().expect("proof boundary");
            assert_eq!(context.launch_mode, "resume");
            assert_eq!(context.expected_native_session_id.as_deref(), Some(ID));
            let events = binding_events::binding_events_since(cold.channel, 0).unwrap();
            let fold = binding_events::codex::read_ownership(&context).unwrap();
            let proof = fold.verified.clone().expect("executed resume proof");
            let meta = fs::metadata(&cold.rollout).unwrap();
            assert_eq!(
                (proof.source.session_id.as_str(), &proof.source.path),
                (ID, &cold.rollout.canonicalize().unwrap())
            );
            assert_eq!(
                (proof.source.dev, proof.source.ino),
                (meta.dev(), meta.ino())
            );
            assert!(matches!(
                &proof.ownership.claim.evidence,
                binding_events::codex::ClaimEvidence::ExplicitResume { executed_session_id }
                    if executed_session_id == ID
            ));
            assert!(fold.pending.is_empty() && !fold.conflicted);
            let targets: Vec<_> = events.iter().map(|e| &e.new).collect();
            if mode == Mode::HookFirst {
                assert_eq!(outcome, IngressOutcome::Durable(DurableKind::Pending));
                assert!(early.verified.is_none() && early.pending.len() == 1);
                let pending_seq = early.pending[0].seq;
                assert!(
                    matches!(targets[..], [binding_events::BindingTarget::Pending { .. },
                        binding_events::BindingTarget::Resolved { pending_seq: seq, .. }] if *seq == pending_seq),
                    "{events:?}"
                );
                assert_eq!(proof.ownership.pending_seq, Some(pending_seq));
            } else {
                assert!(
                    matches!(targets[..], [binding_events::BindingTarget::Source(_)]),
                    "{events:?}"
                );
            }
            let binding = dedupe::runtime_binding_for_tmux_session(cold.tmux).expect("published");
            assert_eq!(binding.session_id.as_deref(), Some(ID));
            assert_eq!(Path::new(&binding.output_path), proof.source.path);
            let marker: serde_json::Value = serde_json::from_slice(
                &fs::read(cold.temp(tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT)).unwrap(),
            )
            .unwrap();
            assert_eq!(marker["rollout_path"].as_str(), proof.source.path.to_str());
            let (store, port, stop, task) = deliver(&cold, &initial).await;
            let mut expected = vec!["resumed body"];
            assert_eq!(port.posts(), expected, "{mode:?}: real POST");
            if mode == Mode::ProofFirst {
                let log = cold.log_bytes();
                assert_eq!(
                    resume_hook(&cold.rollout, &context),
                    IngressOutcome::Durable(DurableKind::Adopted)
                );
                assert_eq!(
                    cold.log_bytes(),
                    log,
                    "a later matching hook keeps the proof"
                );
                assert_eq!(
                    binding_events::codex::read_ownership(&context).unwrap(),
                    fold
                );
                response(&cold.rollout, "after", "after hook");
                polls().await;
                expected.push("after hook");
                assert_eq!(port.posts(), expected, "no source switch or replay");
            }
            let era = store.read_era().unwrap().unwrap();
            let channel = store.open_channel(&era, cold.channel).unwrap().unwrap();
            assert_eq!(posted(&channel), expected.len(), "durable Posted");
            stop.send(true).unwrap();
            assert!(task.await.unwrap().is_none());
        }
        Mode::SpawnFailure | Mode::MetadataFailure => {
            let error = result.expect_err("refused launch");
            let expected = if mode == Mode::SpawnFailure {
                "tmux error"
            } else {
                "without rollout path"
            };
            assert!(error.contains(expected), "{error}");
            assert!(boundary.lock().unwrap().is_none(), "no proof boundary");
            cold.assert_no_explicit_proof(&log_before, "returned");
            // A restart rebuilds only from durable and marker evidence.
            dedupe::reset_state_for_tests();
            binding_events::forget_channel_for_tests(cold.channel);
            dedupe::register_tmux_channel(cold.tmux, cold.channel);
            let shared = crate::services::discord::make_shared_data_for_tests();
            let _ =
                crate::services::discord::run_codex_rehydrate_pass_for_tests(&shared, cold.tmux);
            dedupe::codex_verified::resolve_registered_claims();
            cold.assert_no_explicit_proof(&log_before, "rehydrated");
            let (_store, port, _stop, task) = deliver(&cold, &initial).await;
            let stopped = task.await.unwrap();
            assert!(
                format!("{stopped:?}").contains("no binding baseline"),
                "O has no proven source to read: {stopped:?}"
            );
            assert!(port.posts().is_empty(), "{mode:?}: POST");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn actual_resume_spawn_resolves_early_native_pending_and_posts_once() {
    cold_resume_case(Mode::HookFirst).await;
}

#[tokio::test(start_paused = true)]
async fn actual_resume_spawn_proof_first_then_hook_is_idempotent() {
    cold_resume_case(Mode::ProofFirst).await;
}

#[tokio::test(start_paused = true)]
async fn actual_resume_spawn_failure_writes_no_explicit_proof() {
    cold_resume_case(Mode::SpawnFailure).await;
}

#[tokio::test(start_paused = true)]
async fn actual_resume_metadata_failure_never_recovers_explicit_proof() {
    cold_resume_case(Mode::MetadataFailure).await;
}
