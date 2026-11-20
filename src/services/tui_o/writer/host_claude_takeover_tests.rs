//! A reserved store-less Claude channel through the real host: a transcript bound after the floor
//! commits before its first output; a moved source rebases on proof the old one was delivered.

use super::super::late_host::Legacy;
use super::*;
use crate::config::Config;
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;

const SESSION: &str = "c1a0de00-0000-4000-8000-000000007618";

fn reserve(channel: u64) -> test_override::ChannelsGuard {
    reserve_for(channel, None)
}

fn reserve_for(channel: u64, parent: Option<u64>) -> test_override::ChannelsGuard {
    let boot = boot_for(parent.unwrap_or(channel));
    if let Some(parent) = parent {
        use crate::services::session_host::surface_policy::VerifiedSurface;
        let thread = thread_for(channel, parent);
        boot.reserve_boot_children(&[(channel, parent)]);
        assert!(boot.resolve_child(
            channel,
            VerifiedSurface::from_discord(&thread, ClaudeTui).unwrap(),
            false
        ));
    }
    assert!(boot.output_candidate(channel).unwrap().late_adopt_origin());
    test_override::force_boot(boot)
}

fn boot_for(selected: u64) -> BootChannels {
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "c", "name": "C",
            "channels": {"claude": {"id": selected.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [selected], "late_adopt": true},
            "turn": {"channels": [selected]}}
    }))
    .unwrap();
    let pending = |_: &_, _| Ok(BTreeMap::from([(selected, Adoption::Pending)]));
    BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, pending)
        .unwrap()
}

fn thread_for(channel: u64, parent: u64) -> poise::serenity_prelude::GuildChannel {
    serde_json::from_value(serde_json::json!({
        "id": channel.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "test"
    }))
    .unwrap()
}

fn record(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

/// What Claude writes through a first prompt, before any output.
fn prompted() -> Vec<u8> {
    prompt_in(SESSION, "u1")
}

fn prompt_in(session: &str, user_id: &str) -> Vec<u8> {
    [
        record(serde_json::json!({"type": "mode", "mode": "normal", "sessionId": session})),
        record(serde_json::json!({"type": "file-history-snapshot", "messageId": "m"})),
        record(
            serde_json::json!({"type": "user", "sessionId": session, "uuid": user_id,
            "message": {"role": "user", "content": "hi"}}),
        ),
        record(
            serde_json::json!({"type": "attachment", "sessionId": session,
            "attachment": {"type": "skill_listing"}}),
        ),
    ]
    .concat()
}

fn answer(id: &str, text: &str) -> Vec<u8> {
    answer_in(SESSION, id, text)
}

fn answer_in(session: &str, id: &str, text: &str) -> Vec<u8> {
    record(
        serde_json::json!({"type": "assistant", "sessionId": session, "uuid": format!("a-{id}"),
        "apiBlockIndex": 0, "message": {"id": id, "role": "assistant",
        "content": [{"type": "text", "text": text}]}}),
    )
}

fn turn_end() -> Vec<u8> {
    turn_end_in(SESSION)
}

fn turn_end_in(session: &str) -> Vec<u8> {
    record(
        serde_json::json!({"type": "system", "subtype": "turn_duration", "durationMs": 5,
        "sessionId": session}),
    )
}

#[cfg(unix)]
fn closed_reply(session: &str, id: &str, text: &str) -> Vec<u8> {
    [
        prompt_in(session, &format!("u-{id}")),
        answer_in(session, id, text),
        turn_end_in(session),
    ]
    .concat()
}

/// The channel's binding log as the hook writes it: each line one more seq.
struct Binds {
    channel: u64,
    tmux: String,
    lines: Mutex<Vec<u8>>,
    seq: AtomicU64,
}

impl Binds {
    fn new(channel: u64) -> Self {
        Self {
            channel,
            tmux: format!("claude-takeover-fixture-{channel}"),
            lines: Mutex::default(),
            seq: AtomicU64::new(0),
        }
    }

    fn bind(&self, root: &Path, nonce: &str, new: p5::BindingTarget) -> u64 {
        self.append(root, nonce, None, new, p5::BindingCause::Startup)
    }

    fn append(
        &self,
        root: &Path,
        nonce: &str,
        old: Option<SourceId>,
        new: p5::BindingTarget,
        cause: p5::BindingCause,
    ) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = p5::BindingEvent {
            seq,
            channel_id: self.channel,
            provider: "claude".into(),
            tmux_session: self.tmux.clone(),
            execution_nonce: Some(nonce.into()),
            old,
            new,
            cause,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: (cause == p5::BindingCause::SameSessionVerified)
                    .then(|| "UserPromptSubmit".into()),
                received_at: Utc::now(),
            },
            committed_at: Utc::now(),
        };
        let mut lines = self.lines.lock().unwrap();
        let mut entry = serde_json::to_value(&event).unwrap();
        if cause == p5::BindingCause::SameSessionVerified {
            entry["verified"] = true.into();
        }
        lines.extend(serde_json::to_vec(&entry).unwrap());
        lines.push(b'\n');
        p5_log(root, self.channel, &lines);
        seq
    }

    fn source(&self, root: &Path, nonce: &str, path: &Path) -> SourceId {
        let source = source_id_for(SESSION, path).unwrap();
        self.bind(root, nonce, p5::BindingTarget::Source(source.clone()));
        source
    }
}

