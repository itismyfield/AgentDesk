//! Herdr clears from the clear entry carried through to what reads them next: the next turn on the
//! same log, a dcserver crash and boot around a clear's Pending, and a real O actor's projection.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Instant as Clock;

use tokio::sync::broadcast;

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::agent_protocol::StreamMessage;
use crate::services::claude::herdr_turn::{self, AttachRequest, HerdrTurn, HerdrTurnPorts};
use crate::services::claude_tui::hook_server::HookEvent;
use crate::services::discord::recovery_engine::herdr_reader::SocketHerdrReader;
use crate::services::discord::recovery_engine::host_reconcile::reconcile_hosted_session_pg;
use crate::services::herdr_launch::{HerdrLaunchEndpoint, HerdrLaunchHost};
use crate::services::session_host::HerdrTarget;
use crate::services::tui_prompt_dedupe::binding_events::{BindingTarget, binding_events_since};

const LIMIT: Duration = Duration::from_secs(60);

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The provider's config home and the turn's working directory, so every transcript path is the
/// one Claude computes for its session; the previous config home comes back on drop.
struct Home {
    cwd: tempfile::TempDir,
    _config: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Drop for Home {
    fn drop(&mut self) {
        // SAFETY: the fixture's runtime root guard holds the shared env lock until after this.
        match self.previous.take() {
            Some(previous) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", previous) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
    }
}

impl Home {
    /// Built after the fixture, whose runtime root guard holds the shared env lock.
    fn new() -> Self {
        let config = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        // SAFETY: the fixture's runtime root guard holds the shared env lock for the whole test.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config.path()) };
        Self {
            cwd: tempfile::tempdir().unwrap(),
            _config: config,
            previous,
        }
    }

    fn transcript(&self, session: &str) -> PathBuf {
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
        path(self.cwd.path(), session, None).unwrap()
    }
}

/// The pane's SessionStart(clear) of `session` as its hook logs it, naming the computed transcript.
fn clear_hook(fx: &Fixture, home: &Home, session: &str, verified: bool) {
    let transcript = home.transcript(session);
    let text = transcript.display().to_string();
    let payload = json!({"source": "clear", "session_id": session, "transcript_path": text});
    let hook = HookSignal::from_payload("session_start", &payload);
    let proposal = Proposal {
        channel_id: fx.channel_id.get(),
        provider: "claude",
        tmux_session: &fx.logical,
        session_id: Some(session),
        path: &text,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Clear),
        hook: Some(&hook),
    };
    with_tmux_source_authority(&fx.logical, |_| {
        if !verified {
            binding_events::record_pending(&proposal).unwrap();
            return;
        }
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, b"").unwrap();
        let meta = std::fs::metadata(&transcript).unwrap();
        let (dev, ino) = (meta.dev(), meta.ino());
        let source = SourceId {
            session_id: session.into(),
            path: transcript.clone(),
            dev,
            ino,
        };
        binding_events::record_verified(&proposal, &source).unwrap();
    });
}

/// The pane's latest record resolves the Pending of `session` to that session.
fn resolved_to(fx: &Fixture, session: &str) -> bool {
    let events = binding_events_since(fx.channel_id.get(), 0).unwrap();
    let pane: Vec<_> = events
        .iter()
        .filter(|e| e.tmux_session == fx.logical)
        .collect();
    let pending = pane.iter().rev().find_map(|e| match &e.new {
        BindingTarget::Pending {
            payload_session_id, ..
        } if payload_session_id == session => Some(e.seq),
        _ => None,
    });
    matches!(pane.last().map(|e| &e.new), Some(BindingTarget::Resolved { pending_seq, source })
        if Some(*pending_seq) == pending && source.session_id == session)
}

/// The clear, with `hook` playing the pane's SessionStart(clear) once this clear's own `/clear` line
/// follows the lines already sent.
async fn clear_then(fx: &Fixture, hook: impl FnOnce()) -> anyhow::Result<()> {
    let sent = fx.rig.sends().len();
    let mut clear = Box::pin(fx.clear());
    let mut hook = Some(hook);
    loop {
        tokio::select! {
            result = &mut clear => return result,
            () = tokio::time::sleep(Duration::from_millis(10)) => {
                if fx.rig.sends().len() > sent && let Some(hook) = hook.take() {
                    hook();
                }
            }
        }
    }
}

