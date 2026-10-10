//! Redrive retirement gates: a retired request or owner key stops before the snapshot, the nudge
//! or the reattach, with no accounting; each fixture's Legacy run reaches the real nudge or apply.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::tests::{clear_redrive_test_state, watcher_handle};
use super::{HealthRegistry, REDRIVE_ATTEMPTS, REDRIVE_PLACEHOLDER_SHIELDS, stall_liveness};
use crate::services::discord::SharedData;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, tree_fingerprint,
};
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::platform::tmux;
use crate::services::provider::ProviderKind;

const GRACE_SECS: i64 = stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;

/// Checkpoints named by the gate site that follows them.
type Stage = &'static str;
const AFTER_SNAPSHOT: Stage = "redrive_snapshot";
const BEFORE_REATTACH: Stage = "redrive_reattach";

struct Plan {
    reached: Vec<Stage>,
    retire: Option<(Stage, String, u64)>,
    retired: Option<RetiredForTest>,
}

static PLANS: LazyLock<Mutex<HashMap<ChannelId, Plan>>> = LazyLock::new(Default::default);

/// Records that the pass reached `stage`; an armed plan retires its key at that instant.
pub(super) fn checkpoint(channel: ChannelId, stage: Stage) {
    let mut plans = PLANS.lock().unwrap();
    let Some(plan) = plans.get_mut(&channel) else {
        return;
    };
    plan.reached.push(stage);
    if plan.retired.is_none()
        && let Some((_, provider, key)) = plan.retire.as_ref().filter(|(at, ..)| *at == stage)
    {
        plan.retired = Some(RetiredForTest::new(provider, *key));
    }
}

/// Watches one pass over `channel`; dropping it forgets the plan and un-retires its key.
struct Probe(ChannelId);

impl Probe {
    fn arm(channel: ChannelId, retire: Option<(Stage, &ProviderKind, ChannelId)>) -> Self {
        let plan = Plan {
            reached: Vec::new(),
            retire: retire
                .map(|(at, provider, key)| (at, provider.as_str().to_string(), key.get())),
            retired: None,
        };
        PLANS.lock().unwrap().insert(channel, plan);
        Self(channel)
    }

