use super::*;
use crate::services::tui_o::writer::binding::ChannelBindingLog;

#[path = "fresh_deriver_compat_tests.rs"]
mod deriver_compat;
#[path = "fresh_gc_waiting_tests.rs"]
mod gc_waiting;
#[path = "fresh_scope_negative_tests.rs"]
mod negative;
#[path = "fresh_obligation_tests.rs"]
mod obligations;

pub(super) struct DedupeGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl DedupeGuard {
    pub(super) fn acquire() -> Self {
        Self {
            _guard: dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }
}

struct ScopeFixture {
    previous: crate::config::Config,
    _hosting: std::sync::MutexGuard<'static, ()>,
    _instance: crate::config::TestEnvVarGuard,
}

impl ScopeFixture {
    fn new() -> Self {
        let instance = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            std::ffi::OsStr::new("b1b-scoped-writer-test"),
        );
        let hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        let config =
            serde_yaml::from_str("server: {}\nproviders:\n  codex: {runtime: tui}\n").unwrap();
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        assert!(dedupe::binding_context::stable_host_identity().is_some());
        Self {
            previous,
            _hosting: hosting,
            _instance: instance,
        }
    }
}

impl Drop for ScopeFixture {
    fn drop(&mut self) {
        crate::services::provider_hosting::install_provider_hosting_config(&self.previous);
    }
}

pub(crate) fn proven(root: &Path) -> (BindingContext, SourceId) {
    let mut context = context(root, CANARY_TMUX, "verified");
    context.host = dedupe::binding_context::stable_host_identity();
    PreparedIncarnation::create(context.clone()).unwrap();
    publish_nonce(&context);
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    let id = uuid::Uuid::new_v4().to_string();
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{id}.jsonl"));
    let at = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let meta = serde_json::json!({"type":"session_meta", "timestamp":at,
        "payload":{"id":id,"timestamp":at,"cwd":root,"source":"cli","originator":"codex_cli_rs"}});
    std::fs::write(&path, format!("{meta}\n")).unwrap();
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let ingress =
        crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
            "codex",
            "session_start",
            Some(&id),
            Some(&id),
            &serde_json::json!({"session_id":id,"transcript_path":path,"source":"startup"}),
            &headers,
        );
    let fold = dedupe::binding_events::codex::read_ownership(&context).unwrap();
    let source = fold
        .verified
        .as_ref()
        .unwrap_or_else(|| panic!("proof: {ingress:?}, {fold:?}"))
        .source
        .clone();
    assert!(dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &source
    ));
    (context, source)
}

fn answer(source: &SourceId, id: &str) {
    for line in [
        output_line(
            "event_msg",
            serde_json::json!({"type":"task_started","turn_id":id}),
        ),
        output_line(
            "response_item",
            serde_json::json!({"type":"message","role":"assistant","id":id,
            "content":[{"type":"output_text","text":id}]}),
        ),
        output_line(
            "event_msg",
            serde_json::json!({"type":"task_complete","turn_id":id}),
        ),
    ] {
        append(&source.path, &line);
    }
}

