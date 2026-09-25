//! #5951 C3r — a persisted TUI-direct episode re-enters the mailbox only while
//! the remint fence would still re-open it (I21). Same-process scope only.
use super::*;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::{mailbox_snapshot, make_shared_data_for_tests};
use crate::services::tui_prompt_dedupe::{ExternalInputRelayLease, TuiRuntimeBinding};
use crate::services::turn_orchestrator::ActiveTurnKind;
use poise::serenity_prelude::UserId;
use std::time::Instant;
use synthetic_start::bridge_handoff::ADMISSION_PAUSE;

const TMUX: &str = "c3r-fenced-admission";
const OTHER_TMUX: &str = "c3r-fenced-admission-other";
type Pause = std::sync::Mutex<Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>;
/// The mailbox slot, the active counter and the turn start clock.
type Slot = (Option<MessageId>, Option<String>, usize, Option<Instant>);

fn run<Fut: std::future::Future<Output = ()>>(body: impl FnOnce(PathBuf) -> Fut) {
    let _telemetry = crate::services::observability::lock_env_then_runtime();
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let output = temp.path().join("transcript.jsonl");
    std::fs::write(&output, "{}\n".repeat(64)).unwrap();
    for tmux in [TMUX, OTHER_TMUX] {
        let generation = crate::services::tmux_common::session_temp_path(tmux, "generation");
        std::fs::write(generation, b"1").unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    runtime.unwrap().block_on(body(output));
}

fn lease(tmux: &str, channel: ChannelId, anchor: u64, output: &Path) -> ExternalInputRelayLease {
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: output.to_str().unwrap().into(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, binding);
    let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
    lease.turn_id = Some(format!("external-{anchor}"));
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease("claude", tmux, lease)
}

/// The production TUI-direct synthetic claim (R9 entry).
async fn claim(
    case: (Arc<SharedData>, ChannelId, PathBuf),
    anchor: u64,
    tmux: &'static str,
) -> bool {
    let (shared, channel, output) = case;
    let lease = lease(tmux, channel, anchor, &output);
    let (provider, anchor) = (ProviderKind::Claude, MessageId::new(anchor));
    let claim = synthetic_start::claim_tui_direct_synthetic_turn;
    let claimed = claim(&shared, &provider, channel, tmux, "prompt", anchor, &lease);
    claimed.await.claimed
}

/// One channel of one isolated process.
struct Case {
    shared: Arc<SharedData>,
    channel: ChannelId,
    output: PathBuf,
}

impl Case {
    fn new(shared: &Arc<SharedData>, channel: u64, output: &Path) -> Self {
        let (shared, output) = (shared.clone(), output.to_path_buf());
        let channel = ChannelId::new(channel);
        Self {
            shared,
            channel,
            output,
        }
    }

    async fn start(&self, msg: u64, nonce: &str) -> bool {
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce.into())));
        let owner = UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID);
        let start = crate::services::discord::mailbox_try_start_turn;
        start(
            &self.shared,
            self.channel,
            token,
            owner,
            MessageId::new(msg),
        )
        .await
    }

    /// Exact-nonce release: raises the fence.
    async fn exact(&self, msg: u64, nonce: &str) -> bool {
        let (msg, nonce) = (MessageId::new(msg), Some(nonce.into()));
        let finish =
            crate::services::discord::mailbox_finish_turn_if_matches_episode_started_before;
        let finish = finish(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            msg,
            nonce,
            Instant::now(),
        );
        finish.await.removed_token.is_some()
    }

    /// Message-id release: leaves the fence and its latest started episode alone.
    async fn by_id(&self, msg: u64) -> bool {
        let finish = crate::services::discord::mailbox_finish_turn_if_matches;
        let finish = finish(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            MessageId::new(msg),
        );
        finish.await.removed_token.is_some()
    }

    /// Start `msg` and end it with an exact (fence-raising) or a message-id release.
    async fn turn(&self, msg: u64, nonce: &str, raise: bool) {
        assert!(self.start(msg, nonce).await);
        let released = match raise {
            true => self.exact(msg, nonce).await,
            false => self.by_id(msg).await,
        };
        assert!(released);
    }

    fn row(&self) -> Option<InflightTurnState> {
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel.get())
    }

    fn nonce(&self) -> Option<String> {
        self.row().and_then(|row| row.turn_nonce)
    }

    fn save(&self, row: &InflightTurnState) {
        inflight::save_inflight_state(row).expect("save row");
    }

    /// Row bytes and the slot.
    async fn observe(&self) -> (Option<String>, Slot) {
        let snapshot = mailbox_snapshot(&self.shared, self.channel).await;
        let row = self.row().map(|row| serde_json::to_string(&row).unwrap());
        let active = self.shared.restart.global_active.load(Ordering::Relaxed);
        let started = self
            .shared
            .turn_start_times
            .get(&self.channel)
            .map(|at| *at);
        let slot = (snapshot.active_user_message_id, snapshot.active_turn_nonce);
        (row, (slot.0, slot.1, active, started))
    }

    /// A TUI-direct row for `anchor` every refresh predicate accepts.
    fn synthetic_row(&self, anchor: u64, owner: RelayOwnerKind) -> InflightTurnState {
        let lease = lease(TMUX, self.channel, anchor, &self.output);
        let (anchor, output) = (MessageId::new(anchor), Some(self.output.as_path()));
        let build = synthetic_start::build_tui_direct_synthetic_inflight_state;
        let provider = ProviderKind::Claude;
        build(
            provider,
            self.channel,
            anchor,
            None,
            "prompt",
            TMUX,
            output,
            0,
            &lease,
            owner,
        )
    }

    /// A never-committed row with an undelivered tail, as the idle recovery finds it.
    fn dormant_row(&self, msg: u64, nonce: &str) -> InflightTurnState {
        let mut row = self.synthetic_row(msg, RelayOwnerKind::None);
        row.turn_start_offset = Some(0);
        row.turn_nonce = Some(nonce.to_string());
        row.full_response = "partial body".to_string();
        self.save(&row);
        self.row().expect("dormant row saved")
    }

    async fn capture(&self, row: &InflightTurnState) -> bool {
        let capture = crate::services::discord::tui_prompt_relay::capture_dormant_partial;
        capture(&self.shared, row, &self.output).await.is_some()
    }

    async fn claim(&self, anchor: u64, tmux: &'static str) -> bool {
        let case = (self.shared.clone(), self.channel, self.output.clone());
        claim(case, anchor, tmux).await
    }

    /// Construct episode E1 through the claim; no handle to its actor survives.
    async fn construct(&self, anchor: u64) -> String {
        assert!(self.row().is_none(), "construction precondition: no row");
        assert!(self.claim(anchor, TMUX).await);
        let nonce = self.nonce().expect("E1 nonce");
        let snapshot = mailbox_snapshot(&self.shared, self.channel).await;
        assert_eq!(snapshot.active_turn_nonce, Some(nonce.clone()));
        nonce
    }

    /// Park the claim at `pause`, run `between`, then let it finish.
    async fn claim_paused(&self, anchor: u64, pause: &Pause, between: impl FnOnce()) -> bool {
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        *pause.lock().unwrap() = Some((self.channel.get(), entered.clone(), resume.clone()));
        let case = (self.shared.clone(), self.channel, self.output.clone());
        let mut claiming = tokio::spawn(claim(case, anchor, TMUX));
        tokio::select! {
            _ = entered.notified() => {}
            _ = &mut claiming => {
                pause.lock().unwrap().take();
                panic!("the claim finished before reaching the pause");
            }
        }
        between();
        resume.notify_one();
        claiming.await.expect("claim task")
    }
}

