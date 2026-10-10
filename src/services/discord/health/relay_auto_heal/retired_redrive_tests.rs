//! Redrive retirement gates: a retired request or owner key stops before the snapshot, the nudge
//! or the reattach, with no accounting; each fixture's Legacy run reaches the real nudge or apply.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::tests::{clear_redrive_test_state, watcher_handle};
use super::{
    HealthRegistry, REDRIVE_ATTEMPTS, REDRIVE_PLACEHOLDER_SHIELDS, RedriveEpisode, stall_liveness,
};
use crate::services::discord::SharedData;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, tree_fingerprint,
};
use crate::services::discord::inflight::{self, InflightTurnIdentity, InflightTurnState};
use crate::services::platform::tmux;
use crate::services::provider::ProviderKind;

const GRACE_SECS: i64 = stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;

/// Checkpoints named by the gate site that follows them.
type Stage = &'static str;
const AFTER_SNAPSHOT: Stage = "redrive_snapshot";
const BEFORE_REATTACH: Stage = "redrive_reattach";

type Tree = BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)>;

/// Everything a pass can leave behind: full redrive accounting of some keys and every durable
/// byte under the runtime root.
#[derive(Debug, PartialEq)]
struct Observed {
    accounting: String,
    tree: Tree,
}

fn observe(
    shared: &SharedData,
    provider: &ProviderKind,
    keys: &[ChannelId],
    root: &Path,
) -> Observed {
    let accounting = keys
        .iter()
        .map(|channel| {
            let key = shared.redrive_key(provider, *channel);
            let attempt = REDRIVE_ATTEMPTS.get(&key).map(|state| state.clone());
            let shield = REDRIVE_PLACEHOLDER_SHIELDS
                .get(&key)
                .map(|shield| shield.clone());
            format!("{channel}: attempt={attempt:?} shield={shield:?}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Observed {
        accounting,
        tree: tree_fingerprint(root),
    }
}

type Capture = Box<dyn Fn() -> Observed + Send>;

struct Plan {
    reached: Vec<Stage>,
    retire: Option<(Stage, String, u64)>,
    retired: Option<RetiredForTest>,
    capture: Option<Capture>,
    captured: Option<Observed>,
}

static PLANS: LazyLock<Mutex<HashMap<ChannelId, Plan>>> = LazyLock::new(Default::default);

/// Records that the pass reached `stage`; an armed plan observes the state and then retires its
/// key at that instant.
pub(super) fn checkpoint(channel: ChannelId, stage: Stage) {
    let mut plans = PLANS.lock().unwrap();
    let Some(plan) = plans.get_mut(&channel) else {
        return;
    };
    plan.reached.push(stage);
    if plan.retired.is_none()
        && let Some((_, provider, key)) = plan.retire.as_ref().filter(|(at, ..)| *at == stage)
    {
        plan.captured = plan.capture.as_ref().map(|capture| capture());
        plan.retired = Some(RetiredForTest::new(provider, *key));
    }
}

/// Watches one pass over `channel`; dropping it forgets the plan and un-retires its key.
struct Probe(ChannelId);

impl Probe {
    fn arm(channel: ChannelId, retire: Option<(Stage, &ProviderKind, ChannelId)>) -> Self {
        Self::arm_capturing(channel, retire, None)
    }

    fn arm_capturing(
        channel: ChannelId,
        retire: Option<(Stage, &ProviderKind, ChannelId)>,
        capture: Option<Capture>,
    ) -> Self {
        let plan = Plan {
            reached: Vec::new(),
            retire: retire
                .map(|(at, provider, key)| (at, provider.as_str().to_string(), key.get())),
            retired: None,
            capture,
            captured: None,
        };
        PLANS.lock().unwrap().insert(channel, plan);
        Self(channel)
    }

    fn reached(&self) -> Vec<Stage> {
        PLANS.lock().unwrap()[&self.0].reached.clone()
    }

    fn captured(&self) -> Option<Observed> {
        PLANS.lock().unwrap().get_mut(&self.0)?.captured.take()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let plan = PLANS.lock().unwrap().remove(&self.0);
        drop(plan);
    }
}

/// Logs every tmux invocation through a PATH wrapper, so a test counts the snapshot's real
/// producer probes instead of trusting a checkpoint placed after the snapshot.
struct TmuxLog {
    log: PathBuf,
    _dir: tempfile::TempDir,
    _path: crate::config::TestEnvVarGuard,
}

impl TmuxLog {
    fn install() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let real = std::env::split_paths(&inherited)
            .map(|dir| dir.join("tmux"))
            .find(|candidate| candidate.is_file())
            .expect("tmux on PATH");
        let dir = tempfile::tempdir().expect("tmux wrapper dir");
        let log = dir.path().join("calls.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        );
        let wrapper = dir.path().join("tmux");
        std::fs::write(&wrapper, script).expect("tmux wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("chmod tmux wrapper");
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
        Self {
            log,
            _dir: dir,
            _path: path,
        }
    }

    /// tmux invocations so far whose arguments name `session`.
    fn calls_naming(&self, session: &str) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(session))
            .count()
    }
}

/// Structured events recorded so far for `channel`, by type.
fn events_for(channel: ChannelId) -> Vec<String> {
    crate::services::observability::events::recent(500)
        .into_iter()
        .filter(|event| event.channel_id == Some(channel.get()))
        .map(|event| event.event_type)
        .collect()
}

/// Durable paths whose bytes or mtime differ between two trees, including added or removed ones.
fn changed_paths(before: &Tree, after: &Tree) -> Vec<PathBuf> {
    let paths: std::collections::BTreeSet<&PathBuf> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

/// A live tmux session killed on drop.
struct Session(String);

impl Session {
    fn start(name: String) -> Self {
        let _ = tmux::kill_session(&name, "retired redrive fixture reset");
        assert!(
            tmux::create_session(&name, None, "sleep 120")
                .expect("start tmux fixture")
                .status
                .success(),
            "the redrive snapshot must observe a live producer tmux session"
        );
        Self(name)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = tmux::kill_session(&self.0, "retired redrive fixture cleanup");
    }
}

/// A Codex turn with an undelivered backlog and a live watcher registered under `owner`.
struct NudgeFixture {
    registry: HealthRegistry,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    owner: ChannelId,
    resume_offset: Arc<Mutex<Option<u64>>>,
    session: Session,
}

const BIRTH_OFFSET: u64 = 22_299_791;

impl NudgeFixture {
    fn new(tmp: &Path, channel: ChannelId, owner: ChannelId) -> Self {
        let provider = ProviderKind::Codex;
        let session = Session::start(format!(
            "AgentDesk-codex-g15n{}-{}",
            channel.get(),
            std::process::id()
        ));
        let output_path = tmp.join(format!("g15-nudge-{}.jsonl", channel.get()));
        std::fs::File::create(&output_path)
            .expect("create capture fixture")
            .set_len(24_553_403)
            .expect("size capture fixture");
        let output_path = output_path.to_string_lossy().into_owned();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        shared.tmux_watchers.insert(
            owner,
            watcher_handle(
                &session.0,
                &output_path,
                resume_offset.clone(),
                Arc::new(AtomicBool::new(true)),
            ),
        );
        let started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            0,
            channel.get() + 1,
            0,
            "test".to_string(),
            None,
            Some(session.0.clone()),
            Some(output_path),
            None,
            BIRTH_OFFSET,
        );
        row.started_at = started_at.clone();
        row.updated_at = started_at;
        inflight::save_inflight_state(&row).expect("seed authoritative inflight");
        clear_redrive_test_state(&shared, &provider, channel, &session.0);
        Self {
            registry: HealthRegistry::new(),
            shared,
            provider,
            channel,
            owner,
            resume_offset,
            session,
        }
    }

    async fn redrive(
        &self,
        now: i64,
    ) -> Result<bool, crate::services::discord::relay_recovery::RelayRecoveryError> {
        self.registry
            .redrive_undelivered_backlog_at(&self.provider, self.shared.clone(), self.channel, now)
            .await
    }

    async fn seed_grace(&self, base: i64) {
        assert!(
            !self
                .redrive(base - GRACE_SECS)
                .await
                .expect("seed redrive grace"),
            "the initial observation seeds the no-progress grace"
        );
    }

    fn resumed_at(&self) -> Option<u64> {
        *self.resume_offset.lock().unwrap()
    }

    fn observe(&self, root: &Path) -> Observed {
        let mut keys = vec![self.channel];
        if self.owner != self.channel {
            keys.push(self.owner);
        }
        observe(&self.shared, &self.provider, &keys, root)
    }

    fn clear(&self) {
        inflight::clear_inflight_state(&self.provider, self.channel.get());
        clear_redrive_test_state(&self.shared, &self.provider, self.channel, &self.session.0);
        if self.owner != self.channel {
            clear_redrive_test_state(&self.shared, &self.provider, self.owner, &self.session.0);
        }
    }
}

/// A Claude turn with no watcher, a frozen relay and a matching mailbox anchor: the redrive
/// escalates to a reattach the planner admits and the apply spawns a watcher.
struct ReattachFixture {
    registry: HealthRegistry,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    user_msg_id: u64,
    started_at: String,
    turn_nonce: Option<String>,
    discord: MockDiscord,
    session: Session,
}

impl ReattachFixture {
    async fn new(tmp: &Path, channel: ChannelId) -> Self {
        use crate::services::agent_protocol::RuntimeHandoffKind;
        let provider = ProviderKind::Claude;
        let user_msg_id = channel.get() + 11;
        let session = Session::start(format!(
            "AgentDesk-claude-g15r{}-{}-cc",
            channel.get(),
            std::process::id()
        ));
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &session.0,
            RuntimeHandoffKind::ClaudeTui,
        )
        .expect("runtime kind marker");
        let output_path = tmp.join(format!("g15-reattach-{}.jsonl", channel.get()));
        std::fs::write(&output_path, vec![b'x'; 128]).expect("seed capture fixture");
        let output_path = output_path.to_string_lossy().into_owned();

        let discord = MockDiscord::start().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        registry
            .register_http(provider.as_str().to_string(), discord.http.clone())
            .await;
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                cancel_token.clone(),
                UserId::new(1),
                MessageId::new(user_msg_id),
            )
            .await,
            "the mailbox anchor matches the row the reattach adopts"
        );
        // Older than the desync threshold, so the frozen frontier reads as a dead relay.
        let started_at = (chrono::Local::now() - chrono::Duration::seconds(600))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            343_742_347,
            user_msg_id,
            channel.get() + 21,
            "retired reattach redrive".to_string(),
            Some(format!("provider-session-g15-{}", channel.get())),
            Some(session.0.clone()),
            Some(output_path),
            None,
            0,
        );
        row.started_at = started_at.clone();
        row.updated_at = started_at.clone();
        row.turn_nonce = cancel_token.turn_nonce().map(str::to_string);
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        inflight::save_inflight_state(&row).expect("seed authoritative inflight");
        clear_redrive_test_state(&shared, &provider, channel, &session.0);
        Self {
            registry,
            shared,
            provider,
            channel,
            user_msg_id,
            started_at,
            turn_nonce: row.turn_nonce.clone(),
            discord,
            session,
        }
    }

    async fn redrive(
        &self,
        now: i64,
    ) -> Result<bool, crate::services::discord::relay_recovery::RelayRecoveryError> {
        self.registry
            .redrive_undelivered_backlog_at(&self.provider, self.shared.clone(), self.channel, now)
            .await
    }

    async fn seed_grace(&self, base: i64) {
        assert!(
            !self
                .redrive(base - GRACE_SECS)
                .await
                .expect("seed redrive grace"),
            "the initial observation seeds the no-progress grace"
        );
    }

    /// Removes and cancels the watcher the apply installed, if any.
    fn take_spawned_watcher(&self) -> Option<crate::services::discord::TmuxWatcherHandle> {
        let spawned = self
            .shared
            .tmux_watchers
            .remove(&self.channel)
            .map(|(_, watcher)| watcher);
        if let Some(watcher) = spawned.as_ref() {
            watcher.cancel.store(true, Ordering::Relaxed);
        }
        spawned
    }

    fn clear(&self) {
        inflight::clear_inflight_state(&self.provider, self.channel.get());
        clear_redrive_test_state(&self.shared, &self.provider, self.channel, &self.session.0);
    }
}

