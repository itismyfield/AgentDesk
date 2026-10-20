//! Actual cold/warm providers preserve restored source identities across an absent target.

use super::super::CanaryFixture;
use super::*;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use std::sync::Mutex;

pub(in crate::services::discord) mod joint_owner_tests;
mod policy_tests;

pub(in crate::services::discord) const Q1: &str = "B1 restored FIFO first instruction";
pub(in crate::services::discord) const Q2: &str = "B1 restored FIFO second instruction";
const Q3: &str = "B1 instruction after the second incarnation disappeared";
const THIRD_NATIVE_ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c745";

pub(in crate::services::discord) fn instruction(message: u64, text: &str) -> Intervention {
    Intervention {
        author_id: serenity::UserId::new(1),
        author_is_bot: false,
        message_id: serenity::MessageId::new(message),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![serenity::MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

pub(in crate::services::discord) async fn restore_instructions(
    fixture: &mut CanaryFixture,
    instructions: Vec<Intervention>,
) -> Vec<QueueIdentity> {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Codex, &fixture.shared.token_hash, None);
    fixture
        .shared
        .mailbox(channel)
        .replace_queue(instructions, persistence.clone())
        .await;
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        channel,
    )
    .unwrap();
    let identities: Vec<_> = disk.iter().map(QueueIdentity::of).collect();
    fixture.shared.mailboxes.remove_fixture_for_test(channel);
    let restored = fixture
        .shared
        .mailbox(channel)
        .hydrate_pending_queue_from_disk(persistence)
        .await;
    assert!(restored.persistence_error.is_none(), "{restored:?}");
    assert_eq!(restored.absorbed, identities.len());
    assert_eq!(queue_identities(fixture).await, identities);
    identities
}

pub(in crate::services::discord) async fn queue_identities(
    fixture: &CanaryFixture,
) -> Vec<QueueIdentity> {
    fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .intervention_queue
        .iter()
        .map(QueueIdentity::of)
        .collect()
}

async fn warm_diagnostic(fixture: &CanaryFixture, context: &BindingContext) {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let source = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    let source_allowed = dedupe::codex_verified_source_allowed(
        CANARY_TMUX,
        source.to_str().unwrap(),
        Some(NEW_NATIVE_ID),
    );
    let input_blocked = dedupe::codex_verified_input_blocked(CANARY_TMUX);
    let launch_blocked = dedupe::clear_relaunch::launch_blocked(CANARY_TMUX);
    let fold = p5::codex::read_ownership(context)
        .map(|fold| {
            (
                fold.verified
                    .map(|proof| (proof.seq, proof.source.session_id)),
                fold.pending.len(),
                fold.conflicted,
            )
        })
        .map_err(|error| error.to_string());
    let core = fixture
        .shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .map(|session| {
            (
                session.current_path.clone(),
                session.session_id.clone(),
                session.channel_name.clone(),
            )
        });
    let host_deferred = crate::services::discord::host_defer_gate::channel_session_deferred(
        &fixture.shared,
        &ProviderKind::Codex,
        CANARY_CHANNEL,
        CANARY_TMUX,
    )
    .await;
    let marker = fs::read(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ))
    .ok()
    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    eprintln!(
        "B1_WARM_SEED source_allowed={source_allowed} input_blocked={input_blocked} launch_blocked={launch_blocked} hold_detail={} host_deferred={host_deferred} core={core:?} fold={fold:?} marker={marker:?} native_state={:?} cwd_matches={}",
        dedupe::codex_verified_hold_detail(CANARY_TMUX),
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(&source),
        crate::services::codex_tui::rollout_tail::rollout_file_matches_cwd(
            &source,
            fixture.root.path()
        )
    );
}