/// A clear's worker leaves its blocking thread without a test binding root when it ends; every idle
/// blocking thread takes the fixture's root again, as a new one does when it starts.
async fn reroot_blocking_threads(fx: &Fixture) {
    const THREADS: usize = 16;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let rerooted = (0..THREADS).map(|_| {
        let (barrier, log) = (barrier.clone(), fx.log.path().to_path_buf());
        tokio::task::spawn_blocking(move || {
            binding_events::set_test_root(Some(&log));
            barrier.wait();
        })
    });
    for task in rerooted.collect::<Vec<_>>() {
        task.await.unwrap();
    }
}

/// A Bound execution's turn: it never launches or attaches, and its pane is confirmed by the
/// production restart reader on the pane's registered endpoint.
struct BoundPorts<'a> {
    pool: &'a sqlx::PgPool,
}

impl HerdrTurnPorts for BoundPorts<'_> {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        None
    }

    fn hook_events(&self) -> broadcast::Receiver<HookEvent> {
        broadcast::channel(1).1
    }

    fn attach(&self, _: &AttachRequest<'_>) -> Result<bool, String> {
        Err("a Bound execution attaches nothing".into())
    }

    fn confirm_bound(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        _: &HerdrTarget,
    ) -> Result<(), String> {
        let reader = SocketHerdrReader::of(record).ok_or("no registered endpoint")?;
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &owner.discord_token_hash,
            channel_id: &owner.channel_id,
        };
        let provider = &owner.provider;
        let key = HostedLookupKey::Canonical { provider, identity };
        let reconcile = reconcile_hosted_session_pg(self.pool, key, &reader);
        let verdict = tokio::runtime::Handle::current().block_on(reconcile);
        match verdict.admits_reconnect() {
            true => Ok(()),
            false => Err(format!("not confirmed: {verdict:?}")),
        }
    }
}

/// The next turn on its own thread, as provider dispatch runs it; this thread plays the provider,
/// answering in `session`'s transcript once paste and Enter follow the `sent` writes.
fn next_turn(
    fx: &Fixture,
    home: &Home,
    session: &str,
    sent: usize,
) -> (Result<(), String>, Vec<StreamMessage>) {
    let channel = fx.channel_id.get().to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &fx.shared.token_hash,
        channel_id: &channel,
    };
    let key = HostedLookupKey::Canonical {
        provider: "claude",
        identity,
    };
    let row = match fx.rt.block_on(load_hosted_execution_pg(&fx.pool, key)) {
        HostedLookup::Found(found) => found.record,
        other => panic!("the turn host reads no row: {other:?}"),
    };
    let HostedRecord::Known(record) = &row else {
        panic!("no known row: {row:?}");
    };
    let endpoint = HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: KEY.into(),
        socket_addr: fx.rig.socket().display().to_string(),
        herdr_session: SESSION.into(),
    };
    let (owner, channel_id) = (record.owner.clone(), fx.channel_id.get());
    fx.rig.show_panes(&[PANE]);
    let (rt, pool, rig, log) = (&fx.rt, &fx.pool, &fx.rig, fx.log.path());
    let cwd = home.cwd.path().to_str().unwrap();
    let cancel = Arc::new(CancelToken::new());
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (token, finished, row) = (cancel.clone(), &finished, &row);
        let executor = scope.spawn(move || {
            let _runtime = rt.enter();
            binding_events::set_test_root(Some(log));
            let _registry = rig.registry_on_this_thread();
            let _admission = open_admission();
            let (sender, receiver) = std::sync::mpsc::channel();
            let turn = HerdrTurn {
                pool,
                owner,
                channel_id,
                endpoint,
                row: Some(row),
                prompt: "질문",
                working_dir: cwd,
                system_prompt: None,
                model: None,
                hook_endpoint: None,
                cancel: Some(token),
            };
            let result = herdr_turn::execute(turn, &BoundPorts { pool }, sender);
            finished.store(true, Ordering::SeqCst);
            (result, receiver.try_iter().collect::<Vec<_>>())
        });
        let deadline = Clock::now() + LIMIT;
        let wait = |done: &dyn Fn() -> bool| {
            while !done() && !finished.load(Ordering::SeqCst) && Clock::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&|| fx.rig.sends().len() >= sent + 2);
        if fx.rig.sends().len() == sent + 2 {
            let user = json!({"type": "user", "sessionId": session,
                "message": {"role": "user", "content": "질문"}});
            let answer = json!({"type": "assistant", "sessionId": session, "message": {
                "role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "답"}]}});
            let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": session});
            let lines: String = [user, answer, done].map(|l| format!("{l}\n")).concat();
            let transcript = home.transcript(session);
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            let mut file = std::fs::OpenOptions::new();
            let mut file = file.create(true).append(true).open(&transcript).unwrap();
            std::io::Write::write_all(&mut file, lines.as_bytes()).unwrap();
        }
        wait(&|| false);
        cancel.cancelled.store(true, Ordering::SeqCst);
        executor.join().unwrap()
    })
}