#[derive(Clone, Copy, Debug)]
enum Retire {
    Never,
    BeforeEntry,
    AfterSnapshot,
}

/// A retired key never reaches the nudge: retired before entry it spends no snapshot probe,
/// retired while the snapshot awaits it stops right after; nothing it leaves behind changes.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn retired_redrive_key_never_moves_the_watcher_resume_offset() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let tmux_log = TmuxLog::install();
    for (index, retire) in [Retire::Never, Retire::BeforeEntry, Retire::AfterSnapshot]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(6_325_615_001 + index as u64);
        let fixture = NudgeFixture::new(tmp.path(), channel, channel);
        let base = chrono::Utc::now().timestamp();
        fixture.seed_grace(base).await;
        let before = fixture.observe(tmp.path());
        let probes_before = tmux_log.calls_naming(&fixture.session.0);
        let _retired_before = matches!(retire, Retire::BeforeEntry)
            .then(|| RetiredForTest::new(fixture.provider.as_str(), channel.get()));
        let probe = Probe::arm(
            channel,
            matches!(retire, Retire::AfterSnapshot).then_some((
                AFTER_SNAPSHOT,
                &fixture.provider,
                channel,
            )),
        );

        let result = fixture.redrive(base).await.expect("redrive entrypoint");

        let probes = tmux_log.calls_naming(&fixture.session.0) - probes_before;
        match retire {
            Retire::Never => {
                assert!(result, "the Legacy key is nudged");
                assert!(probes > 0, "the snapshot probes the producer session");
                assert_eq!(fixture.resumed_at(), Some(BIRTH_OFFSET));
            }
            Retire::BeforeEntry => {
                assert!(!result, "a retired key reports no redrive action");
                assert_eq!(probes, 0, "a key retired before entry takes no snapshot");
                assert!(probe.reached().is_empty());
            }
            Retire::AfterSnapshot => {
                assert!(!result, "a retired key reports no redrive action");
                assert!(probes > 0, "the snapshot ran before the retirement landed");
                assert_eq!(probe.reached(), vec![AFTER_SNAPSHOT]);
            }
        }
        if !matches!(retire, Retire::Never) {
            assert_eq!(
                fixture.resumed_at(),
                None,
                "{retire:?}: resume offset untouched"
            );
            assert_eq!(
                fixture.observe(tmp.path()),
                before,
                "{retire:?}: accounting and durable bytes untouched"
            );
        }
        drop(probe);
        fixture.clear();
    }
}