pub(in crate::services::discord) async fn recorder(
    fixture: &mut CanaryFixture,
) -> crate::services::discord::admin_host_guard::tests::Recorder {
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                json!({"id": CANARY_CHANNEL.to_string(), "type":0,
                    "name":"adk-codex-tui-e2e", "guild_id":"42000", "position":0,
                    "permission_overwrites":[], "nsfw":false, "parent_id":null})
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    api
}

pub(in crate::services::discord) fn target_missing(fixture: &CanaryFixture, remove_markers: bool) {
    fixture.fake.mark_missing();
    fixture.alive.store(false, Ordering::SeqCst);
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    if remove_markers {
        crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
    }
}

pub(in crate::services::discord) async fn kickoff(
    fixture: &CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
) -> bool {
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: TEST_BOT,
    };
    crate::services::discord::queue_dispatch::kickoff::kickoff_idle_queue_channel(
        &deps,
        &ProviderKind::Codex,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await
    .started
}

pub(in crate::services::discord) use super::late_tests::wait_context_after;

pub(in crate::services::discord) fn publish_named_source(
    fixture: &CanaryFixture,
    context: &BindingContext,
    native: &str,
) -> PathBuf {
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{native}.jsonl"));
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let outcome = observe_binding_hook(
        "codex",
        "session-start",
        Some(native),
        Some(native),
        &json!({"session_id":native,"transcript_path":path,"source":"startup"}),
        &headers,
    );
    if context.source_policy.as_deref() == Some("verified") {
        assert_eq!(outcome, IngressOutcome::Durable(DurableKind::Adopted));
        assert!(dedupe::codex_verified_source_allowed(
            CANARY_TMUX,
            &path.display().to_string(),
            Some(native)
        ));
    }
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &fixture.old_source
    ));
    path
}

fn publish_named(fixture: &CanaryFixture, context: &BindingContext, native: &str) {
    let path = publish_named_source(fixture, context, native);
    terminal(&path, "new-clear-turn", "clear relaunch completed");
}

