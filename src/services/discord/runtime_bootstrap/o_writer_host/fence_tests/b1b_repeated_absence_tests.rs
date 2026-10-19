//! A second real absent-target launch keeps the same O actor and suspends both predecessors.

use super::*;
use crate::services::codex::actual_submit_tests::source_io::Watch;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::rotation::Successor;
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_o::store::{ChannelStore, OEra};
use dedupe::binding_context::{BindingContext, SpawnNonceMarker};

const THIRD_PROMPT: &str = "B1b third instruction after the fresh incarnation disappeared";
const THIRD_REPLY: &str = "B1b second fresh incarnation O reply";
const THIRD_NATIVE_ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c799";

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_ordinary_full_resume_recovers_absence_in_place_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_ordinary_full_resume_recovers_absence_in_place_pg"
    )) {
        scenario(true, BeforeInput::InPlaceAbsent).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_ordinary_full_resume_clear_recovers_in_place_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_ordinary_full_resume_clear_recovers_in_place_pg"
    )) {
        scenario(true, BeforeInput::InPlaceClear).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_unproved_successor_holds_until_clear_then_fresh_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_unproved_successor_holds_until_clear_then_fresh_fifo_pg"
    )) {
        scenario(true, BeforeInput::UnprovedThenClear).await;
    }
}

pub(super) async fn unproved_successor_holds_actual_input(
    fixture: &CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    output: &Discord,
    readiness: &Arc<host::Readiness>,
) {
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    assert!(readiness.is_hosted(CANARY_CHANNEL));
    assert!(!readiness.is_ready(CANARY_CHANNEL));
    assert!(!readiness.accepts(CANARY_CHANNEL));
    let mut context = fixture.context.clone();
    context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    context.created_at = chrono::Utc::now() + chrono::Duration::seconds(2);
    let prepared = PreparedIncarnation::create(context).unwrap();
    crate::services::discord::stamp_spawn_markers(CANARY_TMUX, Some(&prepared)).unwrap();
    assert!(dedupe::clear_relaunch::successor_unproved_for_channel(
        CANARY_CHANNEL
    ));
    assert_eq!(
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::SessionPresence::Missing,
    );
    assert!(
        dedupe::binding_events::codex::read_ownership(&prepared.context)
            .unwrap()
            .verified
            .is_none()
    );
    fixture.fake.take_calls();
    crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    let deps = IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "e51-isolated-test-bot",
    };
    let admission = admit_text_intake(&deps, &input()).await;
    let decision = format!("{admission:?}");
    assert!(
        decision.starts_with("Blocked {") && decision.contains("successor_unproved"),
        "actual D intake must hold the unproved successor: {decision}",
    );
    assert!(b1::queue_identities(fixture).await.is_empty());
    assert!(output.texts().is_empty());
    assert!(
        crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX)
            .is_empty()
    );
    assert!(!fixture.fake.take_calls().iter().any(|call| {
        ["new-session", "kill-session", "send-keys", "paste-buffer"]
            .iter()
            .any(|effect| call.starts_with(effect))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_same_actor_repeats_absence_fresh_provider_and_post_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_same_actor_repeats_absence_fresh_provider_and_post_pg"
    )) {
        scenario(true, BeforeInput::RepeatAbsent).await;
    }
}

pub(super) struct PreservedSource {
    source: SourceId,
    cursor: Cursor,
    link: SourceLink,
    successor: Option<Successor>,
    spool: BTreeMap<PathBuf, Vec<u8>>,
    bytes: Vec<u8>,
    channel_dir: PathBuf,
    watch: Watch,
}

impl PreservedSource {
    pub(super) fn assert_preserved(&self, store: &ChannelStore) {
        let key = source_key(&self.source);
        assert_eq!(store.cursor(&self.source), Some(&self.cursor));
        assert!(!self.cursor.retired);
        assert_eq!(store.rotation().unwrap().links.get(&key), Some(&self.link));
        assert_eq!(
            store.rotation().unwrap().successors.get(&key),
            self.successor.as_ref()
        );
        assert_eq!(source_spool(&self.channel_dir, &key), self.spool);
        assert_eq!(fs::read(&self.source.path).unwrap(), self.bytes);
        assert_eq!(
            self.watch.stats(),
            Default::default(),
            "N3 source read/advance must remain zero after N4 launch publication, including shutdown"
        );
    }
}

