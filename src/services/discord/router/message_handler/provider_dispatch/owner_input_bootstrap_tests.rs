//! The retired Herdr canary over its real fixture: a verified tmux-era pin, no spawn marker, a live
//! blocked writer, a seeded O store and its owner's Retired row. Placement, the turn host and a
//! held front's watch judge it alike.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::record;
use crate::db::dispatched_sessions::hosted_execution::{HostedOwner, HostedState};
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::codex::CodexSourceMode;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::absent_launch_tests::auxiliary_tests;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::queue_io::input_owner::drive::Drive;
use crate::services::herdr_admission::{Admission, force_for_test as force_admission};
use crate::services::tmux_common as tc;
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};
use crate::services::tui_prompt_dedupe as dedupe;
use crate::services::turn_host::{HerdrRefusal, for_turn, force_codex_switch_for_test};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const HOST: &str = "bootstrap-host";
const NOT_READY: Result<bool, HerdrRefusal> = Err(HerdrRefusal::OWriterNotReady);

/// Runs `test` where every thread, the blocking ones placement judges on included, boots with the
/// canary's Herdr endpoint on this node.
fn on_herdr_canary<F: std::future::Future<Output = ()>>(test: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(|| {
            let forced =
                crate::config::session_hosts::force_for_test(Some(HOST), &[(CANARY_CHANNEL, HOST)]);
            // The override lives as long as the runtime's own thread does.
            std::mem::forget(forced);
        })
        .build()
        .unwrap()
        .block_on(test);
}

/// The shared record fixture, its launch evidence naming the owner's provider.
fn codex_record(
    owner: &HostedOwner,
    nonce: &str,
    state: HostedState,
) -> crate::db::dispatched_sessions::hosted_execution::HostedExecution {
    let mut recorded = record(owner, nonce, state);
    if let Some(expected) = recorded.expected.as_mut() {
        expected.binding_provider = owner.provider.clone();
    }
    recorded
}
const RETIRED: &str = "0123456789abcdef0123456789abcdef";

/// The canary as the live incident left it. Guards drop before the fixture that they override.
struct Canary {
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _switch: crate::services::turn_host::ForcedCodexSwitch,
    _admission: crate::services::herdr_admission::ForcedAdmission,
    _selected: test_override::ChannelsGuard,
    _env: [crate::config::TestEnvVarGuard; 2],
    writers: Vec<host::ManagedWriterHandle>,
    checkpoint: std::path::PathBuf,
    key: String,
    owner: HostedOwner,
    fixture: CanaryFixture,
}