/// The nudge targets the watcher owner's slot, so a retired owner stops the pass even when the
/// requested key is still Legacy, leaving both keys' accounting as it was.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn retired_watcher_owner_key_stops_the_nudge_of_a_legacy_request() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    for (index, owner_retired) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6_325_616_001 + 10 * index as u64);
        let owner = ChannelId::new(channel.get() + 5);
        let fixture = NudgeFixture::new(tmp.path(), channel, owner);
        let base = chrono::Utc::now().timestamp();
        fixture.seed_grace(base).await;
        let before = fixture.observe(tmp.path());
        let _retired =
            owner_retired.then(|| RetiredForTest::new(fixture.provider.as_str(), owner.get()));
        let probe = Probe::arm(channel, None);

        let result = fixture.redrive(base).await.expect("redrive entrypoint");

        assert_eq!(
            probe.reached(),
            vec![AFTER_SNAPSHOT],
            "the request key is Legacy"
        );
        if owner_retired {
            assert!(!result, "a retired owner slot is not nudged");
            assert_eq!(
                fixture.resumed_at(),
                None,
                "the owner watcher offset is untouched"
            );
            assert_eq!(
                fixture.observe(tmp.path()),
                before,
                "request and owner accounting and durable bytes untouched"
            );
        } else {
            assert!(result, "the Legacy owner slot is nudged");
            assert_eq!(
                fixture.resumed_at(),
                Some(BIRTH_OFFSET),
                "the owner watcher was nudged"
            );
        }
        drop(probe);
        fixture.clear();
    }
}