struct Reserved {
    harness: Harness,
    io: Arc<TestIo>,
    legacy: Arc<Legacy>,
    binds: Binds,
    host: Option<ManagedWriterHandle>,
    _reserved: test_override::ChannelsGuard,
}

impl Reserved {
    fn start_host(&mut self) {
        assert!(self.host.is_none());
        let readiness = Arc::new(Readiness::default());
        let owned = cutover::boot_ownership()
            .into_iter()
            .filter(|(channel, _, _)| *channel == self.harness.channel_id)
            .collect();
        let mut handles = start_managed(ShadowProvider::Claude, true, owned, || HostParts {
            io: self.io.clone(),
            runtime_root: Some(self.root().to_owned()),
            gate: self.harness.gate.clone(),
            readiness,
        });
        assert_eq!(handles.len(), 1);
        self.host = handles.pop();
    }

    #[cfg(unix)]
    async fn stop_host(&mut self) {
        let host = self.host.take().unwrap();
        host.keep_reservation();
        let stopped = host.stop_and_join().await.unwrap();
        assert_eq!(stopped.channel, self.harness.channel_id);
    }

    fn root(&self) -> &Path {
        self.harness._runtime.path()
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn init(&self) -> Vec<(SourceId, u64)> {
        let channel = self.harness.channel_id;
        let init = self.harness.store.read_init(channel).unwrap().unwrap();
        let origin = self.harness.store.read_init_origin(channel).unwrap();
        assert_eq!(
            origin,
            crate::services::tui_o::store::InitOrigin::LateClaudeTakeover
        );
        (init.sources.into_iter())
            .map(|source| (source.source_id, source.delivery_start))
            .collect()
    }
}

/// A reserved host over `before`, a log already written as the reservation reads it first.
async fn reserved(channel: u64, before: impl FnOnce(&Harness, &Binds) -> PathBuf) -> Reserved {
    reserved_for(channel, None, before).await
}

async fn reserved_for(
    channel: u64,
    parent: Option<u64>,
    before: impl FnOnce(&Harness, &Binds) -> PathBuf,
) -> Reserved {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve_for(channel, parent);
    p5::set_test_root(Some(harness._runtime.path()));
    let binds = Binds::new(channel);
    let path = before(&harness, &binds);
    let io = TestIo::over(&harness);
    io.late_enabled.store(true, Ordering::SeqCst);
    let legacy = Arc::new(Legacy {
        path,
        frontier: AtomicU64::new(0),
    });
    *io.legacy.lock().unwrap() = Some(legacy.clone());
    let mut fixture = Reserved {
        harness,
        io,
        legacy,
        binds,
        host: None,
        _reserved: reserved,
    };
    fixture.start_host();
    polls(3).await;
    assert_eq!(reserved_adoption(channel), Adoption::Deferred);
    fixture
}

fn reserved_adoption(channel: u64) -> Adoption {
    test_override::with_channels(|boot| boot.unwrap().output_candidate(channel).unwrap().peek())
}

async fn settle() {
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        polls(3).await;
    }
}

fn legacy_sends(channel: u64) -> bool {
    let claim = crate::services::tui_o::cutover::o_owns_tui_output_for_channel_reserving;
    !claim(channel, Some(ClaudeTui)).unwrap().owned
}