impl Canary {
    async fn new() -> Self {
        let fixture =
            CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some(HOST)).await;
        let env = [
            fixture.scoped_test_env("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref()),
            fixture.scoped_test_env(
                test_override::CHANNELS_ENV,
                serde_json::to_string(&[(CANARY_CHANNEL, CodexTui)])
                    .unwrap()
                    .as_ref(),
            ),
        ];
        let selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
        auxiliary_tests::target_missing(&fixture, true);
        crate::services::platform::tmux::invalidate_availability_cache();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
            .unwrap()
            .unwrap();
        let era = store
            .begin_fixture_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    policy_parent: None,
                    channel,
                    sources: Vec::new(),
                    initial_anchor: 0,
                    build_digest: "herdr-bootstrap".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let io = TestHost::new([(CANARY_CHANNEL, fixture.old_source.clone())]);
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let readiness = host::process_readiness();
        let writers =
            host::start_managed(ShadowProvider::Codex, true, cutover::boot_ownership(), {
                let root = fixture.root.path().to_owned();
                move || HostParts {
                    io,
                    runtime_root: Some(root),
                    gate,
                    readiness,
                }
            });
        assert_eq!(writers.len(), 1, "the canary starts its real writer");
        tokio::time::timeout(Duration::from_secs(5), async {
            while host::process_readiness()
                .submission(CANARY_CHANNEL)
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the writer actor registers");
        // Seeded after the actor is live, so its boot reads the store as the incident's did.
        let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        channel.set_binding_checkpoint(1).unwrap();
        let checkpoint = fixture
            .root
            .path()
            .join(crate::services::tui_o::store::STORE_DIR_NAME)
            .join(CANARY_CHANNEL.to_string())
            .join(crate::services::tui_o::store::rotation::CHECKPOINT_FILE);
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &fixture.shared.token_hash,
            &ProviderKind::Codex,
            CANARY_TMUX,
        );
        let identity = crate::domain::session_identity::SessionIdentity::parse(&key).unwrap();
        let owner = HostedOwner {
            provider: "codex".into(),
            discord_token_hash: identity.token_hash.unwrap(),
            channel_id: CANARY_CHANNEL.to_string(),
            logical_key: CANARY_TMUX.into(),
            owner_node: HOST.into(),
            runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
        };
        let canary = Self {
            _hosts: crate::config::session_hosts::force_for_test(
                Some(HOST),
                &[(CANARY_CHANNEL, HOST)],
            ),
            _switch: force_codex_switch_for_test(Some(true)),
            _admission: force_admission(Admission::new(
                true,
                Some(fixture.root.path().join("no-admission-stop")),
            )),
            _selected: selected,
            _env: env,
            writers,
            checkpoint,
            key,
            owner,
            fixture,
        };
        canary
            .seed(&canary.owner, RETIRED, HostedState::Retired)
            .await;
        canary
    }

    fn pool(&self) -> &sqlx::PgPool {
        self.fixture.shared.pg_pool.as_ref().unwrap()
    }

    async fn seed(&self, owner: &HostedOwner, nonce: &str, state: HostedState) {
        let value = serde_json::to_value(codex_record(owner, nonce, state)).unwrap();
        sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE session_key = $2")
            .bind(value)
            .bind(&self.key)
            .execute(self.pool())
            .await
            .unwrap();
    }

    /// The turn host's verdict: `Ok(bootstrap)` for a Herdr plan, else its refusal.
    async fn judge(&self) -> Result<bool, HerdrRefusal> {
        match for_turn(
            Some(self.pool()),
            &ProviderKind::Codex,
            CANARY_CHANNEL,
            Some(&self.key),
        )
        .await
        {
            TurnHost::Herdr(plan) => Ok(plan.bootstrap),
            TurnHost::Refused(refusal) => Err(refusal),
            other => panic!("{other:?}"),
        }
    }

    async fn placement(&self) -> IntakeRoute {
        intake_route::for_input_placement("codex", CANARY_CHANNEL).await
    }

    async fn stop(mut self) {
        for writer in self.writers.drain(..) {
            writer.stop_and_join().await.unwrap();
        }
    }
}

// A retired canary whose old pin alone blocks its writer starts its first Herdr turn only while the
// old target is gone, the row is its owner's retired execution, unheld and not admitted; every
// other state keeps the existing o_writer_not_ready.
#[test]
fn retired_herdr_canary_bootstraps_only_on_an_exact_vacant_unheld_retired_row_pg() {
    on_herdr_canary(
        retired_herdr_canary_bootstraps_only_on_an_exact_vacant_unheld_retired_row_pg_case(),
    );
}

async fn retired_herdr_canary_bootstraps_only_on_an_exact_vacant_unheld_retired_row_pg_case() {
    let canary = Canary::new().await;
    assert!(!host::channel_accepts(CANARY_CHANNEL), "the pin blocks it");
    assert_eq!(canary.judge().await, Ok(true), "a bootstrap plan");

    canary.fixture.fake.mark_present();
    crate::services::platform::tmux::invalidate_availability_cache();
    assert_eq!(canary.judge().await, NOT_READY, "the old tmux runs");
    canary.fixture.fake.mark_missing();
    crate::services::platform::tmux::invalidate_availability_cache();
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::services::session_backend::insert_process_session(
        CANARY_TMUX,
        crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_845_052,
            alive,
        },
    );
    assert_eq!(canary.judge().await, NOT_READY, "the old process runs");
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    assert_eq!(canary.judge().await, Ok(true));

    let marker = tc::session_temp_path(CANARY_TMUX, "spawn_nonce");
    std::fs::write(&marker, "fedcba9876543210fedcba9876543210").unwrap();
    assert_eq!(canary.judge().await, NOT_READY, "a current spawn marker");
    std::fs::remove_file(&marker).unwrap();

    for state in [HostedState::Pending, HostedState::Bound] {
        canary.seed(&canary.owner, RETIRED, state).await;
        assert_eq!(canary.judge().await, NOT_READY, "{state:?}");
    }
    let pin = canary.fixture.context.execution_nonce.clone();
    canary.seed(&canary.owner, &pin, HostedState::Retired).await;
    assert_eq!(
        canary.judge().await,
        NOT_READY,
        "the pinned execution itself"
    );
    let fields: [fn(&mut HostedOwner) -> &mut String; 6] = [
        |owner| &mut owner.provider,
        |owner| &mut owner.discord_token_hash,
        |owner| &mut owner.channel_id,
        |owner| &mut owner.logical_key,
        |owner| &mut owner.owner_node,
        |owner| &mut owner.runtime_root,
    ];
    for (index, field) in fields.into_iter().enumerate() {
        let mut other = canary.owner.clone();
        field(&mut other).push_str("-other");
        canary.seed(&other, RETIRED, HostedState::Retired).await;
        // The row read itself refuses a record naming another provider, account or channel.
        assert!(canary.judge().await.is_err(), "owner field {index}");
    }
    canary
        .seed(&canary.owner, RETIRED, HostedState::Retired)
        .await;
    assert_eq!(canary.judge().await, Ok(true));