/// A key retired right before the reattach promotion spawns nothing and leaves everything as it
/// stood at that instant; the same fixture's Legacy run really spawns a watcher.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_before_reattach_promotion_spawns_no_watcher() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    for (index, retire) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6_325_617_001 + index as u64);
        let fixture = ReattachFixture::new(tmp.path(), channel).await;
        let base = chrono::Utc::now().timestamp();
        fixture.seed_grace(base).await;
        let (shared, provider, root) = (
            fixture.shared.clone(),
            fixture.provider.clone(),
            tmp.path().to_path_buf(),
        );
        let capture: Capture = Box::new(move || observe(&shared, &provider, &[channel], &root));
        let probe = Probe::arm_capturing(
            channel,
            retire.then_some((BEFORE_REATTACH, &fixture.provider, channel)),
            Some(capture),
        );

        let result = fixture.redrive(base).await.expect("redrive entrypoint");

        assert_eq!(
            probe.reached(),
            vec![AFTER_SNAPSHOT, BEFORE_REATTACH],
            "the nudge declined for want of a watcher and the pass reached the promotion"
        );
        let spawned = fixture.take_spawned_watcher();
        if retire {
            assert!(!result, "a retired key reports no redrive action");
            assert!(
                spawned.is_none(),
                "no watcher is installed for a retired key"
            );
            let at_gate = probe
                .captured()
                .expect("state observed at the promotion gate");
            assert_eq!(
                observe(&fixture.shared, &fixture.provider, &[channel], tmp.path()),
                at_gate,
                "nothing changes after the promotion gate refuses a retired key"
            );
        } else {
            assert!(result, "the Legacy key is reattached");
            assert!(spawned.is_some(), "the apply spawned a watcher");
        }
        drop(probe);
        fixture.clear();
    }
}