// ---- R8: dormant resumption (`capture_dormant`) ----

/// T-R8a (P5) + T-R8c: neither the idle partial capture nor the unpublished resume
/// re-opens an exactly released episode.
#[test]
fn dormant_resumption_refuses_an_exactly_released_episode() {
    run(|output| async move {
        let shared = make_shared_data_for_tests();
        for (channel, unpublished) in [(5_951_801, false), (5_951_802, true)] {
            let (case, msg) = (Case::new(&shared, channel, &output), channel + 10);
            let mut row = case.dormant_row(msg, "r8a");
            if unpublished {
                row.full_response.clear();
                case.save(&row);
            }
            case.turn(msg, "r8a", true).await;
            let token = Arc::new(CancelToken::from_persisted_turn_nonce(
                row.turn_nonce.clone(),
            ));
            let owner = UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID);
            let control =
                crate::services::discord::queue_io::mailbox_try_start_turn_unless_released;
            let control = control(&shared, case.channel, token, owner, MessageId::new(msg)).await;
            assert!(!control.started && control.refused_released_episode);
            let before = case.observe().await;
            let resume = synthetic_start::bridge_handoff::resume_unpublished;
            let resumed = match unpublished {
                true => resume(&shared, &row, &output).await.is_some(),
                false => case.capture(&row).await,
            };
            assert!(
                !resumed,
                "R8 re-opened a released episode (unpublished={unpublished})"
            );
            assert_eq!(case.observe().await, before, "a refusal changes nothing");
        }
    });
}