#[tokio::test(start_paused = true)]
async fn late_claude_thread_startup_and_same_execution_verification_take_one_source() {
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};

    const CH: u64 = 8_118_130;
    const PARENT: u64 = 8_118_131;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(PARENT, "test-node")]);
    let f = reserved_for(CH, Some(PARENT), |h, _| {
        h._runtime.path().join("thread.jsonl")
    })
    .await;
    let floor = test_override::with_channels(|boot| {
        boot.unwrap()
            .output_candidate(CH)
            .unwrap()
            .late_boundary()
            .unwrap()
    });
    assert_eq!(floor.takeover_after, 0);
    crate::services::tmux_common::write_tmux_runtime_kind_marker(&f.binds.tmux, ClaudeTui).unwrap();
    let path = f.file("thread.jsonl", &prompted());
    let source = f.binds.source(f.root(), "thread-launch", &path);
    f.binds.append(
        f.root(),
        "thread-launch",
        Some(source.clone()),
        p5::BindingTarget::Source(source.clone()),
        p5::BindingCause::SameSessionVerified,
    );
    settle().await;
    assert_eq!(reserved_adoption(CH), Adoption::Committed);
    assert_eq!(f.init(), [(source, 0)]);
    let claim = Some(BodyClaim::tmux(CH, Some(&f.binds.tmux)));
    let legacy = std::cell::Cell::new(0);
    let sent = claim_then_send_at(BodySite::WatcherStreaming, claim, || async {
        legacy.set(legacy.get() + 1);
    })
    .await;
    assert_eq!(sent, Ok(BodySend::OwnedByO));
    assert_eq!(legacy.get(), 0);
    append(&path, &answer("msg_1", "thread answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["thread answer"]);
}

#[cfg(unix)]
#[tokio::test]
async fn existing_tmux_thread_holds_buffered_body_until_previous_sources_are_proven_pg() {
    use crate::services::provider::ProviderKind;
    use crate::services::tui_o::cutover::{
        BodyClaim, BodySend, BodySite, IdentityError, claim_then_send_at,
    };
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    use crate::services::turn_host::{self, TurnHost};

    const CH: u64 = 8_118_132;
    const PARENT: u64 = 8_118_133;
    const KEY: &str = "test-node:claude-takeover-fixture-8118132";
    const OLD: &str = "c1a0de00-0000-4000-8000-000000007619";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'disconnected', 'discord_channel', '0123456789abcdef', $2)")
        .bind(KEY)
        .bind(CH.to_string())
        .execute(&pool)
        .await.unwrap();
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(PARENT, "test-node")]);
    let _policy = reserve_for(CH, Some(PARENT));
    assert!(matches!(
        turn_host::for_turn(Some(&pool), &ProviderKind::Claude, CH, Some(KEY)).await,
        TurnHost::Tmux
    ));
    tokio::time::pause();
    let mut f = reserved_for(CH, Some(PARENT), |h, binds| {
        let root = h._runtime.path();
        for (name, nonce) in [("old-source", "old"), ("middle-source", "middle")] {
            let path = root.join(name);
            std::fs::write(
                &path,
                record(serde_json::json!({"type": "mode", "sessionId": OLD})),
            )
            .unwrap();
            let source = source_id_for(OLD, &path).unwrap();
            binds.bind(root, nonce, p5::BindingTarget::Source(source));
        }
        root.join("middle-source")
    })
    .await;
    let missing = f.root().join("old-source");
    let aside = f.root().join("old-source-aside");
    std::fs::rename(&missing, &aside).unwrap();
    *f.io.custody.lock().unwrap() = Ok(Custody::Active);
    let policy = cutover::claude_thread::launch_policy(CH);
    assert!(policy.is_some());
    let launch = PreparedIncarnation::prepare_pinned(
        "claude",
        &f.binds.tmux,
        Some(CH),
        Some(SESSION),
        false,
        Some(f.root().to_owned()),
        (None, policy),
    )
    .unwrap();
    crate::services::tmux_common::host_marker::record_tmux_host_marker(&f.binds.tmux);
    let nonce = &launch.context.execution_nonce;
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&f.binds.tmux, "spawn_nonce"),
        nonce,
    )
    .unwrap();
    let tmux = f.binds.tmux.clone();
    let claim = BodyClaim::tmux(CH, Some(&tmux));
    let legacy = std::cell::Cell::new(0);
    let send = || async { legacy.set(legacy.get() + 1) };
    assert_eq!(
        claim_then_send_at(BodySite::WatcherStreaming, Some(claim), send).await,
        Err(IdentityError::ChildDeliveryHeld)
    );
    {
        let _before_restore = test_override::force_channels(&[(PARENT, ClaudeTui)]);
        assert_eq!(claim.peek(), Err(IdentityError::ChildDeliveryHeld));
    }
    let path = f.file(
        "current.jsonl",
        &[
            prompted(),
            answer("msg_2", "buffered child answer"),
            turn_end(),
        ]
        .concat(),
    );
    let source = f.binds.source(f.root(), nonce, &path);
    f.binds.append(
        f.root(),
        nonce,
        Some(source.clone()),
        p5::BindingTarget::Source(source.clone()),
        p5::BindingCause::SameSessionVerified,
    );
    settle().await;
    assert_eq!(reserved_adoption(CH), Adoption::Deferred);
    assert!(f.harness.port.posts().is_empty());
    assert_eq!(
        claim_then_send_at(BodySite::WatcherStreaming, Some(claim), send).await,
        Err(IdentityError::ChildDeliveryHeld)
    );
    assert_eq!(legacy.get(), 0);
    f.stop_host().await;
    let boot = boot_for(PARENT);
    boot.reserve_boot_children(&[(CH, PARENT)]);
    let _restored = test_override::force_boot(boot);
    assert_eq!(claim.peek(), Err(IdentityError::ChildDeliveryHeld));
    cutover::claude_thread::record_restored(&thread_for(CH, PARENT), &f.binds.tmux);
    assert_eq!(reserved_adoption(CH), Adoption::Deferred);
    f.start_host();
    tokio::time::advance(std::time::Duration::from_secs(81 * 60)).await;
    assert_eq!(claim.peek(), Err(IdentityError::ChildDeliveryHeld));
    assert_eq!(reserved_adoption(CH), Adoption::Deferred);
    *f.io.custody.lock().unwrap() = Ok(Custody::Free);
    *f.io.intake_pool.lock().unwrap() = Some(pool.clone());
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while f.harness.port.posts().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(reserved_adoption(CH), Adoption::Committed);
    assert_eq!(f.init(), [(source, 0)]);
    assert_eq!(f.harness.port.posts(), ["buffered child answer"]);
    assert_eq!(
        claim_then_send_at(BodySite::WatcherStreaming, Some(claim), send).await,
        Ok(BodySend::OwnedByO)
    );
    assert_eq!(legacy.get(), 0);
    f.stop_host().await;
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn reserved_child_preserves_old_source_offer_but_holds_reused_turn_nonce_on_new_source() {
    use crate::services::tui_o::channel_policy::LegacyRun;
    use crate::services::tui_o::cutover::{
        BodyClaim, BodySend, BodySite, IdentityError, claim_then_send_at,
    };
    use crate::services::tui_o::writer::adoption::LegacyDelivery;
    use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

    const CH: u64 = 8_118_134;
    const PARENT: u64 = 8_118_135;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = reserved_for(CH, Some(PARENT), |h, binds| {
        let root = h._runtime.path();
        let old = root.join("old.jsonl");
        std::fs::write(
            &old,
            [prompted(), answer("old", "old answer"), turn_end()].concat(),
        )
        .unwrap();
        binds.source(root, "old-execution", &old);
        old
    })
    .await;
    let candidate =
        test_override::with_channels(|boot| boot.unwrap().output_candidate(CH).unwrap());
    let binding = ChannelBindingLog::new(CH, ShadowProvider::Claude)
        .binding_events_since(CH, 0)
        .unwrap()
        .remove(0);
    let old = &f.legacy.path;
    let run = LegacyRun {
        binding,
        turn: Some((
            1,
            "start".into(),
            Some(f.binds.tmux.clone()),
            Some(0),
            Some("same-turn".into()),
        )),
        observed_end: old.metadata().unwrap().len(),
        delivery: Some(LegacyDelivery {
            source_path: old.clone(),
            execution_nonce: Some("old-execution".into()),
            owner: CH,
            generation: 1,
        }),
    };
    assert!(candidate.release_child_run(CH, run.clone()));
    assert!(candidate.retry_child_at_boundary(&run, 1));
    let launch = PreparedIncarnation::prepare_pinned(
        "claude",
        &f.binds.tmux,
        Some(CH),
        Some(SESSION),
        false,
        Some(f.root().to_owned()),
        (None, cutover::claude_thread::launch_policy(CH)),
    )
    .unwrap();
    crate::services::tmux_common::host_marker::record_tmux_host_marker(&f.binds.tmux);
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&f.binds.tmux, "spawn_nonce"),
        &launch.context.execution_nonce,
    )
    .unwrap();
    let body = BodyClaim::tmux(CH, Some(&f.binds.tmux)).for_turn(Some("same-turn"));
    let old_body = body.for_source(old.to_str());
    let new_body = body.for_source(f.root().join("new.jsonl").to_str());
    assert_eq!(old_body.peek(), Ok(false));
    assert_eq!(new_body.peek(), Err(IdentityError::ChildDeliveryHeld));
    assert_eq!(
        claim_then_send_at(BodySite::WatcherStreaming, Some(old_body), || async {
            "old answer"
        })
        .await,
        Ok(BodySend::Sent("old answer"))
    );
    assert_eq!(
        claim_then_send_at(BodySite::WatcherStreaming, Some(new_body), || async {
            panic!("new body escaped")
        })
        .await,
        Err::<BodySend<()>, _>(IdentityError::ChildDeliveryHeld)
    );
    assert_eq!(candidate.sends(), (1, 1));
}