/// With nothing retired the nudge pass leaves exactly the pre-gate outcome: the offset, the full
/// attempt state and shield, and no durable change beyond the grace baseline it always refreshes.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn empty_retired_set_keeps_the_legacy_nudge_outcome() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let channel = ChannelId::new(6_325_618_001);
    let fixture = NudgeFixture::new(tmp.path(), channel, channel);
    let row = inflight::load_inflight_state_read_only(&fixture.provider, channel.get())
        .expect("seeded row");
    let base = chrono::Utc::now().timestamp();
    fixture.seed_grace(base).await;
    let (tree_before, events_before) = (tree_fingerprint(tmp.path()), events_for(channel));
    let millis_before = chrono::Utc::now().timestamp_millis();

    let result = fixture.redrive(base).await.expect("redrive entrypoint");

    let millis_after = chrono::Utc::now().timestamp_millis();
    assert!(result, "the Legacy key is nudged");
    assert_eq!(fixture.resumed_at(), Some(BIRTH_OFFSET));
    let key = fixture.shared.redrive_key(&fixture.provider, channel);
    let attempt = REDRIVE_ATTEMPTS
        .get(&key)
        .map(|state| state.clone())
        .expect("attempt");
    let shield_started = attempt.shield_started_at_millis.expect("shield start");
    assert!((millis_before..=millis_after).contains(&shield_started));
    let episode = RedriveEpisode {
        frontier: 0,
        reset_incarnation: 0,
        identity: Some(InflightTurnIdentity {
            user_msg_id: channel.get() + 1,
            started_at: row.started_at.clone(),
            tmux_session_name: Some(fixture.session.0.clone()),
            turn_start_offset: Some(BIRTH_OFFSET),
        }),
        turn_nonce: Some(
            row.turn_nonce
                .clone()
                .expect("the seeded row carries a nonce"),
        ),
        reconnect_count: 0,
    };
    assert_eq!(attempt.episode, episode);
    assert_eq!(
        (
            attempt.attempts,
            attempt.last_attempt_unix,
            attempt.capped_alarm_emitted,
            attempt.retry_not_before_unix,
        ),
        (1, base, false, None)
    );
    assert_eq!(
        REDRIVE_PLACEHOLDER_SHIELDS
            .get(&key)
            .map(|shield| shield.clone()),
        Some((episode, shield_started)),
        "the shield carries the committed episode and the attempt's own start time"
    );
    let baseline = tmp.path().join(format!(
        "runtime/discord_redrive_baselines/codex/{}.json",
        channel.get()
    ));
    assert_eq!(
        changed_paths(&tree_before, &tree_fingerprint(tmp.path())),
        vec![baseline],
        "only the grace baseline is rewritten"
    );
    assert_eq!(
        &events_for(channel)[events_before.len()..],
        &[] as &[String]
    );
    fixture.clear();
}