/// T-R8b: without any exact release the dormant episode is resumed with its own nonce.
#[test]
fn dormant_partial_capture_resumes_when_nothing_was_released() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_803, &output);
        let row = case.dormant_row(5_951_813, "r8b");
        assert!(case.capture(&row).await);
        let snapshot = mailbox_snapshot(&case.shared, case.channel).await;
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(5_951_813))
        );
        assert_eq!(snapshot.active_turn_nonce.as_deref(), Some("r8b"));
        assert_eq!(snapshot.active_turn_kind, ActiveTurnKind::UserOrAgent);
    });
}

/// T-R8d: after another episode's exact release, X is resumable only while it is the
/// latest episode started; once a later one starts, X is refused.
#[test]
fn dormant_partial_capture_follows_the_latest_started_episode() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_804, &output);
        let (y, x, z) = (5_951_814, 5_951_824, 5_951_834);
        case.turn(y, "r8d-y", true).await;
        case.turn(x, "r8d-x", false).await;
        let row = case.dormant_row(x, "r8d-x");
        assert!(case.capture(&row).await, "X is the latest started episode");
        assert!(case.by_id(x).await);
        case.turn(z, "r8d-z", false).await;
        let before = case.observe().await;
        assert!(
            !case.capture(&row).await,
            "Z started after X; X is no longer latest"
        );
        assert_eq!(case.observe().await, before);
    });
}

/// T-R8b' (D1): the owner's own normal finalize is an exact release too, so the
/// undelivered tail it left is resumed only while the fence is not consulted.
#[test]
fn dormant_partial_capture_after_the_owners_normal_finalize() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_805, &output);
        let msg = 5_951_815;
        assert!(case.start(msg, "r8b2").await);
        let row = case.dormant_row(msg, "r8b2");
        let key = crate::services::discord::turn_finalizer::TurnKey::new(case.channel, msg, 0);
        let key = key.with_episode_nonce(Some("r8b2"));
        let finalize = crate::services::discord::turn_finalizer::claim_normal_episode;
        let finished = finalize(&case.shared, &ProviderKind::Claude, key, false, None).await;
        assert!(finished.is_ok_and(|captured| captured.is_some()));
        assert!(
            row.full_response.len() > row.response_sent_offset,
            "an undelivered tail remains"
        );
        assert!(
            case.capture(&row).await,
            "D1 characterization: main resumes"
        );
    });
}

/// T-R8f (D1, F2): X's dormant row makes another anchor's claim fail; that claim's own
/// rollback is an exact release.
#[test]
fn dormant_partial_capture_after_another_claims_rollback() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_806, &output);
        case.turn(5_951_816, "r8f-x", false).await;
        let row = case.dormant_row(5_951_816, "r8f-x");
        assert!(!case.claim(5_951_826, OTHER_TMUX).await);
        assert!(
            case.capture(&row).await,
            "D1 characterization: main resumes"
        );
    });
}

// ---- R9: TUI-direct admission over a matching row ----

/// T-R9d (P10) + T-R9a: the same anchor's claim over E1's row is a re-adoption and must
/// not re-open the exactly released episode, with a fresh or the retained token.
#[test]
fn claim_over_an_exactly_released_row_is_refused() {
    run(|output| async move {
        let shared = make_shared_data_for_tests();
        for (channel, retain) in [(5_951_807, false), (5_951_808, true)] {
            let (case, anchor) = (Case::new(&shared, channel, &output), channel + 10);
            let n1 = case.construct(anchor).await;
            let retained = mailbox_snapshot(&shared, case.channel).await.cancel_token;
            let retained = retained.filter(|_| retain);
            assert!(case.exact(anchor, &n1).await);
            let before = case.observe().await;
            let claimed = case.claim(anchor, TMUX).await;
            assert!(!claimed, "retained={retain}: re-opened a released episode");
            assert_eq!(
                case.observe().await,
                before,
                "row, slot and counters untouched"
            );
            drop(retained);
        }
    });
}