fn prompt_lines() -> Vec<Value> {
    vec![
        json!({"pane_id": PANE, "text": "\u{1b}[200~질문\u{1b}[201~"}),
        json!({"pane_id": PANE, "keys": ["enter"]}),
    ]
}

/// The turn prompted `session` once, read its transcript to the answer and resolved its Pending.
fn assert_turn_took(fx: &Fixture, home: &Home, session: &str, turn: &TurnResult) {
    let (result, messages) = turn;
    let init = messages.iter().find_map(|m| match m {
        StreamMessage::Init { session_id, .. } => Some(session_id.as_str()),
        _ => None,
    });
    assert_eq!(
        init,
        Some(session),
        "the prompt went to {session}: {result:?}"
    );
    let text = messages
        .iter()
        .any(|m| matches!(m, StreamMessage::Text { content } if content == "답"));
    assert!(
        text,
        "the answer was read from its transcript: {messages:?}"
    );
    let ready = messages.iter().find_map(|m| match m {
        StreamMessage::RuntimeReady { handoff } => Some(format!("{handoff:?}")),
        _ => None,
    });
    let transcript = home.transcript(session).display().to_string();
    assert!(
        ready
            .as_deref()
            .is_some_and(|ready| ready.contains(&transcript)),
        "handed off on {transcript}: {ready:?}"
    );
    assert!(resolved_to(fx, session), "its Pending resolved: {result:?}");
    assert_eq!(result, &Ok(()));
}

type TurnResult = (Result<(), String>, Vec<StreamMessage>);

/// Nothing ended, reset or rewrote the execution.
async fn assert_execution_kept(fx: &Fixture) {
    let resets = fx
        .fake
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("reset:"));
    assert_eq!(resets.count(), 0, "no reset");
    assert!(fx.alive.load(Ordering::SeqCst), "no managed reset");
    assert!(fx.marker_names_execution(), "the execution is kept");
    assert!(fx.row_bound().await, "the row stays Bound");
}

// Source X, then `!clear` twice from the entry with each Pending logged by its own hook under real
// session ids and computed transcripts; that same log takes the next turn to Z once, alone.
#[test]
fn two_clears_from_the_entry_take_the_next_turn_to_the_latest_cleared_session_pg() {
    let fx = Fixture::new(11);
    let home = Home::new();
    let [x, y, z] = [(); 3].map(|()| uuid());
    clear_hook(&fx, &home, &x, true);
    fx.rt.block_on(async {
        let first = clear_then(&fx, || clear_hook(&fx, &home, &y, false)).await;
        first.expect("the first clear commits on Y");
        reroot_blocking_threads(&fx).await;
        let second = clear_then(&fx, || clear_hook(&fx, &home, &z, false)).await;
        second.expect("the second clear commits on Z");
        assert_eq!(fx.session().await, (Some(z.clone()), true));
    });

    let turn = next_turn(&fx, &home, &z, 2);
    let lines = [clear_line(), clear_line(), prompt_lines()].concat();
    assert_eq!(
        fx.rig.sends(),
        lines,
        "one line per clear, one paste and Enter"
    );
    assert_turn_took(&fx, &home, &z, &turn);
    let key = &fx.session_key;
    let (clear, save) = (format!("clear:{key}"), |s: &str| format!("save:{s}"));
    assert_eq!(fx.fake.calls(), [clear.clone(), save(&y), clear, save(&z)]);
    fx.rt.block_on(assert_execution_kept(&fx));
}

/// The clear effects of a dcserver about to crash: its save of the cleared session never returns.
#[derive(Default)]
struct Crashing {
    calls: Mutex<Vec<String>>,
}