async fn scenario(in_place: bool, retired_old: bool) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let n1 = retired_old.then(|| proven(root.path()).1);
    let (_, n2) = proven(root.path());
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        n1.iter()
            .chain(std::iter::once(&n2))
            .map(|source| InitSource {
                source_id: source.clone(),
                delivery_start: std::fs::metadata(&source.path).unwrap().len(),
                prefix_hash: hex::encode(Sha256::digest(std::fs::read(&source.path).unwrap())),
            })
            .collect()
    });
    if let Some(source) = &n1 {
        harness.channel().set_retired(source, true).unwrap();
    }
    let n1_cursor = n1
        .as_ref()
        .map(|source| harness.channel().cursor(source).unwrap().clone());
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let before = harness.channel().cursor(&n2).unwrap().clone();
    let bytes = std::fs::read(&n2.path).unwrap();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let make_n3 = || proven(root.path());
    let n3 = (!in_place).then(make_n3);
    let mut n2_io = (!in_place)
        .then(|| crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path));
    let actor = tokio::spawn(run_channel(
        harness.writer(),
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    polls(3).await;
    if in_place {
        assert!(*ready.borrow(), "N2 actor starts fully resumed");
    }
    let (_, n3) = n3.unwrap_or_else(make_n3);
    n2_io.get_or_insert_with(|| {
        crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path)
    });
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &n2
    ));
    answer(&n3, "Q1");
    polls(6).await;
    assert!(*ready.borrow(), "fresh proof restores the scoped writer");
    assert_eq!(
        harness.port.posts(),
        ["Q1", "-# ✅ 완료"],
        "O2/O2-W: a fresh source posts behind a denied old cursor"
    );
    let n3_before = harness.channel().cursor(&n3).unwrap().clone();
    let n3_bytes = std::fs::read(&n3.path).unwrap();
    let n3_rotation = harness.channel().rotation().unwrap().link(&n3).cloned();
    let n3_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n3.path);
    let (_, n4) = proven(root.path());
    answer(&n4, "Q2");
    polls(6).await;
    assert_eq!(
        harness.port.posts(),
        ["Q1", "-# ✅ 완료", "Q2", "-# ✅ 완료"],
        "O7-2X: one actor resumes the next exact incarnation"
    );
    assert_eq!(
        harness
            .alarms
            .raised
            .lock()
            .unwrap()
            .iter()
            .filter(|alarm| matches!(alarm,
                WriterAlarm::WaitingTooLong { incident, .. }
                    if incident.starts_with("new_root_waiting:")))
            .count(),
        0,
        "normal fresh proof must not raise a new-root waiting alarm"
    );
    assert_eq!(harness.channel().cursor(&n2).unwrap(), &before);
    if let Some(source) = &n1 {
        assert_eq!(harness.channel().cursor(source), n1_cursor.as_ref());
    }
    assert_eq!(harness.channel().cursor(&n3).unwrap(), &n3_before);
    assert_eq!(std::fs::read(&n2.path).unwrap(), bytes);
    assert_eq!(std::fs::read(&n3.path).unwrap(), n3_bytes);
    assert_eq!(
        harness.channel().rotation().unwrap().link(&n3),
        n3_rotation.as_ref()
    );
    assert_eq!(
        n2_io.unwrap().stats().read_bytes,
        0,
        "suspended N2 bytes were read"
    );
    assert_eq!(n3_io.stats().read_bytes, 0, "suspended N3 bytes were read");
    stop.send(true).unwrap();
    assert_eq!(actor.await.unwrap(), None);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn fresh_startup_proof_resumes_with_old_cursor_untouched() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::fresh_startup_proof_resumes_with_old_cursor_untouched"
    )) {
        scenario(false, false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn same_actor_scopes_two_fresh_incarnations_without_clear_or_restart() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::same_actor_scopes_two_fresh_incarnations_without_clear_or_restart"
    )) {
        scenario(true, false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn retired_and_unretired_denied_sources_stay_untouched_while_fresh_work_posts() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::retired_and_unretired_denied_sources_stay_untouched_while_fresh_work_posts"
    )) {
        scenario(false, true).await;
    }
}