/// T-R9b': with no row for the anchor the claim is a construction, fence or not.
#[test]
fn claim_without_a_row_constructs_after_another_release() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_809, &output);
        case.turn(5_951_819, "r9b-b", true).await;
        assert!(case.row().is_none(), "precondition: no row for any anchor");
        assert!(case.claim(5_951_829, TMUX).await);
    });
}

/// T-R9e + T-R9c: the fence names the row's episode, not the token installed, and the
/// re-adopted turn keeps the background kind; a retained allocation is re-installed.
#[test]
fn claim_over_the_latest_started_row_readopts_it() {
    run(|output| async move {
        let shared = make_shared_data_for_tests();
        for (channel, retain) in [(5_951_810, false), (5_951_811, true)] {
            let (case, a) = (Case::new(&shared, channel, &output), channel + 20);
            case.turn(channel + 10, "r9e-y", true).await;
            let n1 = case.construct(a).await;
            let retained = mailbox_snapshot(&shared, case.channel).await.cancel_token;
            let retained = retained.filter(|_| retain);
            assert!(case.by_id(a).await);
            assert!(case.claim(a, TMUX).await, "retained={retain}");
            let snapshot = mailbox_snapshot(&shared, case.channel).await;
            let n2 = snapshot.active_turn_nonce.clone().expect("installed nonce");
            assert_eq!(
                n2 == n1,
                retain,
                "a fresh token is installed unless retained"
            );
            assert_eq!(case.nonce(), Some(n2));
            assert_eq!(snapshot.active_turn_kind, ActiveTurnKind::Background);
            drop(retained);
        }
    });
}

/// T-R9f: a construction that finds a matching row after admission leaves it alone
/// and releases its own lease, whoever owns the row's relay.
#[test]
fn construction_does_not_refresh_a_row_that_appeared_after_admission() {
    run(|output| async move {
        let shared = make_shared_data_for_tests();
        for (channel, owner) in [
            (5_951_840, RelayOwnerKind::None),
            (5_951_841, RelayOwnerKind::Watcher),
        ] {
            let (case, anchor) = (Case::new(&shared, channel, &output), channel + 10);
            let mut foreign = case.synthetic_row(anchor, owner);
            foreign.turn_nonce = Some("foreign-episode".to_string());
            let before = case.observe().await;
            let claimed = case
                .claim_paused(anchor, &ADMISSION_PAUSE, || case.save(&foreign))
                .await;
            assert!(!claimed, "{owner:?}: construction refreshed a foreign row");
            assert_eq!(case.nonce(), foreign.turn_nonce);
            assert_eq!(case.observe().await.1, before.1);
        }
    });
}

/// T-R9f': the episode re-check is added to, not substituted for, the row predicate.
#[test]
fn adoption_does_not_refresh_a_row_whose_external_turn_changed() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_842, &output);
        case.turn(5_951_852, "r9f2-y", true).await;
        case.construct(5_951_862).await;
        assert!(case.by_id(5_951_862).await);
        let mut changed = case.row().unwrap();
        changed.external_turn_id = Some("another-external-turn".to_string());
        let claimed = case
            .claim_paused(5_951_862, &ADMISSION_PAUSE, || case.save(&changed))
            .await;
        assert!(!claimed);
        let row = case.row().unwrap();
        assert_eq!(
            (row.turn_nonce, row.external_turn_id),
            (changed.turn_nonce, changed.external_turn_id)
        );
    });
}

/// T-R9i (D1, F2): another anchor's failed claim rolls back with an exact release,
/// before the same anchor claims its row again.
#[test]
fn claim_over_a_row_after_another_claims_rollback() {
    run(|output| async move {
        let case = Case::new(&make_shared_data_for_tests(), 5_951_844, &output);
        case.construct(5_951_854).await;
        assert!(case.by_id(5_951_854).await);
        assert!(!case.claim(5_951_864, OTHER_TMUX).await);
        assert!(
            case.claim(5_951_854, TMUX).await,
            "D1 characterization: main re-adopts"
        );
    });
}