#[cfg(unix)]
async fn seed_resume_row(pool: &sqlx::PgPool, channel: u64, tmux: &str) {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, claude_session_id) VALUES ($1, 'claude', 'disconnected', 'discord_channel', '0123456789abcdef', $2, $3)")
        .bind(format!("test-node:{tmux}"))
        .bind(channel.to_string())
        .bind(SESSION)
        .execute(pool)
        .await
        .unwrap();
}

#[cfg(unix)]
fn mark_fixture_launch(f: &Reserved, native: &str, resume: bool) -> String {
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    let policy = cutover::claude_thread::launch_policy(f.binds.channel);
    assert!(policy.is_some());
    let launch = PreparedIncarnation::prepare_pinned(
        "claude",
        &f.binds.tmux,
        Some(f.binds.channel),
        Some(native),
        resume,
        Some(f.root().to_owned()),
        (None, policy),
    )
    .unwrap();
    crate::services::tmux_common::host_marker::record_tmux_host_marker(&f.binds.tmux);
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&f.binds.tmux, "spawn_nonce"),
        &launch.context.execution_nonce,
    )
    .unwrap();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(&f.binds.tmux, ClaudeTui).unwrap();
    launch.context.execution_nonce
}

#[cfg(unix)]
fn verify_fixture_launch(f: &Reserved, source: &SourceId, nonce: &str) {
    f.binds.append(
        f.root(),
        nonce,
        Some(source.clone()),
        p5::BindingTarget::Source(source.clone()),
        p5::BindingCause::SameSessionVerified,
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_thread_keeps_old_history_and_posts_only_its_new_turn_pg() {
    const CH: u64 = 8_118_136;
    const PARENT: u64 = 8_118_137;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (channel, warm) in [(CH, false), (CH + 20, true)] {
        exercise_resume(&pool, channel, PARENT, warm).await;
    }
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
async fn exercise_resume(pool: &sqlx::PgPool, ch: u64, parent: u64, warm: bool) {
    use crate::services::claude::tui_session_launch::{ResumePane, reserve_thread_body_before_key};
    use crate::services::provider::ProviderKind;
    use crate::services::tui_o::cutover::{BodyClaim, IdentityError};
    use crate::services::turn_host::{self, TurnHost};

    let tmux = Binds::new(ch).tmux;
    seed_resume_row(pool, ch, &tmux).await;
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(parent, "test-node")]);
    let _policy = reserve_for(ch, Some(parent));
    assert!(matches!(
        turn_host::for_turn(
            Some(pool),
            &ProviderKind::Claude,
            ch,
            Some(&format!("test-node:{tmux}"))
        )
        .await,
        TurnHost::Tmux
    ));
    tokio::time::pause();
    let mut f = reserved_for(ch, Some(parent), |h, binds| {
        let root = h._runtime.path();
        let path = root.join("resumed.jsonl");
        std::fs::write(&path, closed_reply(SESSION, "old", "old answer")).unwrap();
        let source = binds.source(root, "old-execution", &path);
        binds.append(
            root,
            "old-execution",
            Some(source.clone()),
            p5::BindingTarget::Source(source),
            p5::BindingCause::SameSessionVerified,
        );
        path
    })
    .await;
    let path = &f.legacy.path;
    let start = path.metadata().unwrap().len();
    let source = source_id_for(SESSION, path).unwrap();
    if warm {
        let nonce = mark_fixture_launch(&f, SESSION, true);
        assert!(cutover::record_thread_child(&thread_for(ch, parent), false));
        reserve_thread_body_before_key(Some(ch), &tmux, Some(&nonce), SESSION, path, true).unwrap();
    } else {
        cutover::claude_thread::reserve_resume(ch, &tmux, SESSION, path, ResumePane::Cold).unwrap();
        let nonce = mark_fixture_launch(&f, SESSION, true);
        reserve_thread_body_before_key(Some(ch), &tmux, Some(&nonce), SESSION, path, false)
            .unwrap();
    }
    let body = BodyClaim::tmux(ch, Some(&f.binds.tmux));
    assert_eq!(body.peek(), Err(IdentityError::ChildDeliveryHeld));
    append(path, &closed_reply(SESSION, "new", "new answer"));
    *f.io.intake_pool.lock().unwrap() = Some(pool.clone());
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while f.harness.port.posts().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.init(), [(source, start)]);
    assert_eq!(f.harness.port.posts(), ["new answer"]);
    assert_eq!(body.peek(), Ok(true));
    f.stop_host().await;
}