pub(in crate::services::discord) fn terminal(path: &std::path::Path, turn: &str, text: &str) {
    let mut output = fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in [
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":text}]}}),
        json!({"type":"event_msg","payload":{"type":"composer_ready","turn_id":turn}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":text}}),
    ] {
        writeln!(output, "{row}").unwrap();
    }
    output.flush().unwrap();
    assert_eq!(
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(path),
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "the terminal fixture must be idle to the existing hosted readiness classifier",
    );
}

pub(in crate::services::discord) async fn wait_completion(token: &Arc<CancelToken>) {
    let complete = tokio::time::timeout(Duration::from_secs(15), async {
        while !token.is_completion_cleanup() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !complete {
        token.cancelled.store(true, Ordering::SeqCst);
    }
    assert!(complete, "the real provider did not complete its terminal");
    assert_eq!(token.cancel_source(), None);
}

pub(in crate::services::discord) async fn wait_idle(fixture: &CanaryFixture) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while fixture
            .shared
            .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
            .snapshot()
            .await
            .cancel_token
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the completed provider must release its mailbox");
}

struct WarmProbe {
    spy: crate::services::codex_tui::host_input::spy::SpyGuard,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Drop for WarmProbe {
    fn drop(&mut self) {
        *self.calls.lock().unwrap() = self.spy.calls();
        crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY
            .with_borrow_mut(|slot| *slot = None);
    }
}

pub(crate) fn warm_draft(prompt: &str) -> String {
    use unicode_width::UnicodeWidthChar;
    let canonical = prompt.replace("\r\n", "\n").replace('\r', "\n");
    let mut rows = Vec::new();
    for line in canonical.split('\n') {
        let mut row = String::new();
        let mut width = 0;
        for character in line.chars() {
            let cells = character
                .width()
                .expect("the fixture prompt must be printable");
            if width + cells > 196 {
                rows.push(std::mem::take(&mut row));
                width = 0;
            }
            row.push(character);
            width += cells;
        }
        rows.push(row);
    }
    assert!(rows.len() <= 40, "fixture draft must fit its 200x80 pane");
    let mut pane = format!("╭{}╮\n", "─".repeat(198));
    let last = rows.len() - 1;
    for (index, row) in rows.into_iter().enumerate() {
        pane.push_str(&format!(
            "│ {row}{}│\n",
            if index == last { "▌" } else { "" }
        ));
    }
    pane.push_str(&format!(
        "╰{}╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send",
        "─".repeat(198)
    ));
    let parsed = crate::services::codex_tui::input::submission_draft_in_pane(&pane)
        .expect("every fixture prompt row must remain inside the composer frame");
    assert_eq!(
        parsed.split_whitespace().collect::<String>(),
        canonical.split_whitespace().collect::<String>(),
        "the fixture must prove the complete actual admitted prompt",
    );
    assert!(
        crate::services::codex_tui::input::prompt_submission_fits_pane(
            &canonical,
            canonical.contains('\n'),
            Some((200, 80)),
        )
    );
    pane
}

pub(in crate::services::discord) fn warm_probe(
    context: &BindingContext,
    accepted: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<Vec<String>>>,
) -> (
    crate::services::discord::codex_absence::test_setup::ResetGuard,
    mpsc::Receiver<String>,
    mpsc::Sender<()>,
    mpsc::Receiver<String>,
) {
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    let (entered, wait_entered) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let (boundary, boundary_events) = mpsc::channel();
    let setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |prompt| {
            use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};
            assert!(
                prompt.contains(Q2) && !prompt.contains(Q1),
                "actual queued prompt={prompt:?}"
            );
            let prompt = prompt.to_owned();
            entered.send(prompt.clone()).unwrap();
            let ready = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let draft = warm_draft(&prompt);
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY.with_borrow_mut(|slot| {
                *slot = Some(Box::new(move |stage| {
                    eprintln!("B1_Q2_WARM_BOUNDARY {stage}");
                    let _ = boundary.send(stage.to_owned());
                }));
            });
            let spy = SpyGuard::install(SpyState {
                pane_size: Some((200, 80)),
                captures: [ready.to_owned(), draft, ready.to_owned(), ready.to_owned()]
                    .into_iter()
                    .map(Some)
                    .collect(),
                on_enter: Some(Box::new(move || {
                    accepted.lock().unwrap().push(prompt.clone());
                    let mut output = fs::OpenOptions::new().append(true).open(&path).unwrap();
                    writeln!(output, "{}", json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"b1-q2"}})).unwrap();
                    output.flush().unwrap();
                    terminal(&path, "b1-q2", "clear relaunch completed");
                })),
                ..SpyState::default()
            });
            let _ = wait_release.recv_timeout(Duration::from_secs(30));
            Box::new(WarmProbe { spy, calls })
        }),
    );
    (setup, wait_entered, release, boundary_events)
}

async fn full_fifo(recover: Option<bool>) {
    full_fifo_with_owners(recover, None).await;
}