#[tokio::test(start_paused = true)]
async fn first_fresh_proof_with_no_old_cursor_records_nonce_and_stops_ownership_scans() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::first_fresh_proof_with_no_old_cursor_records_nonce_and_stops_ownership_scans"
    )) {
        return;
    }
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (current, source) = proven(root.path());
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| Vec::new());
    harness.gate.acquired();
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let actor = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    answer(&source, "Q1");
    polls(6).await;
    assert!(*ready.borrow());
    assert_eq!(harness.port.posts(), ["Q1", "-# ✅ 완료"]);
    assert_eq!(
        selection.nonce().as_deref(),
        Some(current.execution_nonce.as_str()),
        "O1: a proven fresh root is recorded even without denied cursors"
    );
    assert!(selection.is_empty());
    assert!(dedupe::clear_relaunch::fresh_scope_ownership_calls_for_tests() > 0);
    polls(6).await;
    assert_eq!(
        dedupe::clear_relaunch::fresh_scope_ownership_calls_for_tests(),
        0,
        "O1: the same nonce cannot rescan ownership on each poll"
    );
    stop.send(true).unwrap();
    assert_eq!(actor.await.unwrap(), None);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn a_new_exact_root_cannot_rescope_an_actual_owed_piece_into_permission() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_new_exact_root_cannot_rescope_an_actual_owed_piece_into_permission"
    )) {
        return;
    }
    held_work(false, false, false).await;
}

#[tokio::test(start_paused = true)]
async fn retired_source_with_an_actual_owed_piece_cannot_enter_suspension() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::retired_source_with_an_actual_owed_piece_cannot_enter_suspension"
    )) {
        held_work(true, false, false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn unsealed_current_native_unit_holds_in_place_rescope_without_dropping_work() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::unsealed_current_native_unit_holds_in_place_rescope_without_dropping_work"
    )) {
        held_work(false, true, false).await;
    }
}

#[derive(Default)]
struct BootNotifications(Mutex<Vec<crate::services::tui_o::alarm::NotificationAttempt>>);

impl crate::services::tui_o::alarm::AlarmNotifier for BootNotifications {
    fn notify(
        &self,
        _alert_channel: u64,
        _alarm: crate::services::tui_o::alarm::AlarmNotification,
        attempt: crate::services::tui_o::alarm::NotificationAttempt,
    ) {
        // Keep the real router's notification slot pending so duplicate incidents stay suppressed.
        self.0.lock().unwrap().push(attempt);
    }
}

#[derive(Clone)]
struct BootHealthAlarms {
    recorded: Alarms,
    router: Arc<crate::services::tui_o::alarm::AlarmRouter>,
}

impl AlarmSink for BootHealthAlarms {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.router.raise(channel, alarm.clone());
        self.recorded.raise(channel, alarm);
    }

    fn waiting_cleared(&self, channel: u64) {
        self.router.waiting_cleared(channel);
        self.recorded.waiting_cleared(channel);
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.router.reconcile_reader_count(channel, count);
        self.recorded.reconcile_reader_count(channel, count);
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.router.resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.router.halt_cleared(channel);
    }
}