#[cfg(unix)]
#[tokio::test]
async fn protected_thread_restart_needs_no_receipts_for_closed_legacy_sources_pg() {
    const CH: u64 = 8_118_138;
    const PARENT: u64 = 8_118_139;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    exercise_protected_thread_restart(&pool, CH, PARENT, p5::BindingCause::Startup).await;
    exercise_protected_thread_restart(&pool, CH + 20, PARENT, p5::BindingCause::Clear).await;
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
async fn exercise_protected_thread_restart(
    pool: &sqlx::PgPool,
    channel: u64,
    parent: u64,
    successor: p5::BindingCause,
) {
    use crate::services::tui_o::cutover::{
        BodyClaim,
        intake_route::{IntakeRoute, route_for_placement},
    };

    const NEXT_SESSION: &str = "c1a0de00-0000-4000-8000-000000007620";
    seed_resume_row(pool, channel, &Binds::new(channel).tmux).await;
    tokio::time::pause();
    let mut f = reserved_for(channel, Some(parent), |h, binds| {
        let root = h._runtime.path();
        for (name, nonce) in [("old-A", "old-a"), ("old-B", "old-b")] {
            let path = root.join(name);
            std::fs::write(&path, closed_reply(SESSION, name, "already handled")).unwrap();
            binds.source(root, nonce, &path);
        }
        root.join("old-B")
    })
    .await;
    *f.io.custody.lock().unwrap() = Ok(Custody::Active);
    let nonce = mark_fixture_launch(&f, SESSION, false);
    let path = f.file("new-C", &closed_reply(SESSION, "new-C", "only C"));
    let source = f.binds.source(f.root(), &nonce, &path);
    verify_fixture_launch(&f, &source, &nonce);
    f.stop_host().await;
    assert!(f.harness.store.read_init(channel).unwrap().is_none());
    assert!(f.harness.port.posts().is_empty());

    let successor_nonce = if successor == p5::BindingCause::Clear {
        nonce
    } else {
        mark_fixture_launch(&f, NEXT_SESSION, false)
    };
    let next = f.file("new-D", &closed_reply(NEXT_SESSION, "new-D", "only D"));
    let next = source_id_for(NEXT_SESSION, &next).unwrap();
    f.binds.append(
        f.root(),
        &successor_nonce,
        Some(source.clone()),
        p5::BindingTarget::Source(next.clone()),
        successor,
    );
    verify_fixture_launch(&f, &next, &successor_nonce);
    let boot = boot_for(parent);
    boot.reserve_boot_children(&[(channel, parent)]);
    let _restored = test_override::force_boot(boot);
    cutover::claude_thread::record_restored(&thread_for(channel, parent), &f.binds.tmux);
    assert_eq!(
        route_for_placement("claude", channel),
        IntakeRoute::Unselected
    );
    *f.io.custody.lock().unwrap() = Ok(Custody::Free);
    *f.io.intake_pool.lock().unwrap() = Some(pool.clone());
    f.start_host();
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while f.harness.port.posts().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the protected new answer must not wait for closed Legacy receipts");
    assert_eq!(f.init(), [(source, 0), (next, 0)]);
    assert_eq!(f.harness.port.posts(), ["only C", "only D"]);
    assert_eq!(
        BodyClaim::tmux(channel, Some(&f.binds.tmux)).peek(),
        Ok(true)
    );
    f.stop_host().await;
}

/// A fresh launch binds Pending until its transcript exists; once resolved with its first prompt
/// and no output, O commits it from its first byte and posts the answer once, and Legacy does not.
#[tokio::test(start_paused = true)]
async fn late_claude_first_turn_from_pending_bind_is_taken_over() {
    const CH: u64 = 63_761_901;
    let f = reserved(CH, |h, _| h._runtime.path().join("t.jsonl")).await;
    let pending = p5::BindingTarget::Pending {
        payload_session_id: SESSION.into(),
        payload_transcript_path: None,
    };
    let at = f.binds.bind(f.root(), "n1", pending);
    polls(3).await;
    assert_eq!(adoption(CH), Adoption::Deferred, "a Pending bind waits");
    let path = f.file("t.jsonl", &prompted());
    let source = source_id_for(SESSION, &path).unwrap();
    let resolved = p5::BindingTarget::Resolved {
        pending_seq: at,
        source: source.clone(),
    };
    f.binds.bind(f.root(), "n1", resolved);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.halted()
    );
    assert_eq!(f.init(), [(source, 0)]);
    assert!(!legacy_sends(CH), "Legacy sends no body once O committed");
    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["the answer"]);
}