    crate::services::claude::herdr_turn::hold(RETIRED).unwrap();
    assert_eq!(canary.judge().await, NOT_READY, "the retired input hold");
    crate::services::claude::herdr_turn::unlink_hold(RETIRED).unwrap();

    dedupe::install_herdr_execution(CANARY_TMUX, "another-execution");
    assert_eq!(
        canary.judge().await,
        NOT_READY,
        "another execution is listed"
    );
    dedupe::install_herdr_execution(CANARY_TMUX, RETIRED);
    dedupe::admit_herdr_execution(CANARY_TMUX, RETIRED);
    assert_eq!(
        canary.judge().await,
        NOT_READY,
        "the retired execution is admitted"
    );
    // A failed marker write leaves the retired execution listed but withheld: still a bootstrap.
    dedupe::withhold_herdr_execution(CANARY_TMUX, Some(RETIRED));
    assert_eq!(canary.judge().await, Ok(true), "withheld listing");
    // The next launch's listing ends the retired execution's hooks.
    dedupe::install_herdr_execution(CANARY_TMUX, "fedcba9876543210fedcba9876543210");
    let mut late = canary.fixture.context.clone();
    late.execution_nonce = RETIRED.into();
    let envelope = dedupe::binding_context::HookBindingEnvelope {
        context: dedupe::binding_context::CapturedContext::Captured(late),
        observed: Default::default(),
    };
    let hook = dedupe::binding_events::HookSignal {
        event: "session-start".into(),
        source: Some("startup".into()),
        transcript_path: None,
        received_at: chrono::Utc::now(),
        published_at: None,
    };
    assert!(
        dedupe::observe_codex_herdr_hook("late-session", &hook, Some(&envelope)).is_none(),
        "the retired execution's late hook is refused"
    );
    dedupe::release_herdr_execution(CANARY_TMUX, "fedcba9876543210fedcba9876543210");
    canary.stop().await;
}

// Placement admits the retired canary's input only while neither the canonical nor the legacy
// rollout marker names an owner and both read; any other marker state keeps it held.
#[test]
fn herdr_canary_placement_needs_both_rollout_markers_unowned_and_readable_pg() {
    on_herdr_canary(
        herdr_canary_placement_needs_both_rollout_markers_unowned_and_readable_pg_case(),
    );
}

async fn herdr_canary_placement_needs_both_rollout_markers_unowned_and_readable_pg_case() {
    let canary = Canary::new().await;
    assert_eq!(canary.placement().await, IntakeRoute::Gateway);
    let ext = tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT;
    let legacy = std::path::PathBuf::from(tc::legacy_tmp_session_path(CANARY_TMUX, ext));
    let canonical = tc::persistent_sessions_dir()
        .unwrap()
        .join(legacy.file_name().unwrap());
    assert_ne!(canonical, legacy, "two distinct marker paths");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let owned = serde_json::json!({"codex_ownership": {
        "execution_nonce": RETIRED, "proof_seq": 1, "dev": 1, "ino": 1}})
    .to_string();
    let unowned = serde_json::json!({"rollout": "/tmp/rollout.jsonl"}).to_string();
    let cases: [(&str, Option<&str>, Option<&str>); 4] = [
        ("legacy owned", None, Some(owned.as_str())),
        (
            "canonical unowned, legacy owned",
            Some(unowned.as_str()),
            Some(owned.as_str()),
        ),
        ("legacy torn", None, Some("{torn")),
        ("canonical owned", Some(owned.as_str()), None),
    ];
    for (case, at_canonical, at_legacy) in cases {
        for (path, body) in [(&canonical, at_canonical), (&legacy, at_legacy)] {
            if let Some(body) = body {
                std::fs::write(path, body).unwrap();
            }
        }
        assert!(
            matches!(canary.placement().await, IntakeRoute::Hold(_)),
            "{case}"
        );
        assert_eq!(canary.judge().await, NOT_READY, "{case}");
        for path in [&canonical, &legacy] {
            let _ = std::fs::remove_file(path);
        }
    }
    std::fs::write(&canonical, &unowned).unwrap();
    std::fs::write(&legacy, &unowned).unwrap();
    assert_eq!(canary.placement().await, IntakeRoute::Gateway, "unowned");
    assert_eq!(canary.judge().await, Ok(true));
    canary.stop().await;
}