pub(super) async fn repeat_absence(
    fixture: &mut CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    output: &Discord,
    readiness: &Arc<host::Readiness>,
    store: &OStore,
    era: &OEra,
    n3: &BindingContext,
) -> PreservedSource {
    b1::wait_recovery_custody_settled(fixture, n3, false).await;
    fixture
        .shared
        .tmux_watchers
        .remove(&serenity::ChannelId::new(CANARY_CHANNEL));
    let source = dedupe::binding_events::codex::read_ownership(n3)
        .unwrap()
        .verified
        .unwrap()
        .source;
    let bytes = fs::read(&source.path).unwrap();
    let key = source_key(&source);
    let channel_dir = fixture
        .root
        .path()
        .join("o_store")
        .join(CANARY_CHANNEL.to_string());
    eventually(
        || {
            let current = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
            current.ledger().unresolved().is_none()
                && current.cursor(&source).is_some_and(|cursor| {
                    cursor.captured_through == bytes.len() as u64 && !cursor.retired
                })
        },
        "settled N3 O cursor before the second absence",
    )
    .await;
    let current = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
    let cursor = current.cursor(&source).unwrap().clone();
    let rotation = current.rotation().unwrap();
    let link = rotation.links.get(&key).unwrap().clone();
    let successor = rotation.successors.get(&key).cloned();
    let spool = source_spool(&channel_dir, &key);
    drop(current);
    assert!(readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL));
    b1::target_missing(fixture, false);
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(n3.execution_nonce.clone())
    );
    let cli = fixture.root.path().join("codex-fixture");
    let script = fs::read_to_string(&cli).unwrap();
    assert!(script.contains(NEW_NATIVE_ID));
    fs::write(cli, script.replace(NEW_NATIVE_ID, THIRD_NATIVE_ID)).unwrap();
    let identities = b1::restore_instructions(
        fixture,
        vec![b1::instruction(CANARY_CHANNEL + 103, THIRD_PROMPT)],
    )
    .await;
    let (observed_tx, observed_rx) = mpsc::channel();
    let _observe = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            crate::services::codex::actual_submit_tests::observe_absence_admission(Box::new(
                move |observation, token| {
                    let token = token.expect("actual Q3 provider token");
                    let exact_self = observation.is_some_and(|observed| {
                        observed.matches_token(token)
                            && observed.recheck(CANARY_CHANNEL, CANARY_TMUX)
                    });
                    let row =
                        crate::services::discord::inflight::load_inflight_state_read_only_result(
                            &ProviderKind::Codex,
                            CANARY_CHANNEL,
                        )
                        .unwrap()
                        .unwrap();
                    assert_eq!(row.provider, "codex");
                    assert_eq!(row.channel_id, CANARY_CHANNEL);
                    assert_eq!(row.user_msg_id, CANARY_CHANNEL + 103);
                    assert_eq!(row.request_owner_user_id, 1);
                    assert_eq!(row.turn_nonce.as_deref(), token.turn_nonce());
                    assert_eq!(row.tmux_session_name.as_deref(), Some(CANARY_TMUX));
                    observed_tx.send((Arc::clone(token), exact_self)).unwrap();
                },
            ));
            Box::new(())
        }),
    );
    fixture.fake.take_calls();
    assert!(
        b1::kickoff(fixture, api).await,
        "second absent-target Q3 must dispatch"
    );
    let third = fixture.turn_failure_guard().await;
    let third_token = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
        .unwrap();
    let (actual_token, exact_self) =
        receive(&observed_rx, "second actual provider ExactSelf row").await;
    assert!(
        exact_self,
        "the repeated actual provider must receive ExactSelf"
    );
    assert!(Arc::ptr_eq(&actual_token, &third_token));
    let n4 = b1::wait_context_after(fixture, &n3.execution_nonce).await;
    assert_ne!(n4.execution_nonce, n3.execution_nonce);
    assert_ne!(n4.execution_nonce, fixture.context.execution_nonce);
    assert_eq!(n4.launch_mode, "fresh");
    assert_eq!(n4.source_policy.as_deref(), Some("verified"));
    assert!(n4.expected_native_session_id.is_none());
    assert_eq!(b1::queue_identities(fixture).await, identities[1..]);
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(
        argv.iter().filter(|arg| arg.contains(THIRD_PROMPT)).count(),
        1
    );
    assert!(
        argv.iter()
            .all(|arg| arg != "resume" && arg != NEW_NATIVE_ID)
    );
    // N3 was still current until the new nonce existed; measure predecessor IO after that boundary.
    let watch = Watch::start(&source.path);
    let path = b1::publish_named_source(fixture, &n4, THIRD_NATIVE_ID);
    let new_io = Watch::start(&path);
    native_output::terminal(&path, "new-clear-turn", THIRD_REPLY);
    b1::wait_completion(&third_token).await;
    b1::wait_idle(fixture).await;
    eventually(
        || output.texts().iter().any(|text| text.contains(THIRD_REPLY)),
        "same O actor second fresh source HTTP POST",
    )
    .await;
    third.finish();
    b1::wait_recovery_custody_settled(fixture, &n4, false).await;
    eventually(
        || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
        "same actor readiness after second fresh proof",
    )
    .await;
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.contains(THIRD_REPLY))
            .count(),
        1
    );
    assert!(
        new_io.stats().read_bytes > 0,
        "the N4 actual consumer must exercise the read oracle"
    );
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
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
    assert!(b1::queue_identities(fixture).await.is_empty());
    PreservedSource {
        source,
        cursor,
        link,
        successor,
        spool,
        bytes,
        channel_dir,
        watch,
    }
}