/// A Legacy body send after the floor may be this transcript's streamed turn: it is not taken.
#[tokio::test(start_paused = true)]
async fn late_claude_takeover_skips_when_legacy_sent_since_baseline() {
    const CH: u64 = 63_761_902;
    let f = reserved(CH, |h, _| h._runtime.path().join("t.jsonl")).await;
    test_hook::set(CH, Step::BeforeFence, move || {
        assert!(legacy_sends(CH));
        Ok(())
    });
    let path = f.file("t.jsonl", &prompted());
    f.binds.source(f.root(), "n1", &path);
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(
        !f.harness.store.has_channel_dir(CH),
        "the streamed turn stays Legacy's"
    );
}

/// Old transcript delivered and sampled; a new turn makes Legacy busy, the generation's frontier
/// drops to 0 and a cleared transcript binds: O takes it while busy, by the sample alone.
#[tokio::test(start_paused = true)]
async fn late_claude_rotation_rebases_with_new_discord_turn_busy() {
    const CH: u64 = 63_761_903;
    let old = [prompted(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(old.len() as u64, Ordering::SeqCst);
    let io = f.io.clone();
    test_hook::set(CH, Step::AfterFence, move || {
        io.busy.store(true, Ordering::SeqCst);
        Err("the new turn claimed the mailbox".into())
    });
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(
        f.io.busy.load(Ordering::SeqCst),
        "the quiet handoff was tried once"
    );
    f.legacy.frontier.store(0, Ordering::SeqCst);
    append(
        &f.legacy.path,
        &record(serde_json::json!({"type": "cost-state", "sessionId": SESSION})),
    );
    let path = f.file("new.jsonl", &prompted());
    let source = f.binds.source(f.root(), "n-new", &path);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.released()
    );
    assert_eq!(f.init(), [(source, 0)]);
}

/// Legacy's read cursor at EOF is not delivery: with its frontier before the answer, no sample
/// proves the old transcript, so the cleared one is not taken and the reservation stays Deferred.
#[tokio::test(start_paused = true)]
async fn late_claude_read_cursor_at_eof_without_delivery_does_not_rebase_into_a_takeover() {
    const CH: u64 = 63_761_904;
    let head = prompted();
    let old = [head.clone(), answer("msg_0", "undelivered"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(head.len() as u64, Ordering::SeqCst);
    settle().await;
    let path = f.file("new.jsonl", &prompted());
    f.binds.source(f.root(), "n-new", &path);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}

/// A first read under Legacy custody fixes no deferred baseline, yet a transcript already bound
/// then is not new: it is not taken over.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_boundary_does_not_take_a_bind_from_before_the_first_read() {
    const CH: u64 = 63_761_905;
    let harness = fresh_on(CH);
    let binds = Binds::new(CH);
    let path = harness._runtime.path().join("t.jsonl");
    std::fs::write(&path, prompted()).unwrap();
    p5::set_test_root(Some(harness._runtime.path()));
    binds.source(harness._runtime.path(), "n1", &path);
    harness.gate.acquired();
    let _reserved = reserve(CH);
    let io = TestIo::over(&harness);
    io.late_enabled.store(true, Ordering::SeqCst);
    *io.custody.lock().unwrap() = Ok(Custody::Row);
    let legacy = Arc::new(Legacy {
        path,
        frontier: AtomicU64::new(0),
    });
    *io.legacy.lock().unwrap() = Some(legacy);
    let ready = Arc::new(Readiness::default());
    let _tasks = hosted_as(ShadowProvider::Claude, &harness, &io, (true, &ready));
    settle().await;
    let candidate = (cutover::boot_ownership().into_iter())
        .find_map(|(channel, _, candidate)| (channel == CH).then_some(candidate))
        .flatten();
    let boundary = candidate.unwrap().late_boundary().unwrap();
    assert_eq!((boundary.baseline, boundary.takeover_after), (0, 1));
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CH));
}