async fn held_work(retired: bool, unsealed: bool, restart_before_replay: bool) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, n2) = proven(root.path());
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: n2.clone(),
            delivery_start: std::fs::metadata(&n2.path).unwrap().len(),
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&n2.path).unwrap())),
        }]
    });
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let (n3_context, n3) = proven(root.path());
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let task = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    if unsealed {
        append(
            &n3.path,
            &output_line(
                "event_msg",
                serde_json::json!({"type":"task_started","turn_id":"unsealed"}),
            ),
        );
        append(
            &n3.path,
            &output_line(
                "event_msg",
                serde_json::json!({"type":"item_completed","item":{"type":"AgentMessage","id":"unsealed"}}),
            ),
        );
    } else {
        answer(&n3, "held N3 answer");
    }
    polls(6).await;
    assert!(*ready.borrow());
    assert!(
        if unsealed {
            crate::services::tui_o::writer::actor::work_for_tests().1
        } else {
            crate::services::tui_o::writer::actor::work_for_tests().0 > 0
        },
        "the real actor must retain captured N3 work before N4 exists"
    );
    assert!(
        harness.port.posts().is_empty(),
        "the unowned gateway holds the actual owed piece"
    );
    if retired {
        harness.channel().set_retired(&n3, true).unwrap();
    }
    let before = harness.channel().cursor(&n3).unwrap().clone();
    if restart_before_replay {
        assert_eq!(
            crate::services::tui_o::writer::actor::work_for_tests(),
            (0, true)
        );
        assert_eq!(
            before.captured_through,
            std::fs::metadata(&n3.path).unwrap().len()
        );
        assert!(harness.channel().ledger().unresolved().is_none());
        assert!(harness.channel().ledger().resume_pieces().next().is_none());
        assert_eq!(
            harness.channel().fresh_selection().unwrap().unwrap().nonce,
            n3_context.execution_nonce
        );
    }
    let boot_bytes = restart_before_replay.then(|| std::fs::read(&n3.path).unwrap());
    let io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n3.path);
    let (n4_context, n4) = proven(root.path());
    if unsealed {
        answer(&n4, "held N4 answer");
    }
    if restart_before_replay {
        assert!(unsealed && !retired);
        assert_ne!(n4_context.execution_nonce, n3_context.execution_nonce);
        assert!(dedupe::codex_verified_channel_delivery_allowed(
            CANARY_CHANNEL
        ));
        // No await follows N4 proof until the old actor is stopped, so it cannot rescope first.
        stop.send_replace(true);
        assert_eq!(task.await.unwrap(), None);
        harness.gate.acquired();
        let health = Arc::new(crate::services::tui_o::alarm::AlarmHealth::default());
        let notifications = Arc::new(BootNotifications::default());
        let router = Arc::new(crate::services::tui_o::alarm::AlarmRouter::new(
            Some(CANARY_CHANNEL + 1),
            Some(notifications.clone()),
            health.clone(),
        ));
        let cleared_before_boot = harness.alarms.cleared.load(Ordering::SeqCst);
        let writer = ChannelWriter::new(
            harness.channel(),
            harness.gate.clone(),
            harness.port.clone(),
            harness.lease.clone(),
            BootHealthAlarms {
                recorded: harness.alarms.clone(),
                router,
            },
        );
        let boot_selection = writer.suspension.clone();
        let (boot_stop, boot_stopped) = watch::channel(false);
        let (boot_resumed, _boot_ready) = watch::channel(false);
        let boot = tokio::spawn(run_channel(
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(
                CANARY_CHANNEL,
                ShadowProvider::Codex,
            )),
            boot_stopped,
            boot_resumed,
        ));
        polls(6).await;
        let posts = harness.port.posts();
        let alarms = harness.alarms.taken();
        let native_read_bytes = io.stats().read_bytes;
        assert_eq!(harness.channel().cursor(&n3), Some(&before));
        assert_eq!(
            std::fs::read(&n3.path).unwrap(),
            *boot_bytes.as_ref().unwrap()
        );
        assert_eq!(
            native_read_bytes, 0,
            "bootstrap must not read the denied N3 body"
        );
        assert!(
            boot_selection.contains(&n3),
            "cross-nonce bootstrap must preserve denied N3 by suspension"
        );
        assert_eq!(
            boot_selection.nonce().as_deref(),
            Some(n4_context.execution_nonce.as_str())
        );
        assert_eq!(
            posts
                .iter()
                .filter(|post| post.as_str() == "held N4 answer")
                .count(),
            1,
            "cross-nonce bootstrap must POST the fresh N4 answer exactly once: {posts:?}"
        );
        assert_eq!(
            alarms
                .iter()
                .filter(|alarm| matches!(alarm,
            WriterAlarm::WaitingTooLong { incident, .. }
                if incident.starts_with("cross_nonce_boot_rescope:")))
                .count(),
            1,
            "cross-nonce bootstrap rescope must raise one preservation warning: {alarms:?}"
        );
        assert_eq!(
            harness.alarms.cleared.load(Ordering::SeqCst) - cleared_before_boot,
            1,
            "the completed boot incident must publish exactly one waiting clear while S is nonempty"
        );
        let waiting_reason = format!("tui_o:waiting_too_long:{CANARY_CHANNEL}");
        assert!(
            !health
                .current_at(std::time::Instant::now())
                .contains(&waiting_reason),
            "actual AlarmHealth must clear the boot waiting reason after fresh work completes"
        );
        assert_eq!(notifications.0.lock().unwrap().len(), 1);

        harness.gate.lost();
        answer(&n4, "N4 ready after boot incident");
        polls(4).await;
        assert_eq!(harness.port.posts(), posts);
        assert!(crate::services::tui_o::writer::actor::work_for_tests().0 > 0);
        let notifications_before_ready = notifications.0.lock().unwrap().len();
        tokio::time::advance(std::time::Duration::from_millis(300_001)).await;
        boot_stop.send_replace(false);
        polls(2).await;
        let ready_alarms = harness.alarms.taken();
        assert_eq!(
            ready_alarms
                .iter()
                .filter(|alarm| matches!(alarm,
                WriterAlarm::WaitingTooLong { incident, ready, .. }
                    if incident.starts_with("ready:") && *ready > 0))
                .count(),
            1,
            "later actual owed work must raise one independent ready incident: {ready_alarms:?}"
        );
        assert!(
            health
                .current_at(std::time::Instant::now())
                .contains(&waiting_reason)
        );
        assert_eq!(
            notifications.0.lock().unwrap().len() - notifications_before_ready,
            1,
            "a later ready incident must notify once rather than reuse the boot incident slot"
        );
        assert_eq!(
            harness.alarms.cleared.load(Ordering::SeqCst) - cleared_before_boot,
            1,
            "the new overdue incident must stay active before its piece posts"
        );
        harness.gate.acquired();
        boot_stop.send_replace(false);
        polls(6).await;
        assert_eq!(
            harness
                .port
                .posts()
                .iter()
                .filter(|post| post.as_str() == "N4 ready after boot incident")
                .count(),
            1
        );
        assert_eq!(
            harness.alarms.cleared.load(Ordering::SeqCst) - cleared_before_boot,
            2,
            "the later ready incident must clear once after posting"
        );
        assert!(
            !health
                .current_at(std::time::Instant::now())
                .contains(&waiting_reason)
        );
        boot_stop.send_replace(true);
        assert_eq!(boot.await.unwrap(), None);
        let posts = harness.port.posts();
        eprintln!(
            "BUNDLE_BOOTSTRAP_CHARACTERIZATION old_announced=true old_owed=0 ledger_pending=0 current_scope_n4=1 old_suspended=1 fresh_body_posts=1 old_cursor_preserved=1 old_bytes_preserved=1 old_read_bytes={native_read_bytes} cross_nonce_warnings=1 waiting_cleared=1 later_ready_notifications=1"
        );

        // The installed N4 selection is an ordinary same-nonce restart, with no new warning.
        let writer = harness.writer();
        let restored_selection = writer.suspension.clone();
        let (same_stop, same_stopped) = watch::channel(false);
        let (same_resumed, same_ready) = watch::channel(false);
        let same = tokio::spawn(run_channel(
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(
                CANARY_CHANNEL,
                ShadowProvider::Codex,
            )),
            same_stopped,
            same_resumed,
        ));
        polls(6).await;
        let same_ready = *same_ready.borrow();
        let same_alarms = harness.alarms.taken();
        same_stop.send_replace(true);
        assert_eq!(same.await.unwrap(), None);
        assert!(
            same_ready,
            "an installed N4 selection must resume at the same nonce"
        );
        assert_eq!(
            restored_selection.nonce().as_deref(),
            Some(n4_context.execution_nonce.as_str())
        );
        assert!(restored_selection.contains(&n3));
        assert_eq!(
            harness.port.posts(),
            posts,
            "same-N4 restart must not duplicate the fresh answer"
        );
        assert_eq!(
            same_alarms
                .iter()
                .filter(|alarm| matches!(alarm,
            WriterAlarm::WaitingTooLong { incident, .. }
                if incident.starts_with("cross_nonce_boot_rescope:")))
                .count(),
            0,
            "same-N4 restart must not raise a cross-nonce preservation warning: {same_alarms:?}"
        );
        assert_eq!(harness.channel().cursor(&n3), Some(&before));
        assert_eq!(
            std::fs::read(&n3.path).unwrap(),
            *boot_bytes.as_ref().unwrap()
        );
        assert_eq!(
            io.stats().read_bytes,
            0,
            "same-N4 restart must not read the denied N3 body"
        );
        dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
        dedupe::binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
        return;
    }
    assert_ne!(n4_context.execution_nonce, n3_context.execution_nonce);
    assert!(
        dedupe::codex_verified_channel_delivery_allowed(CANARY_CHANNEL),
        "N4 must have real channel permission; a closed channel gate would hide the mutant"
    );
    harness.gate.acquired();
    polls(6).await;
    let work = crate::services::tui_o::writer::actor::work_for_tests();
    let posts = harness.port.posts();
    let alarms = harness.alarms.taken();
    stop.send(true).unwrap();
    assert_eq!(task.await.unwrap(), None);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
    assert!(
        posts.is_empty(),
        "N3 work gained POST permission by refreezing: {posts:?}"
    );
    assert!(
        if unsealed { work.1 } else { work.0 > 0 },
        "N3 work was discarded"
    );
    assert_eq!(
        selection.nonce().as_deref(),
        Some(n3_context.execution_nonce.as_str())
    );
    assert_eq!(harness.channel().cursor(&n3), Some(&before));
    assert_eq!(io.stats().read_bytes, 0);
    if unsealed {
        assert_eq!(
            alarms
                .iter()
                .filter(|alarm| matches!(alarm,
                WriterAlarm::WaitingTooLong { incident, .. }
                    if incident.starts_with("new_root_waiting:")))
                .count(),
            1,
            "O7-2X-K: a proven N4 blocked by announced N3 work raises one alarm: {alarms:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn new_root_waiting_alarm_clears_once_after_transition_job_finishes() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::new_root_waiting_alarm_clears_once_after_transition_job_finishes"
    )) {
        return;
    }
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, n2) = proven(root.path());
    let (n3_context, n3) = proven(root.path());
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        [&n2, &n3]
            .into_iter()
            .map(|source| InitSource {
                source_id: source.clone(),
                delivery_start: std::fs::metadata(&source.path).unwrap().len(),
                prefix_hash: hex::encode(Sha256::digest(std::fs::read(&source.path).unwrap())),
            })
            .collect()
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let old_cursor = harness.channel().cursor(&n2).unwrap().clone();
    let old_bytes = std::fs::read(&n2.path).unwrap();
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&n2.path);
    answer(&n3, "Q1 before transition");
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let alarm_count = || {
        harness
            .alarms
            .raised
            .lock()
            .unwrap()
            .iter()
            .filter(|alarm| {
                matches!(alarm,
                WriterAlarm::WaitingTooLong { incident, .. }
                    if incident.starts_with("new_root_waiting:"))
            })
            .count()
    };
    let n4_nonce = crate::services::tui_o::writer::actor::exercise_scope_waiting_job_for_tests(
        writer,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        CANARY_TMUX,
        &n3_context.execution_nonce,
        || {
            assert_eq!(harness.port.posts(), ["Q1 before transition", "-# ✅ 완료"]);
            assert!(selection.contains(&n2));
            assert_eq!(alarm_count(), 0);
            let (context, n4) = proven(root.path());
            answer(&n4, "Q2 after transition");
            context.execution_nonce
        },
        || {
            assert_eq!(alarm_count(), 1, "a held exact root raises one alarm");
            assert_eq!(
                harness.alarms.cleared.load(Ordering::SeqCst),
                0,
                "new_root_waiting must not clear while the transition job still exists"
            );
            assert_eq!(harness.port.posts(), ["Q1 before transition", "-# ✅ 완료"]);
            assert_eq!(
                selection.nonce().as_deref(),
                Some(n3_context.execution_nonce.as_str()),
                "a held root must not change the installed selection"
            );
        },
    )
    .await;
    assert_eq!(selection.nonce().as_deref(), Some(n4_nonce.as_str()));
    assert!(selection.contains(&n2));
    assert!(selection.contains(&n3));
    assert_eq!(
        harness.port.posts(),
        [
            "Q1 before transition",
            "-# ✅ 완료",
            "Q2 after transition",
            "-# ✅ 완료"
        ]
    );
    assert_eq!(alarm_count(), 1, "resolving the hold must not re-alarm");
    assert_eq!(
        harness.alarms.cleared.load(Ordering::SeqCst),
        1,
        "installing the new root must clear its own waiting incident exactly once"
    );
    assert_eq!(harness.channel().cursor(&n2), Some(&old_cursor));
    assert_eq!(std::fs::read(&n2.path).unwrap(), old_bytes);
    assert_eq!(old_io.stats().read_bytes, 0);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