/// The owner's queue with one admitted original, held at its front as a refused start leaves it.
async fn held_front() -> (ChannelMailboxRegistry, ChannelMailboxHandle, u64) {
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(CANARY_CHANNEL));
    let message = CANARY_CHANNEL + 8181;
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":message,"channel_id":CANARY_CHANNEL,"source_message_ids":[message],
        "queued_generation":1,"text":"held for the bootstrap"}))
    .unwrap();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex/held-bootstrap");
    std::fs::create_dir_all(&dir).unwrap();
    mailbox
        .v2_adopt(
            dir.join(format!("{CANARY_CHANNEL}.json")),
            "input-owner".into(),
        )
        .await
        .unwrap();
    let admitted = admit(&mailbox, &original, AdmissionTicket::new()).await;
    assert!(matches!(
        admitted,
        crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
    ));
    mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            owner.hold_front(&front)
        })
        .await
        .unwrap();
    // What the drive reads after the refused handoff before it starts the watch.
    let held = mailbox.v2_request(move |slot| Ok(slot.as_mut().unwrap().take_held(message)));
    assert!(held.await.unwrap(), "the front is held");
    (registry, mailbox, message)
}

async fn read(probe: &mut Option<held_watch::ScreenProbe>) -> Screen {
    let mut taken = probe.take().unwrap();
    let (screen, taken) = tokio::task::spawn_blocking(move || (taken(), taken))
        .await
        .unwrap();
    *probe = Some(taken);
    screen
}

// A held canary front's watch reads what the turn host would do: ready for a bootstrap and for an
// accepting writer with a checkpoint, blocked while the checkpoint is torn. The blocked front is
// not driven again however many looks pass, and is driven once when the checkpoint returns.
#[test]
fn held_canary_front_is_driven_again_once_only_when_the_turn_host_would_plan_it_pg() {
    on_herdr_canary(
        held_canary_front_is_driven_again_once_only_when_the_turn_host_would_plan_it_pg_case(),
    );
}

async fn held_canary_front_is_driven_again_once_only_when_the_turn_host_would_plan_it_pg_case() {
    let canary = Canary::new().await;
    let shared = Some(&canary.fixture.shared);
    let mut probe = bootstrap_probe(shared, &ProviderKind::Codex, CANARY_CHANNEL).await;
    assert!(probe.is_some(), "the canary's held front is watched");
    assert_eq!(read(&mut probe).await, Screen::Ready);
    assert_eq!(canary.judge().await, Ok(true), "the same bootstrap plan");

    let _accepts = crate::services::herdr_launch::force_writer_accepts(Some(true));
    assert_eq!(read(&mut probe).await, Screen::Ready);
    assert_eq!(canary.judge().await, Ok(false), "the ordinary plan");

    let seeded = std::fs::read(&canary.checkpoint).unwrap();
    std::fs::write(&canary.checkpoint, b"{torn").unwrap();
    assert_eq!(read(&mut probe).await, Screen::Blocked);
    assert_eq!(canary.judge().await, NOT_READY);

    let (_registry, mailbox, message) = held_front().await;
    let driven = Arc::new(AtomicUsize::new(0));
    let count = driven.clone();
    let front = Arc::new(Drive {
        mailbox,
        channel: CANARY_CHANNEL,
        settled: None,
        handoff: Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("NotSent(\"probe_test\")".to_string()) })
        }),
    });
    held_watch::arm(CANARY_CHANNEL, probe.take().unwrap());
    held_watch::watch(front.clone(), message);
    // Three looks of the blocked front.
    tokio::time::sleep(Duration::from_millis(6_500)).await;
    assert_eq!(driven.load(Ordering::SeqCst), 0, "no drive while blocked");
    std::fs::write(&canary.checkpoint, seeded).unwrap();
    let woke = tokio::time::timeout(Duration::from_secs(6), async {
        while driven.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(woke.is_ok(), "the restored checkpoint wakes the front");
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    assert_eq!(driven.load(Ordering::SeqCst), 1, "driven once");
    drop(front);
    canary.stop().await;
}