    fn reached(&self) -> Vec<Stage> {
        PLANS.lock().unwrap()[&self.0].reached.clone()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let plan = PLANS.lock().unwrap().remove(&self.0);
        drop(plan);
    }
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

/// The redrive accounting a pass can leave behind: attempt state and placeholder shield.
fn accounting(shared: &SharedData, provider: &ProviderKind, channel: ChannelId) -> (String, bool) {
    let key = shared.redrive_key(provider, channel);
    let attempts = REDRIVE_ATTEMPTS.get(&key).map(|state| state.clone());
    (
        format!("{attempts:?}"),
        REDRIVE_PLACEHOLDER_SHIELDS.contains_key(&key),
    )
}

/// Durable bytes under `root` except the no-progress grace baseline, which the pass records
/// before it decides to promote.
fn bytes_beyond_grace(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut tree = tree_fingerprint(root);
    tree.retain(|path, _| {
        !path
            .components()
            .any(|part| part.as_os_str() == "discord_redrive_baselines")
    });
    tree
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

    fn accounting(&self) -> (String, bool) {
        accounting(&self.shared, &self.provider, self.channel)
    }

    fn clear(&self) {
        inflight::clear_inflight_state(&self.provider, self.channel.get());
        clear_redrive_test_state(&self.shared, &self.provider, self.channel, &self.session.0);
        if self.owner != self.channel {
            clear_redrive_test_state(&self.shared, &self.provider, self.owner, &self.session.0);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Retire {
    Never,
    BeforeEntry,
    AfterSnapshot,
}

/// A retired key never reaches the nudge: retired before entry it takes no snapshot, retired
/// while the snapshot awaits it stops right after; the Legacy run moves the resume offset.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn retired_redrive_key_never_moves_the_watcher_resume_offset() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    for (index, retire) in [Retire::Never, Retire::BeforeEntry, Retire::AfterSnapshot]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(6_325_615_001 + index as u64);
        let fixture = NudgeFixture::new(tmp.path(), channel, channel);
        let base = chrono::Utc::now().timestamp();
        fixture.seed_grace(base).await;
        let (before_accounting, before_tree) = (fixture.accounting(), tree_fingerprint(tmp.path()));
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

        match retire {
            Retire::Never => {
                assert!(result, "the Legacy key is nudged");
                assert_eq!(
                    fixture.resumed_at(),
                    Some(BIRTH_OFFSET),
                    "the nudge moved the offset"
                );
                assert_ne!(
                    fixture.accounting(),
                    before_accounting,
                    "the nudge is accounted"
                );
                assert!(fixture.accounting().1, "a committed nudge arms the shield");
            }
            Retire::BeforeEntry | Retire::AfterSnapshot => {
                assert!(
                    !result,
                    "{retire:?}: a retired key reports no redrive action"
                );
                assert_eq!(
                    fixture.resumed_at(),
                    None,
                    "{retire:?}: resume offset untouched"
                );
                assert_eq!(
                    fixture.accounting(),
                    before_accounting,
                    "{retire:?}: no accounting"
                );
                assert_eq!(
                    tree_fingerprint(tmp.path()),
                    before_tree,
                    "{retire:?}: durable runtime bytes untouched"
                );
                let snapshots = usize::from(matches!(retire, Retire::AfterSnapshot));
                assert_eq!(
                    probe.reached(),
                    vec![AFTER_SNAPSHOT; snapshots],
                    "{retire:?}: a key retired before entry takes no snapshot"
                );
            }
        }
        drop(probe);
        fixture.clear();
    }
}

/// The nudge targets the watcher owner's slot, so a retired owner stops the pass even when the
/// requested key is still Legacy; with the owner Legacy the same fixture nudges that slot.
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
        let before_accounting = fixture.accounting();
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
                fixture.accounting(),
                before_accounting,
                "no redrive accounting"
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

/// With no watcher the redrive escalates to a reattach whose apply spawns one; a key retired
/// right before that promotion spawns nothing, keeps its row and leaves no accounting.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_before_reattach_promotion_spawns_no_watcher() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    for (index, retire) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6_325_617_001 + index as u64);
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
        let output_path = tmp
            .path()
            .join(format!("g15-reattach-{}.jsonl", channel.get()));
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
        row.updated_at = started_at;
        row.turn_nonce = cancel_token.turn_nonce().map(str::to_string);
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        inflight::save_inflight_state(&row).expect("seed authoritative inflight");
        clear_redrive_test_state(&shared, &provider, channel, &session.0);

        let base = chrono::Utc::now().timestamp();
        let redrive =
            |now| registry.redrive_undelivered_backlog_at(&provider, shared.clone(), channel, now);
        assert!(
            !redrive(base - GRACE_SECS)
                .await
                .expect("seed redrive grace")
        );
        let before_tree = bytes_beyond_grace(tmp.path());
        let probe = Probe::arm(
            channel,
            retire.then_some((BEFORE_REATTACH, &provider, channel)),
        );

        let result = redrive(base).await.expect("redrive entrypoint");

        assert_eq!(
            probe.reached(),
            vec![AFTER_SNAPSHOT, BEFORE_REATTACH],
            "the nudge declined for want of a watcher and the pass reached the promotion"
        );
        let spawned = shared
            .tmux_watchers
            .remove(&channel)
            .map(|(_, watcher)| watcher);
        if let Some(watcher) = spawned.as_ref() {
            watcher.cancel.store(true, Ordering::Relaxed);
        }
        let key = shared.redrive_key(&provider, channel);
        let attempt = REDRIVE_ATTEMPTS.get(&key).map(|state| state.clone());
        if retire {
            assert!(!result, "a retired key reports no redrive action");
            assert!(
                spawned.is_none(),
                "no watcher is installed for a retired key"
            );
            assert_eq!(
                bytes_beyond_grace(tmp.path()),
                before_tree,
                "row and durable bytes untouched"
            );
            let attempt = attempt.expect("the attempt decision preceded the promotion");
            assert_eq!(
                (attempt.attempts, attempt.retry_not_before_unix),
                (0, None),
                "a retirement is neither a refusal nor a no-op cooldown"
            );
            assert!(
                !REDRIVE_PLACEHOLDER_SHIELDS.contains_key(&key),
                "no shield is armed"
            );
        } else {
            assert!(result, "the Legacy key is reattached");
            assert!(spawned.is_some(), "the apply spawned a watcher");
            let adopted = inflight::load_inflight_state_read_only(&provider, channel.get())
                .expect("the reattach keeps the row");
            assert!(
                adopted.readopted_from_inflight,
                "the rebind adopted the row"
            );
            assert_eq!(
                attempt.map(|state| state.attempts),
                Some(1),
                "the reattach is accounted"
            );
        }
        drop(probe);
        inflight::clear_inflight_state(&provider, channel.get());
        clear_redrive_test_state(&shared, &provider, channel, &session.0);
    }
}