/// With nothing retired the reattach pass leaves exactly the pre-gate outcome: a spawned watcher,
/// the re-adopted row, the full attempt state and shield, and the same Discord traffic.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_retired_set_keeps_the_legacy_reattach_outcome() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let channel = ChannelId::new(6_325_618_101);
    let fixture = ReattachFixture::new(tmp.path(), channel).await;
    let base = chrono::Utc::now().timestamp();
    fixture.seed_grace(base).await;
    let (tree_before, events_before) = (tree_fingerprint(tmp.path()), events_for(channel));
    let millis_before = chrono::Utc::now().timestamp_millis();

    let result = fixture.redrive(base).await.expect("redrive entrypoint");

    let millis_after = chrono::Utc::now().timestamp_millis();
    let requests = fixture.discord.requests_for(channel.get());
    let spawned = fixture.take_spawned_watcher();
    assert!(result, "the Legacy key is reattached");
    assert!(spawned.is_some(), "the apply spawned a watcher");
    let adopted = inflight::load_inflight_state_read_only(&fixture.provider, channel.get())
        .expect("the reattach keeps the row");
    assert!(
        adopted.readopted_from_inflight,
        "the rebind adopted the row"
    );
    let key = fixture.shared.redrive_key(&fixture.provider, channel);
    let attempt = REDRIVE_ATTEMPTS
        .get(&key)
        .map(|state| state.clone())
        .expect("attempt");
    let shield = REDRIVE_PLACEHOLDER_SHIELDS
        .get(&key)
        .map(|shield| shield.clone());
    let shield_started = attempt.shield_started_at_millis.expect("shield start");
    assert!((millis_before..=millis_after).contains(&shield_started));
    // The rebind bumps the reconnect count and the committed episode follows it.
    let episode = RedriveEpisode {
        frontier: 0,
        reset_incarnation: 0,
        identity: Some(InflightTurnIdentity {
            user_msg_id: fixture.user_msg_id,
            started_at: fixture.started_at.clone(),
            tmux_session_name: Some(fixture.session.0.clone()),
            turn_start_offset: Some(0),
        }),
        turn_nonce: fixture.turn_nonce.clone(),
        reconnect_count: 1,
    };
    assert_eq!(attempt.episode, episode);
    assert_eq!(
        (
            attempt.attempts,
            attempt.last_attempt_unix,
            attempt.capped_alarm_emitted,
            attempt.retry_not_before_unix,
        ),
        (1, base, false, None)
    );
    assert_eq!(shield, Some((episode, shield_started)));
    let runtime = tmp.path().join("runtime");
    let channel_file = format!("claude/{}.json", channel.get());
    assert_eq!(
        changed_paths(&tree_before, &tree_fingerprint(tmp.path())),
        vec![
            runtime.join("discord_inflight").join(&channel_file),
            runtime
                .join("discord_redrive_baselines")
                .join(&channel_file),
            runtime
                .join("discord_relay_recovery_circuit")
                .join(&channel_file),
            runtime
                .join("discord_relay_recovery_circuit")
                .join(format!("{channel_file}.lock")),
        ],
        "the rebind rewrites its row and reserves its circuit; nothing else is touched"
    );
    assert_eq!(
        &events_for(channel)[events_before.len()..],
        &[] as &[String]
    );
    assert_eq!(
        requests,
        Vec::<String>::new(),
        "the reattach itself sends nothing to Discord"
    );
    fixture.clear();
}