async fn selection_install_fault(after_rename: bool) {
    use crate::services::tui_o::store::fault::{self, Step as FaultStep};
    const FRESH_SELECTION_FILE: &str = "fresh_selection";
    use crate::services::tui_o::writer::transition_cleanup::Request;
    use crate::services::tui_o::writer::transition_port::{
        self, Context, Operation, TransitionPort,
    };

    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, old) = proven(root.path());
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: old.clone(),
            delivery_start: std::fs::metadata(&old.path).unwrap().len(),
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&old.path).unwrap())),
        }]
    });
    harness.gate.acquired();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    harness
        .channel()
        .set_binding_checkpoint(checkpoint)
        .unwrap();
    let old_cursor = harness.channel().cursor(&old).unwrap().clone();
    let old_bytes = std::fs::read(&old.path).unwrap();
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
    let (current, source) = proven(root.path());
    answer(&source, "answer after durable selection");
    let writer = harness.writer();
    let selection = writer.suspension.clone();
    let dir = harness
        ._runtime
        .path()
        .join(crate::services::tui_o::store::STORE_DIR_NAME)
        .join(CANARY_CHANNEL.to_string());
    let selection_path = dir.join(FRESH_SELECTION_FILE);
    let fault = fault::plant(
        if after_rename { &selection_path } else { &dir },
        if after_rename {
            FaultStep::ReplaceDirSync
        } else {
            FaultStep::Write
        },
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let (transition, receiver) = TransitionPort::new(
        91,
        harness.gate.clone(),
        Arc::new(std::time::SystemTime::now),
        stopped.clone(),
    );
    let transition = transition.with_suspension(selection.clone());
    let actor = transition_port::spawning(receiver, || {
        spawn_if_enabled(
            &crate::services::tui_o::writer::WriterConfig { enabled: true },
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(
                CANARY_CHANNEL,
                ShadowProvider::Codex,
            )),
            stopped,
            resumed,
        )
        .unwrap()
    });
    polls(3).await;
    let halted = actor.is_finished();
    if !halted {
        stop.send_replace(true);
    }
    let outcome = actor.await.unwrap();
    drop(fault);
    assert!(
        halted,
        "selection persistence failure must halt the actual actor"
    );
    let cause = outcome.expect("selection persistence failure must retain a stop cause");
    assert!(
        matches!(cause.alarm, WriterAlarm::Halted { ref detail }
            if detail.starts_with("fresh selection write:")),
        "the selection write must be the halt cause: {cause:?}"
    );
    assert_eq!(cause.io, Some(std::io::ErrorKind::StorageFull));
    assert!(
        !*ready.borrow(),
        "an unpublished memory selection cannot publish ready"
    );
    assert!(
        selection.is_empty(),
        "a failed store write must not publish memory keys"
    );
    assert_eq!(
        selection.nonce(),
        None,
        "a failed store write must not publish the nonce"
    );
    assert!(
        harness.port.posts().is_empty(),
        "selection write failure must precede POST"
    );
    let request = Operation::Inspect(Request {
        channel: CANARY_CHANNEL,
        cutoff: checkpoint,
        job_id: "after-selection-write-failure".into(),
        operator: "fixture".into(),
        at: Utc::now(),
        reason: "a failed actor must not accept a strong consumer".into(),
        termination: None,
    });
    let context = Context {
        tmux: CANARY_TMUX.into(),
        nonce: current.execution_nonce.clone(),
        home_epoch: None,
        lock: Arc::default(),
        permit: None,
    };
    let refusal = transition.request(request, context).await;
    assert!(
        refusal.as_ref().is_err_and(|error| error == "actor ended"),
        "the failed actor's shared-selection port must be closed: {refusal:?}"
    );
    if after_rename {
        let published = std::fs::read(&selection_path).unwrap();
        let record: serde_json::Value = serde_json::from_slice(&published).unwrap();
        assert_eq!(record["schema"], 1);
        assert_eq!(record["channel"], CANARY_CHANNEL);
        assert_eq!(record["nonce"], current.execution_nonce);
        assert_eq!(
            record["keys"],
            serde_json::json!([crate::services::tui_o::store::spool::source_key(&old)])
        );
        // Restoration must use the whole record; a fresh re-install trips this exact file's sync.
        let rewrite = fault::plant(
            &selection_path,
            FaultStep::ReplaceDirSync,
            std::io::ErrorKind::StorageFull,
            None,
        );
        let writer = harness.writer();
        let restored_selection = writer.suspension.clone();
        assert_eq!(restored_selection.nonce(), None);
        let (restart_stop, restart_stopped) = watch::channel(false);
        let (restart_resumed, restart_ready) = watch::channel(false);
        let restarted = tokio::spawn(run_channel(
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(
                CANARY_CHANNEL,
                ShadowProvider::Codex,
            )),
            restart_stopped,
            restart_resumed,
        ));
        polls(6).await;
        let ready_now = *restart_ready.borrow();
        restart_stop.send_replace(true);
        let outcome = restarted.await.unwrap();
        drop(rewrite);
        assert!(
            ready_now,
            "a renamed whole selection must restore before same-nonce resume"
        );
        assert_eq!(
            outcome, None,
            "restoring a selection must not retry its failed write"
        );
        assert_eq!(
            restored_selection.nonce().as_deref(),
            Some(current.execution_nonce.as_str())
        );
        assert!(restored_selection.contains(&old));
        assert_eq!(
            harness.port.posts(),
            ["answer after durable selection", "-# ✅ 완료"]
        );
        assert_eq!(std::fs::read(&selection_path).unwrap(), published);
    } else {
        assert!(
            !selection_path.exists(),
            "a failed temp write must leave no selection to restore"
        );
        assert_eq!(
            std::fs::read(&selection_path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
    assert_eq!(harness.channel().cursor(&old), Some(&old_cursor));
    assert_eq!(std::fs::read(&old.path).unwrap(), old_bytes);
    assert_eq!(old_io.stats().read_bytes, 0);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn selection_write_failure_halts_before_publishing_scope_or_ready() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::selection_write_failure_halts_before_publishing_scope_or_ready"
    )) {
        selection_install_fault(false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn selection_rename_sync_failure_restores_whole_record_before_fresh_post() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::selection_rename_sync_failure_restores_whole_record_before_fresh_post"
    )) {
        selection_install_fault(true).await;
    }
}

#[tokio::test(start_paused = true)]
async fn bootstrap_cross_nonce_preserves_announced_source_and_warns_once() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::bootstrap_cross_nonce_preserves_announced_source_and_warns_once"
    )) {
        held_work(false, true, true).await;
    }
}