impl NativeClearEffects for Crashing {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a> {
        let call = format!("clear:{session_key}");
        self.calls.lock().unwrap().push(call);
        Box::pin(async { true })
    }
    fn save_selector<'a>(&'a self, _: &'a str, session: &'a str, _: ChannelId) -> Effect<'a> {
        self.calls.lock().unwrap().push(format!("save:{session}"));
        Box::pin(std::future::pending())
    }
    fn submit(&self, _: &ClearTicket, _: Instant) -> NativeClearSubmission {
        NativeClearSubmission::NotSent
    }
    fn composer_empty(&self, _: &str, _: Instant) -> bool {
        true
    }
    fn reset_process(&self, tmux: &str) {
        self.calls.lock().unwrap().push(format!("reset:{tmux}"));
    }
}

/// `!clear` in a dcserver of its own until `/clear` is sent (and `session`'s Pending logged when
/// `logged`); then it dies with its runtime, tasks, guard, pool and memory.
fn clear_then_crash(fx: &Fixture, home: &Home, session: &str, logged: bool) -> Vec<String> {
    let rt = {
        let (rig, log) = (fx.rig.clone(), fx.log.path().to_path_buf());
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(move || {
                std::mem::forget(rig.registry_on_this_thread());
                std::mem::forget(open_admission());
                binding_events::set_test_root(Some(&log));
            })
            .build()
            .unwrap()
    };
    let effects = Arc::new(Crashing::default());
    let installed = host_effects_for_tests(effects.clone());
    rt.block_on(async {
        let pool = fx.db.as_ref().unwrap().connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
        let session_state = super::session(fx.channel_id, fx.channel_name.clone(), &shared);
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(fx.channel_id, session_state);
        let clear = super::super::super::clear_channel_session_state(
            &fx.http,
            &shared,
            &ProviderKind::Claude,
            fx.channel_id,
            "!clear",
            super::super::super::SoftClearNotifyMode::Suppress,
        );
        let mut clear = Box::pin(clear);
        let deadline = Clock::now() + LIMIT;
        let saving = format!("save:{session}");
        let mut logged_at = None;
        loop {
            let sent = !fx.rig.sends().is_empty();
            if sent && logged && logged_at.is_none() {
                clear_hook(fx, home, session, false);
                logged_at = Some(());
            }
            let calls = effects.calls.lock().unwrap().clone();
            if sent && (!logged || calls.contains(&saving)) {
                break;
            }
            assert!(Clock::now() < deadline, "the clear never sent: {calls:?}");
            tokio::select! {
                result = &mut clear => panic!("the clear ended before the crash: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    });
    drop(installed);
    rt.shutdown_timeout(Duration::from_secs(10));
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(fx.channel_id.get());
    effects.calls.lock().unwrap().clone()
}

/// A boot after that crash holds input until the Pending is logged, completes it with one save and
/// no second line or reset, and its next turn prompts the cleared session once.
fn assert_boot_completes_the_crashed_clear(n: u64, logged: bool) {
    let fx = Fixture::new(n);
    let home = Home::new();
    let z = uuid();
    let crashed = clear_then_crash(&fx, &home, &z, logged);
    let key = &fx.session_key;
    let mut before = vec![format!("clear:{key}")];
    if logged {
        before.push(format!("save:{z}"));
    }
    assert_eq!(crashed, before, "the crashed process saved nothing durable");
    assert_eq!(fx.rig.sends(), clear_line(), "one line before the crash");

    let _booted = host_effects_for_tests(fx.fake.clone());
    fx.rt.block_on(async {
        assert!(
            matches!(fx.state().await, NativeClearBoundary::Unresolved { .. }),
            "the crash left the clear unresolved"
        );
        if !logged {
            assert_eq!(
                fx.admits().await.0,
                false,
                "input is held without its Pending"
            );
            assert!(
                matches!(fx.state().await, NativeClearBoundary::Unresolved { .. }),
                "still unresolved"
            );
            assert_eq!(
                fx.fake.calls(),
                Vec::<String>::new(),
                "nothing reset or saved"
            );
            clear_hook(&fx, &home, &z, false);
        }
        assert_eq!(fx.admits().await, (true, Some(z.clone())));
        assert_eq!(fx.state().await, NativeClearBoundary::Resolved);
        assert_eq!(
            fx.fake.calls(),
            [format!("save:{z}")],
            "the late Pending saved once"
        );
        assert_eq!(
            fx.admits().await,
            (true, Some("stale".into())),
            "settled once"
        );
    });

    let turn = next_turn(&fx, &home, &z, 1);
    let lines = [clear_line(), prompt_lines()].concat();
    assert_eq!(
        fx.rig.sends(),
        lines,
        "no second `/clear`, one paste and Enter: {:?}",
        turn.0
    );
    assert_turn_took(&fx, &home, &z, &turn);
    assert_eq!(fx.fake.calls(), [format!("save:{z}")]);
    fx.rt.block_on(assert_execution_kept(&fx));
}

#[test]
fn a_crash_before_the_clears_pending_is_logged_completes_at_boot_without_a_second_line_pg() {
    assert_boot_completes_the_crashed_clear(12, false);
}

#[test]
fn a_crash_after_the_clears_pending_is_logged_completes_at_boot_without_a_second_line_pg() {
    assert_boot_completes_the_crashed_clear(13, true);
}

/// The clear entry reading the rotation projection a real hosted O actor published to this
/// process's readiness, with no forced count.
mod o_actor {
    use crate::services::claude_tui::hook_server::HookEventKind;
    use crate::services::tui_o::cutover::test_override as cutover;
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::shadow::{self, ShadowProvider, binding_reader::source_id_for};
    use crate::services::tui_o::writer::activation::ActivationFacts;
    use crate::services::tui_o::writer::actor::POLL_INTERVAL;
    use crate::services::tui_o::writer::adoption::{LegacyView, NoLegacy};
    use crate::services::tui_o::writer::binding as o;
    use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, test_io};

    use super::super::Fixture;
    use super::*;

    /// The pane's log as O reads it: startup, then each clear's bind onto a new source.
    struct Hops {
        events: Mutex<Vec<o::BindingEvent>>,
        notice: tokio::sync::watch::Sender<u64>,
    }

    impl Hops {
        fn new() -> Arc<Self> {
            let notice = tokio::sync::watch::channel(0).0;
            Arc::new(Self {
                events: Mutex::default(),
                notice,
            })
        }

        fn bind(&self, channel: u64, old: Option<&shadow::SourceId>, new: &shadow::SourceId) {
            let mut events = self.events.lock().unwrap();
            let seq = events.len() as u64 + 1;
            let cause = match old {
                None => o::BindingCause::Startup,
                Some(_) => o::BindingCause::Clear,
            };
            let evidence = o::BindingEvidence {
                hook_event: HookEventKind::SessionStart.as_str().into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            };
            events.push(o::BindingEvent {
                seq,
                channel_id: channel,
                provider: ShadowProvider::Claude,
                tmux_session: "pane".into(),
                execution_nonce: "nonce".into(),
                record: o::BindingRecord::Bound {
                    old: old.cloned(),
                    new: o::BindingTarget::Source(new.clone()),
                    cause,
                    parent_hint: old.cloned(),
                    evidence,
                },
                committed_at: chrono::Utc::now(),
            });
            self.notice.send_replace(seq);
        }
    }

    impl o::BindingEvents for Hops {
        fn binding_events_since(
            &self,
            channel: u64,
            after: u64,
        ) -> Result<Vec<o::BindingEvent>, String> {
            let events = self.events.lock().unwrap();
            let since = events
                .iter()
                .filter(|e| e.channel_id == channel && e.seq > after);
            Ok(since.cloned().collect())
        }

        fn subscribe(&self, _: u64) -> tokio::sync::watch::Receiver<u64> {
            self.notice.subscribe()
        }
    }

    /// The gateway side of the writer host over that log; nothing of Legacy holds the channel.
    struct Gateway {
        posts: Arc<test_io::Posts>,
        alarms: test_io::Alarms,
        log: Arc<Hops>,
    }

    impl HostIo for Gateway {
        type Port = test_io::Posts;
        type Lease = test_io::AnyLease;
        type Alarms = test_io::Alarms;
        type Bindings = Hops;

        fn port(&self) -> impl Future<Output = Arc<test_io::Posts>> + Send {
            std::future::ready(Arc::clone(&self.posts))
        }

        fn lease(&self) -> test_io::AnyLease {
            test_io::AnyLease
        }

        fn alarms(&self) -> test_io::Alarms {
            self.alarms.clone()
        }

        fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<Hops> {
            Arc::clone(&self.log)
        }

        fn activation_facts(
            &self,
            _: u64,
            _: ShadowProvider,
        ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
            std::future::ready(Ok(ActivationFacts::default()))
        }

        fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
            Ok(Custody::Free)
        }

        fn legacy(&self) -> Arc<dyn LegacyView> {
            Arc::new(NoLegacy)
        }

        fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
            std::future::ready(false)
        }

        fn relaying(&self, _: u64) -> bool {
            false
        }
    }

    /// Drops the fixture's forced projection, so the entry reads this process's readiness.
    fn unforce_projection(fx: &mut Fixture) {
        let probe = host::force_unsettled_for_test(None);
        let forced = std::any::Any::type_id(&probe);
        drop(probe);
        let at = fx
            ._thread
            .iter()
            .position(|guard| (**guard).type_id() == forced);
        drop(
            fx._thread
                .remove(at.expect("the fixture forces a projection")),
        );
    }

    // No actor gives no count, a clear's rotated-away source the running actor has not retired gives
    // a typed refusal before any change, and once O retires it the entry clears with one line.
    #[test]
    fn the_clear_entry_follows_a_hosted_actors_projection_until_its_rotation_settles_pg() {
        let mut fx = Fixture::new(14);
        unforce_projection(&mut fx);
        let channel = fx.channel_id.get();
        let before = fx.rt.block_on(fx.state());
        let refused = |what: &str| {
            let error = fx.rt.block_on(fx.clear()).unwrap_err().to_string();
            fx.rt.block_on(fx.assert_untouched(what));
            assert_eq!(fx.rt.block_on(fx.state()), before, "{what}: no boundary");
            error
        };
        let error = refused("no actor");
        assert!(error.contains("rotation_unread"), "{error}");

        let o_root = tempfile::tempdir().unwrap();
        let a_path = o_root.path().join("a.jsonl");
        std::fs::write(&a_path, b"").unwrap();
        let a = source_id_for("s1", &a_path).unwrap();
        let log = Hops::new();
        log.bind(channel, None, &a);
        let io = Arc::new(Gateway {
            posts: Arc::default(),
            alarms: test_io::Alarms::default(),
            log: Arc::clone(&log),
        });
        let _candidates = cutover::force_candidates(&[(channel, ClaudeTui)]);
        let gate = Arc::new(OwnershipGate::default());
        let writer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let polls =
            |count: u32| writer.block_on(async { tokio::time::sleep(POLL_INTERVAL * count).await });
        let parts = || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(o_root.path().to_path_buf()),
            gate: Arc::clone(&gate),
            readiness: host::process_readiness(),
        };
        let tasks = writer.block_on(async { host::start(ShadowProvider::Claude, true, parts) });
        gate.acquired();
        polls(3);
        let alarms = || io.alarms.0.lock().unwrap().clone();
        assert_eq!(host::rotation_unsettled(channel), Some(0), "{:?}", alarms());

        let b_path = o_root.path().join("b.jsonl");
        let row = json!({"type": "assistant", "uuid": "u-n1", "apiBlockIndex": 0,
            "message": {"id": "n1", "content": [{"type": "text", "text": "new first"}]}});
        std::fs::write(&b_path, format!("{row}\n")).unwrap();
        log.bind(channel, Some(&a), &source_id_for("s2", &b_path).unwrap());
        polls(3);
        assert_eq!(host::rotation_unsettled(channel), Some(1), "{:?}", alarms());
        let error = refused("unsettled");
        assert!(error.contains("rotation_unsettled(1)"), "{error}");
        assert_eq!(fx.rt.block_on(fx.session()), (Some("old".into()), false));

        polls(12);
        assert_eq!(
            host::rotation_unsettled(channel),
            Some(0),
            "retired once quiet"
        );
        fx.rt.block_on(async {
            let result = fx
                .clear_with(|| fx.record("new", BindingCause::Clear, true))
                .await;
            assert_eq!(fx.rig.sends(), clear_line(), "one gated line");
            assert_eq!(fx.session().await, (Some("new".into()), true));
            result.expect("the settled rotation admits the clear");
        });
        assert_eq!(io.posts.to(channel), ["new first"]);
        tasks.iter().for_each(tokio::task::JoinHandle::abort);
        polls(1);
    }
}