/// The old transcript answered past Legacy's frontier, or delivered when `delivered` but with its
/// commit taken; Legacy then moves to a cleared transcript and delivers its whole turn.
async fn moved_on(channel: u64, delivered: bool) -> (Reserved, SourceId, u64) {
    let head = prompted();
    let old = [head.clone(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(channel, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    let frontier = if delivered { old.len() } else { head.len() };
    f.legacy.frontier.store(frontier as u64, Ordering::SeqCst);
    // A new Discord turn takes the commit of the old transcript Legacy delivered and sampled.
    if delivered {
        let io = f.io.clone();
        test_hook::set(channel, Step::AfterFence, move || {
            io.busy.store(true, Ordering::SeqCst);
            Err("the new turn claimed the mailbox".into())
        });
    }
    settle().await;
    assert_eq!(adoption(channel), Adoption::Deferred);
    let new = [prompted(), answer("msg_1", "new"), turn_end()].concat();
    let path = f.file("new.jsonl", &new);
    let moved = Legacy {
        path: path.clone(),
        frontier: AtomicU64::new(new.len() as u64),
    };
    *f.io.legacy.lock().unwrap() = Some(Arc::new(moved));
    let source = f.binds.source(f.root(), "n-new", &path);
    f.io.busy.store(false, Ordering::SeqCst);
    settle().await;
    settle().await;
    (f, source, new.len() as u64)
}

/// With the old transcript's answer undelivered, the cleared one finishing its turn and Legacy
/// delivering it to EOF does not let O start the old one at EOF: the channel stays Legacy's.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_keeps_an_undelivered_old_transcript_owed_after_the_new_turn() {
    const CH: u64 = 63_761_906;
    let (f, _, _) = moved_on(CH, false).await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}

/// The old transcript sampled delivered settles its obligation, so the quiet handoff commits the
/// cleared one at its EOF once Legacy delivered its turn.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_commits_once_the_old_transcript_was_proven_delivered() {
    const CH: u64 = 63_761_907;
    let (f, source, len) = moved_on(CH, true).await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.released()
    );
    let init = f.harness.store.read_init(CH).unwrap().unwrap();
    let new = init
        .sources
        .iter()
        .find(|pinned| pinned.source_id == source);
    assert_eq!(new.map(|pinned| pinned.delivery_start), Some(len));
}

/// The old transcript delivered and sampled, two rotations land before the next look: B with an
/// answer Legacy never delivered, then C delivered to EOF. B stays owed, so C is not committed.
#[tokio::test(start_paused = true)]
async fn late_claude_rotations_read_together_keep_the_middle_transcript_owed() {
    const CH: u64 = 63_761_908;
    let old = [prompted(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(old.len() as u64, Ordering::SeqCst);
    let io = f.io.clone();
    test_hook::set(CH, Step::AfterFence, move || {
        io.busy.store(true, Ordering::SeqCst);
        Err("the new turn claimed the mailbox".into())
    });
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    let turn = |id, text| [prompted(), answer(id, text), turn_end()].concat();
    let middle = f.file("middle.jsonl", &turn("msg_1", "undelivered"));
    let last = turn("msg_2", "delivered");
    let path = f.file("last.jsonl", &last);
    f.binds.source(f.root(), "n-middle", &middle);
    f.binds.source(f.root(), "n-last", &path);
    let moved = Legacy {
        path,
        frontier: AtomicU64::new(last.len() as u64),
    };
    *f.io.legacy.lock().unwrap() = Some(Arc::new(moved));
    f.io.busy.store(false, Ordering::SeqCst);
    settle().await;
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}