async fn full_fifo_with_owners(recover: Option<bool>, foreign_no_core: Option<bool>) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    if recover.is_some() {
        let cleared = clear_channel_session_state(
            &api.http,
            &fixture.shared,
            &ProviderKind::Codex,
            serenity::ChannelId::new(CANARY_CHANNEL),
            "!clear",
            SoftClearNotifyMode::Suppress,
        )
        .await
        .unwrap();
        assert_no_retirement_warning(&cleared);
        assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    } else {
        target_missing(&fixture, true);
    }
    if foreign_no_core == Some(true) {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
    }
    let _foreign_owners = match foreign_no_core {
        Some(no_core) => Some(joint_owner_tests::install_positive(&mut fixture, no_core).await),
        None => None,
    };
    let identities = restore_instructions(
        &mut fixture,
        vec![
            instruction(CANARY_CHANNEL + 101, Q1),
            instruction(CANARY_CHANNEL + 102, Q2),
        ],
    )
    .await;
    let old_state = foreign_no_core
        .is_some_and(|no_core| no_core)
        .then(|| joint_owner_tests::OldSourceSnapshot::seed(&fixture));
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    fixture.fake.take_calls();
    let started = kickoff(&fixture, &api).await;
    if foreign_no_core.is_some() && !started {
        assert_eq!(queue_identities(&fixture).await, identities);
        assert_eq!(old_io.stats(), Default::default());
        assert!(fixture.fake.take_calls().iter().all(|call| {
            ["new-session", "kill-session", "send-keys", "paste-buffer"]
                .iter()
                .all(|effect| !call.starts_with(effect))
        }));
        eprintln!(
            "BUNDLE_OWNER_DRAIN started=0 fifo_advanced=0 provider_effects=0 fifo_preserved=1"
        );
    }
    assert!(
        started,
        "exact foreign ownership must admit the restored FIFO head"
    );
    let first = fixture.turn_failure_guard().await;
    let n2 = wait_context_after(&fixture, &fixture.context.execution_nonce).await;
    let n2_path = n2
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    let new_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2_path);
    assert_eq!(n2.launch_mode, "fresh");
    assert_eq!(n2.source_policy.as_deref(), Some("verified"));
    assert!(n2.expected_native_session_id.is_none());
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(
        argv.iter().filter(|arg| arg.contains(Q1)).count(),
        1,
        "{argv:?}"
    );
    assert!(
        argv.iter()
            .all(|arg| !arg.contains(Q2) && arg != "resume" && arg != NATIVE_ID)
    );
    assert_eq!(queue_identities(&fixture).await, identities[1..]);
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let input_calls = Arc::new(Mutex::new(Vec::new()));
    let (_setup, warm_entered, warm_release, warm_boundary) =
        warm_probe(&n2, Arc::clone(&accepted), Arc::clone(&input_calls));
    let n2_rollout = publish_named_source(&fixture, &n2, NEW_NATIVE_ID);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
                &ProviderKind::Codex,
                CANARY_CHANNEL,
            )
            .unwrap();
            if row.is_some_and(|row| {
                row.turn_nonce.as_deref() == first.token.turn_nonce()
                    && row.session_id.as_deref() == Some(NEW_NATIVE_ID)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the real Q1 Init must reach its own inflight row before terminal");
    terminal(&n2_rollout, "new-clear-turn", "clear relaunch completed");
    wait_completion(&first.token).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while fixture
            .shared
            .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
            .snapshot()
            .await
            .cancel_token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &first.token))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the completed Q1 must release its exact mailbox token");
    tokio::time::timeout(Duration::from_secs(15), async {
        while fixture
            .shared
            .core
            .lock()
            .await
            .sessions
            .get(&serenity::ChannelId::new(CANARY_CHANNEL))
            .and_then(|session| session.session_id.as_deref())
            != Some(NEW_NATIVE_ID)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real Q1 completion must publish its native selector before warm Q2");
    first.finish();
    if foreign_no_core.is_some() {
        joint_owner_tests::assert_bridge_once(&api, "Q1 cold").await;
    }
    let before_attach = activity::activity_now(
        &fixture.shared,
        &ProviderKind::Codex,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await;
    eprintln!("B1_N2_SEED_BEFORE_WATCHER activity={before_attach:?}");
    let n2_rollout = n2
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    // The REST fixture has no gateway watcher; attach its handle to the real hook-created log.
    activity::tests::companion_attach_log_watcher(
        &fixture.shared,
        CANARY_CHANNEL,
        CANARY_TMUX,
        &n2_rollout,
    );
    let settled =
        activity::tests::companion_settled_activity(&fixture.shared, CANARY_CHANNEL).await;
    assert_eq!(
        settled.activity,
        Activity::Idle,
        "actual N2 terminal must be idle before warm Q2: {settled:?}"
    );
    warm_diagnostic(&fixture, &n2).await;
    let warm_started = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(prompt) = warm_entered.try_recv() {
                assert!(prompt.contains(Q2));
                break;
            }
            let snapshot = fixture
                .shared
                .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                .snapshot()
                .await;
            if snapshot.cancel_token.is_none() && !snapshot.intervention_queue.is_empty() {
                kickoff(&fixture, &api).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !warm_started {
        let channel = serenity::ChannelId::new(CANARY_CHANNEL);
        let snapshot = fixture.shared.mailbox(channel).snapshot().await;
        let observed = activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
        let parked = fixture.shared.queue_park_ledger.project(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
            &snapshot,
        );
        let readiness = crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await;
        let binding = dedupe::runtime_binding_for_tmux_session(CANARY_TMUX);
        let mut effects = std::collections::BTreeMap::new();
        for call in fixture.fake.take_calls() {
            *effects
                .entry(call.split_whitespace().next().unwrap_or("empty").to_owned())
                .or_insert(0_usize) += 1;
        }
        eprintln!(
            "B1_Q2_NO_PROVIDER activity={observed:?} park={parked:?} readiness_blocked={readiness} binding={binding:?} queue={:?} token={} pending={:?} recovery={:?} effect_counts={effects:?}",
            snapshot
                .intervention_queue
                .iter()
                .map(QueueIdentity::of)
                .collect::<Vec<_>>(),
            snapshot.cancel_token.is_some(),
            snapshot.pending_user_dispatch,
            snapshot.recovery_started_at,
        );
        if let Some(token) = snapshot.cancel_token {
            token.cancelled.store(true, Ordering::SeqCst);
        }
    }
    assert!(
        warm_started,
        "Q2 must reach the actual provider on its worker"
    );
    let second = fixture.turn_failure_guard().await;
    warm_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(stage) = warm_boundary.try_recv() {
                panic!("Q2 actual verified provider held before accepted Enter: {stage}");
            }
            if accepted.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Q2 must receive exactly one actual guarded Enter before its terminal");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(stage) = warm_boundary.try_recv() {
                panic!("Q2 actual verified provider held after accepted Enter: {stage}");
            }
            if second.token.is_completion_cleanup() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Q2 accepted Enter but its actual provider did not complete the terminal");
    wait_completion(&second.token).await;
    wait_idle(&fixture).await;
    second.finish();
    if foreign_no_core.is_some() {
        joint_owner_tests::assert_bridge_once(&api, "Q2 warm").await;
    }
    assert!(queue_identities(&fixture).await.is_empty());
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert_eq!(accepted.lock().unwrap().len(), 1);
    assert_eq!(
        input_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1
    );
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(n2.execution_nonce.clone())
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .unwrap();
    assert!(disk.is_empty());
    if let Some(old_state) = old_state {
        old_state.assert_preserved();
    }
    assert_eq!(
        old_io.stats(),
        Default::default(),
        "an absent launch must never read or advance the old consumer source"
    );
    assert!(
        new_io.stats().read_bytes > 0,
        "the oracle must observe the actual new source consumer"
    );
    drop(new_io);
    if let Some(boot) = recover {
        recover_actual_n2(&mut fixture, &api, &n2, boot).await;
    }
}

async fn recovery_diagnostic(
    fixture: &CanaryFixture,
    n2: &BindingContext,
    boot: bool,
    stage: &str,
    readiness: Option<bool>,
) {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let snapshot = fixture.shared.mailbox(channel).snapshot().await;
    let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
        &ProviderKind::Codex,
        CANARY_CHANNEL,
    )
    .map(|row| {
        row.map(|row| {
            json!({
                "provider":row.provider, "channel":row.channel_id, "tmux":row.tmux_session_name,
                "session":row.session_id, "nonce":row.turn_nonce,
                "message":row.user_msg_id, "owner":row.request_owner_user_id,
                "runtime":format!("{:?}", row.runtime_kind),
            })
        })
    });
    let durable_pending =
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Codex,
            &fixture.shared.token_hash,
            channel,
        )
        .map(|marker| {
            marker.map(|(item, override_channel)| (QueueIdentity::of(&item), override_channel))
        });
    let disk = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        channel,
    )
    .map(|(items, _)| items.iter().map(QueueIdentity::of).collect::<Vec<_>>());
    let turn_state = snapshot.cancel_token.as_ref().map(|cancel| {
        (
            cancel.cancelled.load(Ordering::SeqCst),
            cancel.is_completion_cleanup(),
            cancel.cancel_source(),
        )
    });
    let activity = activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
    let unbound =
        activity::tests::unbound_verdict_for_tests(&fixture.shared, &ProviderKind::Codex, channel)
            .await;
    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX);
    let observed_capture = observation
        .as_ref()
        .and_then(|observation| dedupe::clear_relaunch::absent_capture(CANARY_TMUX, observation))
        .map(|captured| (captured.seq, captured.nonce));
    let capture = dedupe::clear_relaunch::capture(CANARY_TMUX)
        .map(|captured| captured.map(|captured| (captured.seq, captured.nonce)));
    let context = dedupe::binding_context::execution_context("codex", &n2.execution_nonce)
        .map(|context| json!({
            "provider":context.provider, "channel":context.channel_id, "tmux":context.tmux_session,
            "nonce":context.execution_nonce, "host":context.host, "runtime":context.owner_runtime_root,
            "policy":context.source_policy, "expected_native":context.expected_native_session_id,
            "launch_mode":context.launch_mode,
        }));
    let binding = dedupe::runtime_binding_for_tmux_session(CANARY_TMUX).map(|binding| {
        (
            binding.runtime_kind,
            binding.session_id,
            binding.output_path,
            binding.last_offset,
        )
    });
    let core = fixture
        .shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .map(|session| {
            (
                session.current_path.clone(),
                session.session_id.clone(),
                session.channel_name.clone(),
            )
        });
    let marker =
        fs::read(crate::services::tmux_common::session_temp_path(
            CANARY_TMUX,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .map(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes).map(|marker| json!({
        "session":marker["session_id"], "path":marker["rollout_path"],
        "nonce":marker["codex_ownership"]["execution_nonce"],
        "proof_seq":marker["codex_ownership"]["proof_seq"],
        "dev":marker["codex_ownership"]["dev"], "ino":marker["codex_ownership"]["ino"],
    })).map_err(|error| error.to_string())
        })
        .map_err(|error| error.to_string());
    let fold = p5::codex::read_ownership(n2)
        .map(|fold| {
            (
                fold.verified.map(|proof| {
                    (
                        proof.seq,
                        proof.source.session_id,
                        proof.source.dev,
                        proof.source.ino,
                    )
                }),
                fold.pending.len(),
                fold.conflicted,
            )
        })
        .map_err(|error| error.to_string());
    let host_deferred = crate::services::discord::host_defer_gate::channel_session_deferred(
        &fixture.shared,
        &ProviderKind::Codex,
        CANARY_CHANNEL,
        CANARY_TMUX,
    )
    .await;
    let park = fixture.shared.queue_park_ledger.project(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        &snapshot,
    );
    let terminal_custody =
        crate::services::discord::terminal_delivery_custody::retains_channel(CANARY_CHANNEL);
    eprintln!(
        "B1_Q3_CUSTODY boot={boot} stage={stage} terminal_custody={terminal_custody:?} turn_state={turn_state:?} active=({:?},{:?},{:?}) pending={:?} pending_sources={:?} lease_caller={} pending_since={:?} recovery={:?} queue={:?} durable_pending={durable_pending:?} disk={disk:?} row={row:?}",
        snapshot.active_turn_nonce,
        snapshot.active_request_owner,
        snapshot.active_user_message_id,
        snapshot.pending_user_dispatch,
        snapshot.pending_user_dispatch_source_ids,
        snapshot.pending_user_dispatch_lease_held_by_caller,
        snapshot.pending_user_dispatch_since,
        snapshot.recovery_started_at,
        snapshot
            .intervention_queue
            .iter()
            .map(QueueIdentity::of)
            .collect::<Vec<_>>()
    );
    eprintln!(
        "B1_Q3_GATES boot={boot} stage={stage} readiness={readiness:?} activity={activity:?} unbound={unbound:?} observation={} observed_capture={observed_capture:?} capture={capture:?} nonce={:?} marker={marker:?} fold={fold:?} core={core:?} binding={binding:?} host_deferred={host_deferred} hold_detail={} park={park:?} presence={:?} pane={:?} process_pid={:?} context={context:?}",
        observation.is_some(),
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        dedupe::codex_verified_hold_detail(CANARY_TMUX),
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::pane_liveness(CANARY_TMUX),
        crate::services::session_backend::process_session_pid(CANARY_TMUX)
    );
}

pub(in crate::services::discord) async fn wait_recovery_custody_settled(
    fixture: &CanaryFixture,
    n2: &BindingContext,
    boot: bool,
) {
    recovery_diagnostic(fixture, n2, boot, "before_settle", None).await;
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = fixture.shared.mailbox(channel).snapshot().await;
            let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
                &ProviderKind::Codex,
                CANARY_CHANNEL,
            );
            let pending =
                crate::services::turn_orchestrator::load_channel_pending_dispatch_marker_checked(
                    &ProviderKind::Codex,
                    &fixture.shared.token_hash,
                    channel,
                );
            let terminal_custody =
                crate::services::discord::terminal_delivery_custody::retains_channel(
                    CANARY_CHANNEL,
                );
            if snapshot.cancel_token.is_none()
                && snapshot.active_turn_nonce.is_none()
                && snapshot.pending_user_dispatch.is_none()
                && !snapshot.pending_user_dispatch_lease_held_by_caller
                && snapshot.recovery_started_at.is_none()
                && matches!(row, Ok(None))
                && matches!(pending, Ok(None))
                && matches!(terminal_custody, Ok(false))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    recovery_diagnostic(
        fixture,
        n2,
        boot,
        if settled {
            "after_settle"
        } else {
            "settle_timeout"
        },
        None,
    )
    .await;
    assert!(
        settled,
        "warm Q2 custody must settle naturally before restoring Q3; no custody was removed"
    );
}

async fn recover_actual_n2(
    fixture: &mut CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    n2: &BindingContext,
    boot: bool,
) {
    wait_recovery_custody_settled(fixture, n2, boot).await;
    fixture
        .shared
        .tmux_watchers
        .remove(&serenity::ChannelId::new(CANARY_CHANNEL));
    let source = p5::codex::read_ownership(n2)
        .unwrap()
        .verified
        .unwrap()
        .source;
    let path = n2
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    let body = fs::read(&path).unwrap();
    let history = fs::read(fixture.history()).unwrap();
    let archive = PathBuf::from(&n2.owner_runtime_root)
        .join("runtime/binding_evidence/codex")
        .join(format!("{}.json", n2.execution_nonce));
    let evidence = fs::read(&archive).unwrap();
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&path);
    target_missing(fixture, false);
    if boot {
        assert!(dedupe::evict_dead_tmux_mirror(CANARY_TMUX));
    }
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(n2.execution_nonce.clone())
    );
    let cli = fixture.root.path().join("codex-fixture");
    let script = fs::read_to_string(&cli).unwrap();
    assert!(script.contains(NEW_NATIVE_ID));
    fs::write(&cli, script.replace(NEW_NATIVE_ID, THIRD_NATIVE_ID)).unwrap();
    let identities =
        restore_instructions(fixture, vec![instruction(CANARY_CHANNEL + 103, Q3)]).await;
    fixture.fake.take_calls();
    crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    let readiness = crate::services::discord::router::hosted_tui_promote_readiness_blocked(
        &fixture.shared,
        &ProviderKind::Codex,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await;
    recovery_diagnostic(
        fixture,
        n2,
        boot,
        "q3_before_readiness_assert",
        Some(readiness),
    )
    .await;
    assert!(
        !readiness,
        "boot={boot}: a restored Q3 must pass the real readiness boundary without another clear"
    );
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    let started = kickoff(fixture, api).await;
    if !started {
        recovery_diagnostic(fixture, n2, boot, "q3_kickoff_no_start", Some(readiness)).await;
    }
    assert!(started, "boot={boot}; source={:?}", identities[0]);
    let third = fixture.turn_failure_guard().await;
    let held = async {
        loop {
            if let Ok(tmux) = hold_rx.try_recv() {
                break tmux;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let n3 = tokio::select! {
        context = wait_context_after(fixture, &n2.execution_nonce) => Some(context),
        tmux = held => {
            assert_eq!(tmux, CANARY_TMUX);
            third.token.cancelled.store(true, Ordering::SeqCst);
            None
        }
    };
    if n3.is_none() {
        wait_idle(fixture).await;
        let calls = fixture.fake.take_calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("new-session "))
                .count(),
            1,
            "boot={boot}: actual Q3 provider preserved the old incarnation instead of launching; calls={calls:?}"
        );
    }
    let n3 = n3.expect("Q3 must create its own fresh incarnation");
    let n3_path = n3
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{THIRD_NATIVE_ID}.jsonl"));
    let new_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n3_path);
    assert_eq!(n3.launch_mode, "fresh");
    assert_eq!(n3.source_policy.as_deref(), Some("verified"));
    assert!(n3.expected_native_session_id.is_none());
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    publish_named(fixture, &n3, THIRD_NATIVE_ID);
    wait_completion(&third.token).await;
    wait_idle(fixture).await;
    third.finish();
    assert_eq!(
        old_io.stats(),
        Default::default(),
        "Q3 must never read or advance the retained N2 consumer source"
    );
    assert!(
        new_io.stats().read_bytes > 0,
        "the real N3 tail must positively exercise the read oracle"
    );
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(argv.iter().filter(|arg| arg.contains(Q3)).count(), 1);
    assert!(
        argv.iter()
            .all(|arg| arg != "resume" && arg != NEW_NATIVE_ID)
    );
    let calls = fixture.fake.take_calls();
    assert_eq!(
        crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX),
        ["session_temp", "codex_session_files"],
        "only the existing dead-target temporary cleanup is authorized",
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert_eq!(fs::read(path).unwrap(), body);
    assert_eq!(fs::read(archive).unwrap(), evidence);
    assert!(fs::read(fixture.history()).unwrap().starts_with(&history));
    assert_eq!(
        dedupe::binding_context::execution_context("codex", &n2.execution_nonce).unwrap(),
        *n2
    );
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    assert!(queue_identities(fixture).await.is_empty());
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
}

#[tokio::test(flavor = "current_thread")]
async fn restored_q1_q2_use_actual_cold_then_verified_warm_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::restored_q1_q2_use_actual_cold_then_verified_warm_fifo_pg"
    )) {
        full_fifo(None).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_n2_and_warm_q2_survive_transport_loss_in_memory_pg() {
    if isolated(concat!(
        module_path!(),
        "::actual_n2_and_warm_q2_survive_transport_loss_in_memory_pg"
    )) {
        full_fifo(Some(false)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_n2_and_warm_q2_survive_transport_loss_after_boot_pg() {
    if isolated(concat!(
        module_path!(),
        "::actual_n2_and_warm_q2_survive_transport_loss_after_boot_pg"
    )) {
        full_fifo(Some(true)).await;
    }
}
