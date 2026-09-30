use super::*;
use crate::services::claude_tui::hook_server::observation_ingress::tests::*;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*, binding_events::*};
use std::cell::RefCell;

struct View {
    tmux: String,
    channel: u64,
    home: PathBuf,
    // Other live panes; the pass's dead-orphan sweep would otherwise evict their mappings.
    peers: Vec<String>,
}
thread_local! { static VIEW: RefCell<Option<View>> = const { RefCell::new(None) }; }
/// Read by every thread when set, so a pass the relay runs on its blocking pool sees it too.
static SHARED_VIEW: std::sync::Mutex<Option<View>> = std::sync::Mutex::new(None);
/// Each thread that read `SHARED_VIEW`.
static SHARED_READERS: std::sync::Mutex<Vec<std::thread::ThreadId>> =
    std::sync::Mutex::new(Vec::new());

/// This thread's view, else the shared one.
fn view<T>(read: impl Fn(&View) -> T) -> Option<T> {
    if let Some(seen) = VIEW.with_borrow(|v| v.as_ref().map(&read)) {
        return Some(seen);
    }
    let shared = SHARED_VIEW.lock().unwrap_or_else(|e| e.into_inner());
    let seen = shared.as_ref().map(&read)?;
    let mut readers = SHARED_READERS.lock().unwrap_or_else(|e| e.into_inner());
    readers.push(std::thread::current().id());
    Some(seen)
}
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        VIEW.with_borrow_mut(|v| *v = None);
        dedupe::pane_registration::BLOCK_ALIAS.set(false);
        dedupe::pane_registration::BEFORE_COMPLETE.with_borrow_mut(|v| *v = None);
    }
}

pub(super) fn claude_session_names() -> Result<Vec<String>, String> {
    if let Some(names) = view(|v| std::iter::once(&v.tmux).chain(&v.peers).cloned().collect()) {
        return Ok(names);
    }
    crate::services::platform::tmux::list_session_names()
}
pub(super) fn claude_pane_live(tmux: &str) -> bool {
    if let Some(live) = view(|v| v.tmux == tmux || v.peers.iter().any(|p| p == tmux)) {
        return live;
    }
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux)
}
pub(super) fn claude_channel(tmux: &str) -> Option<u64> {
    if let Some(channel) = view(|v| (v.tmux == tmux).then_some(v.channel)) {
        return channel;
    }
    resolve_rehydrated_claude_tmux_channel_id(tmux)
}
pub(super) fn claude_home() -> Option<PathBuf> {
    view(|v| v.home.clone())
}

fn outer_failure(alias: bool, header: bool) {
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let (tmux, channel) = (format!("ingress-pass-{}", uuid()), 7_490);
    let h = uuid();
    let a = if alias { uuid() } else { h.clone() };
    let b = uuid();
    let home = root.path().join("claude-home");
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let a_path =
        crate::services::claude_tui::transcript_tail::claude_transcript_path(&cwd, &a, Some(&home))
            .unwrap();
    std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
    std::fs::write(&a_path, "{}\n").unwrap();
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.clone(),
        channel_id: Some(channel),
        owner_runtime_root: root.path().display().to_string(),
        host: None,
        expected_native_session_id: Some(h.clone()),
        launch_mode: "fresh".into(),
        provider_root: Some(home.clone()),
    };
    let prepared = PreparedIncarnation::create(context).unwrap();
    use crate::services::tmux_common as tc;
    let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
    std::fs::create_dir_all(Path::new(&script).parent().unwrap()).unwrap();
    std::fs::write(
        &script,
        format!(
            "{}cd '{}'\nexec 'claude' '--session-id' '{}'\n",
            prepared.env_lines(),
            cwd.display(),
            h
        ),
    )
    .unwrap();
    std::fs::write(tc::session_temp_path(&tmux, tc::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT),
        serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"command":format!("adk hook --session-id {h}")}]}]}}).to_string()).unwrap();
    std::fs::write(
        tc::session_temp_path(&tmux, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    // Reproduce a real H→A adoption and artifact cutover before losing dcserver state.
    let h_path = a_path.parent().unwrap().join(format!("{h}.jsonl"));
    std::fs::write(&h_path, "{}\n").unwrap();
    assert!(dedupe::register_rehydrated_tmux_runtime_binding(
        "claude",
        &tmux,
        channel,
        claude(&h_path, &h)
    ));
    if alias {
        let payload = serde_json::json!({"session_id":a,"source":"clear"});
        assert_eq!(
            ingress.claude_hook("SessionStart", &h, &payload, Some(&uuid())),
            202
        );
        assert!(
            crate::services::claude_tui::session::persist_claude_continuation_session(&tmux, &a)
                .is_ok()
        );
    }
    dedupe::reset_state_for_tests();
    dedupe::clear_claude_session_rotation(&tmux);
    forget_channel_for_tests(channel);
    let log_before = events(channel).len();
    forget_channel_for_tests(channel);
    VIEW.with_borrow_mut(|v| {
        *v = Some(View {
            tmux: tmux.clone(),
            channel,
            home,
            peers: Vec::new(),
        })
    });
    let _reset = Reset;
    let shared = crate::services::discord::make_shared_data_for_tests();
    ingress.seed_feedback(&h);
    let before_h = buffered(&h);
    let before_a = buffered(&a);
    let mut rx = ingress.state.subscribe();
    APPEND_FAULT.with(|f| f.set(Some("reload")));
    rehydrate_existing_claude_tui_bindings(&shared);
    assert!(
        dedupe::runtime_binding_for_tmux_session(&tmux).is_none(),
        "outer pass must fail registration"
    );
    let request = uuid();
    let encoded = HookBindingEnvelope {
        context: CapturedContext::Captured(prepared.context.clone()),
        observed: ObservedHookProcess::default(),
    }
    .encode()
    .unwrap();
    let envelope = header.then_some(encoded.as_str());
    let uri = format!("/hooks/claude/SessionStart?session_id={h}");
    let payload = serde_json::json!({"session_id":b,"source":"clear","transcript_path":a_path.parent().unwrap().join(format!("{b}.jsonl"))});
    let status = ingress
        .send_envelope(&uri, &payload, Some(&request), envelope)
        .0;
    assert_eq!(status, 425, "F4-alias status == 425");
    let mut captured = HookBindingEnvelope {
        context: CapturedContext::Captured(prepared.context.clone()),
        observed: ObservedHookProcess::default(),
    };
    assert!(
        dedupe::pane_registration::pane_registration_failed(&uuid(), Some(&captured)),
        "header finds failed pane without a command alias"
    );
    if let CapturedContext::Captured(ctx) = &mut captured.context {
        ctx.execution_nonce = uuid();
    }
    assert!(
        !dedupe::pane_registration::pane_registration_failed(&h, Some(&captured)),
        "another incarnation must not match by command alias"
    );
    if let CapturedContext::Captured(ctx) = &mut captured.context {
        ctx.execution_nonce = prepared.context.execution_nonce.clone();
        ctx.tmux_session = "another-pane".into();
    }
    assert!(
        !dedupe::pane_registration::pane_registration_failed(&h, Some(&captured)),
        "another pane must not match by nonce or command alias"
    );
    assert_eq!(
        ingress.claude_hook("SessionStart", &a, &payload, Some(&uuid())),
        425,
        "launch command also stays refused"
    );
    assert_eq!(
        (
            buffered(&h) - before_h,
            buffered(&a) - before_a,
            buffered(&b),
            drain(&mut rx)
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(ingress.pending_feedback(&h), 1);
    APPEND_FAULT.with(|f| f.set(None));
    assert_eq!(events(channel).len(), log_before);
    if alias {
        dedupe::pane_registration::BLOCK_ALIAS.set(true);
        rehydrate_existing_claude_tui_bindings(&shared);
        assert!(
            dedupe::runtime_binding_for_tmux_session(&tmux).is_some(),
            "binding registered before alias readiness"
        );
        let blocked = ingress
            .send_envelope(&uri, &payload, Some(&request), envelope)
            .0;
        assert_eq!(blocked, 425, "alias not ready status == 425");
        assert_eq!(pending_lines(channel, &b), 0);
        assert_eq!(
            (buffered(&h) - before_h, buffered(&b), drain(&mut rx)),
            (0, 0, 0)
        );
        assert_eq!(ingress.pending_feedback(&h), 1);
        dedupe::pane_registration::BLOCK_ALIAS.set(false);
    }
    rehydrate_existing_claude_tui_bindings(&shared);
    assert!(dedupe::runtime_binding_for_tmux_session(&tmux).is_some());
    assert_eq!(
        dedupe::provider_session_for_tmux("claude", &tmux).as_deref(),
        Some(h.as_str()),
        "cached command remains the hook wait key"
    );
    let status = ingress
        .send_envelope(&uri, &payload, Some(&request), envelope)
        .0;
    assert_ne!(status, 409, "same pin never conflicts (409)");
    assert_eq!(status, 202, "F4 recovered status == 202");
    assert_eq!(pending_lines(channel, &b), 1, "F4 Pending B exactly one");
    assert_eq!(
        (buffered(&h) - before_h, buffered(&b), drain(&mut rx)),
        (1, 0, 1)
    );
    let logged = events(channel).len();
    let cached = ingress
        .send_envelope(&uri, &payload, Some(&request), envelope)
        .0;
    assert_ne!(cached, 409, "same pin never conflicts (409)");
    assert_eq!(cached, 202);
    assert_eq!(events(channel).len(), logged);
    assert_eq!(
        (buffered(&h) - before_h, buffered(&b), drain(&mut rx)),
        (1, 0, 0)
    );
}

#[test]
fn f4_alias_outer_pass_refuses_until_binding_and_alias_are_ready() {
    outer_failure(true, false);
}
#[test]
fn f4_launch_outer_pass_refuses_until_binding_and_alias_are_ready() {
    outer_failure(false, false);
}

#[test]
fn f4_alias_binding_envelope_uses_the_failed_incarnation() {
    outer_failure(true, true);
}

struct RegistrationRace {
    tmux: String,
    channel: u64,
    h: String,
    a: String,
    b: String,
    context: BindingContext,
    payload: serde_json::Value,
}
impl RegistrationRace {
    fn new(root: &Path, channel: u64) -> Self {
        let (tmux, h, a, b) = (
            format!("registration-race-{}", uuid()),
            uuid(),
            uuid(),
            uuid(),
        );
        let home = root.join("claude-home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let a_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            &cwd,
            &a,
            Some(&home),
        )
        .unwrap();
        std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
        let b_path = a_path.parent().unwrap().join(format!("{b}.jsonl"));
        std::fs::write(&a_path, "{}\n").unwrap();
        std::fs::write(&b_path, "{}\n").unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: None,
            expected_native_session_id: Some(h.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(home.clone()),
        };
        let prepared = PreparedIncarnation::create(context.clone()).unwrap();
        use crate::services::tmux_common as tc;
        let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
        std::fs::create_dir_all(Path::new(&script).parent().unwrap()).unwrap();
        std::fs::write(
            &script,
            format!(
                "{}cd '{}'\nexec 'claude' '--session-id' '{}'\n",
                prepared.env_lines(),
                cwd.display(),
                a
            ),
        )
        .unwrap();
        std::fs::write(
            tc::session_temp_path(&tmux, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        VIEW.with_borrow_mut(|v| {
            *v = Some(View {
                tmux: tmux.clone(),
                channel,
                home,
                peers: Vec::new(),
            })
        });
        Self {
            tmux,
            channel,
            h,
            a,
            b: b.clone(),
            context,
            payload: serde_json::json!({"session_id":b,"source":"clear","transcript_path":b_path}),
        }
    }
    fn envelope(&self) -> HookBindingEnvelope {
        HookBindingEnvelope {
            context: CapturedContext::Captured(self.context.clone()),
            observed: ObservedHookProcess::default(),
        }
    }
}

#[test]
fn registration_completion_accepts_adoption_advanced_binding() {
    use crate::services::claude_tui::hook_server::adoption_retry::{
        AdoptionHttp, DurableKind, adopt_from_hook,
    };
    use dedupe::pane_registration as pr;
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let _reset = Reset;
    let race = RegistrationRace::new(root.path(), 7_491);
    let shared = crate::services::discord::make_shared_data_for_tests();
    // The in-flight hook has passed the production failure lookup before registration starts.
    assert!(!pr::pane_registration_failed(&race.a, None));
    let (tmux, a, b, payload, channel) = (
        race.tmux.clone(),
        race.a.clone(),
        race.b.clone(),
        race.payload.clone(),
        race.channel,
    );
    pr::BEFORE_COMPLETE.with_borrow_mut(|v| {
        *v = Some(Box::new(move || {
            assert!(pr::pane_registration_failed(&a, None));
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&tmux)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some(a.as_str())
            );
            assert_eq!(
                events(channel).len(),
                1,
                "registration logged A before completion"
            );
            assert_eq!(
                adopt_from_hook(&a, &b, &HookSignal::from_payload("session_start", &payload)),
                AdoptionHttp::Durable(DurableKind::Adopted)
            );
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&tmux)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some(b.as_str())
            );
            assert_eq!(events(channel).len(), 2, "normal adoption durably logged B");
        }))
    });
    let mut rx = ingress.state.subscribe();
    rehydrate_existing_claude_tui_bindings(&shared);
    assert!(
        pr::BEFORE_COMPLETE.with_borrow(|v| v.is_none()),
        "actual outer pass reached completion seam"
    );
    assert_eq!(events(race.channel).len(), 2);
    assert_eq!(
        (
            buffered(&race.h),
            buffered(&race.a),
            buffered(&race.b),
            drain(&mut rx)
        ),
        (0, 0, 0, 0)
    );
    for (index, command) in [&race.a, &race.h].into_iter().enumerate() {
        let id = uuid();
        let status = ingress.claude_hook("Stop", command, &race.payload, Some(&id));
        assert_eq!(status, 202, "advanced binding Stop status == 202");
        assert_eq!(buffered(command), 1);
        assert_eq!(buffered(&race.b), index + 1);
        assert_eq!(drain(&mut rx), 2);
        assert_eq!(
            events(race.channel).len(),
            2,
            "Stop adds no duplicate adoption"
        );
        assert_eq!(
            ingress.claude_hook("Stop", command, &race.payload, Some(&id)),
            202
        );
        assert_eq!(
            (buffered(command), buffered(&race.b), drain(&mut rx)),
            (1, index + 1, 0)
        );
    }
    rehydrate_existing_claude_tui_bindings(&shared);
    assert_eq!(
        ingress.claude_hook("Stop", &race.h, &race.payload, Some(&uuid())),
        202
    );
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(&race.tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(race.b.as_str())
    );
    assert_eq!(events(race.channel).len(), 2);
    assert_eq!(
        (buffered(&race.h), buffered(&race.b), drain(&mut rx)),
        (2, 3, 2)
    );
}

fn registration_nonce_replacement(independent_failure: bool) {
    use dedupe::pane_registration as pr;
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let _reset = Reset;
    let race = RegistrationRace::new(root.path(), 7_492);
    let mut next = race.context.clone();
    next.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    next.expected_native_session_id = Some(uuid());
    let next_context = next.clone();
    let n2 = HookBindingEnvelope {
        context: CapturedContext::Captured(next.clone()),
        observed: ObservedHookProcess::default(),
    };
    let (tmux, a) = (race.tmux.clone(), race.a.clone());
    pr::BEFORE_COMPLETE.with_borrow_mut(|v| {
        *v = Some(Box::new(move || {
            assert!(pr::pane_registration_failed(&a, None));
            PreparedIncarnation::create(next.clone()).unwrap();
            std::fs::write(
                crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce"),
                next.execution_nonce,
            )
            .unwrap();
            if independent_failure {
                APPEND_FAULT.with(|f| f.set(Some("reload")));
                forget_channel_for_tests(7_492);
                let binding = dedupe::runtime_binding_for_tmux_session(&tmux).unwrap();
                pr::register_claude_pane(&tmux, 7_492, binding);
                APPEND_FAULT.with(|f| f.set(None));
                assert!(pr::pane_registration_failed(&a, Some(&n2)));
            }
        }))
    });
    let shared = crate::services::discord::make_shared_data_for_tests();
    rehydrate_existing_claude_tui_bindings(&shared);
    assert!(pr::BEFORE_COMPLETE.with_borrow(|v| v.is_none()));
    let assert_retired = || {
        assert!(
            !pr::pane_registration_failed(&race.h, Some(&race.envelope())),
            "N1 failure retired after nonce replacement"
        );
        assert!(
            !pr::pane_registration_failed(&race.h, None),
            "N1 legacy alias no longer blocked"
        );
    };
    let mut rx = ingress.state.subscribe();
    if independent_failure {
        let ctx = next_context;
        let command = ctx.expected_native_session_id.clone().unwrap();
        let encoded = HookBindingEnvelope {
            context: CapturedContext::Captured(ctx),
            observed: ObservedHookProcess::default(),
        }
        .encode()
        .unwrap();
        assert_eq!(
            ingress
                .send_envelope(
                    &format!("/hooks/claude/Stop?session_id={command}"),
                    &race.payload,
                    Some(&uuid()),
                    Some(&encoded)
                )
                .0,
            425,
            "N2 independent failure status == 425"
        );
        assert_eq!((buffered(&command), drain(&mut rx)), (0, 0));
        assert_eq!(events(race.channel).len(), 1);
        assert_retired();
    } else {
        assert_retired();
        assert_eq!(
            ingress.claude_hook("Stop", &race.h, &race.payload, Some(&uuid())),
            202,
            "retired legacy alias status == 202"
        );
        assert_eq!((buffered(&race.h), drain(&mut rx)), (1, 1));
        rehydrate_existing_claude_tui_bindings(&shared);
        assert!(!pr::pane_registration_failed(&race.h, None));
    }
}

#[test]
fn registration_nonce_replacement_retires_old_failure() {
    registration_nonce_replacement(false);
}

#[test]
fn registration_nonce_replacement_preserves_new_failure() {
    registration_nonce_replacement(true);
}

#[test]
fn registration_alias_conflict_keeps_original_pane_unready() {
    use dedupe::pane_registration as pr;
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let _reset = Reset;
    let race = RegistrationRace::new(root.path(), 7_493);
    // Coarse filesystem clocks give A and B one mtime; pin it so every platform takes that path.
    let b_path = PathBuf::from(race.payload["transcript_path"].as_str().unwrap());
    let a_path = b_path.with_file_name(format!("{}.jsonl", race.a));
    let a_mtime = std::fs::metadata(a_path).unwrap().modified().unwrap();
    let b_file = std::fs::File::options().write(true).open(&b_path).unwrap();
    b_file.set_modified(a_mtime).unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    pr::BLOCK_ALIAS.set(true);
    rehydrate_existing_claude_tui_bindings(&shared);
    let other = format!("reused-command-{}", uuid());
    ingress.pane(&other, 7_494, &race.h);
    let set_peers =
        |peers: Vec<String>| VIEW.with_borrow_mut(|v| v.as_mut().unwrap().peers = peers);
    set_peers(vec![other.clone()]);
    pr::BLOCK_ALIAS.set(false);
    rehydrate_existing_claude_tui_bindings(&shared);
    let encoded = race.envelope().encode().unwrap();
    let uri = format!("/hooks/claude/Stop?session_id={}", race.a);
    let id = uuid();
    let mut rx = ingress.state.subscribe();
    assert_eq!(
        ingress
            .send_envelope(&uri, &race.payload, Some(&id), Some(&encoded))
            .0,
        425,
        "alias conflict status == 425"
    );
    assert_eq!(
        (
            buffered(&race.a),
            drain(&mut rx),
            events(race.channel).len()
        ),
        (0, 0, 1)
    );
    assert_eq!(
        ingress.claude_hook("Stop", &race.h, &race.payload, Some(&uuid())),
        202,
        "reassigned legacy alias status == 202"
    );
    assert_eq!((buffered(&race.h), drain(&mut rx)), (1, 1));
    assert_eq!(
        dedupe::resolve_tmux_session_name("claude", &race.h).as_deref(),
        Some(other.as_str())
    );
    set_peers(Vec::new());
    dedupe::register_provider_session("claude", &race.h, &race.tmux);
    rehydrate_existing_claude_tui_bindings(&shared);
    assert_eq!(
        ingress
            .send_envelope(&uri, &race.payload, Some(&id), Some(&encoded))
            .0,
        202
    );
    assert_eq!(
        (
            buffered(&race.a),
            drain(&mut rx),
            events(race.channel).len()
        ),
        (1, 2, 2)
    );
    assert_eq!(
        ingress
            .send_envelope(&uri, &race.payload, Some(&id), Some(&encoded))
            .0,
        202
    );
    assert_eq!(
        (
            buffered(&race.a),
            drain(&mut rx),
            events(race.channel).len()
        ),
        (1, 0, 2)
    );
}

const NEIGHBOUR_CHILD: &str = "ADK_T3BB_NEIGHBOUR_CHILD";

/// Runs `name` alone in a child process with its own runtime root; true inside that child. A
/// child still running after a minute fails the test.
fn in_child(name: &str) -> bool {
    if std::env::var_os(NEIGHBOUR_CHILD).is_some() {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture"])
        .env(NEIGHBOUR_CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("TMPDIR", root.path())
        .env_remove(crate::services::tui_o::cutover::test_override::CHILD_ENV)
        .env_remove("DATABASE_URL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("{name} did not finish: the neighbour's rehydration waited on the adoption");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

/// Legacy has started; the stalled pin never reads anything else.
struct Started;

impl crate::services::tui_o::writer::adoption::LegacyView for Started {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, _: &str) -> crate::services::tui_o::writer::adoption::LegacyCursor {
        crate::services::tui_o::writer::adoption::LegacyCursor::Unbound
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        None
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn a_stalled_adoption_does_not_delay_a_neighbours_first_rehydration() {
    if !in_child("a_stalled_adoption_does_not_delay_a_neighbours_first_rehydration") {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::tmux_common as tc;
    use crate::services::tui_o::shadow::ShadowProvider;
    use crate::services::tui_o::writer::activation::test_hook;
    use crate::services::tui_o::writer::host::{self, HostParts, Readiness, test_io::TestHost};
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    // B: a live Legacy pane this process has never bound, named only by its launch script.
    let (tmux, neighbour, session) = (format!("t3bb-{}", uuid()), 7_493, uuid());
    let (home, cwd) = (root.join("claude-home"), root.join("project"));
    std::fs::create_dir_all(&cwd).unwrap();
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &cwd,
        &session,
        Some(&home),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"type\":\"summary\"}\n").unwrap();
    let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
    std::fs::create_dir_all(Path::new(&script).parent().unwrap()).unwrap();
    let launch = format!(
        "cd '{}'\nexec 'claude' '--session-id' '{session}'\n",
        cwd.display()
    );
    std::fs::write(&script, launch).unwrap();
    *SHARED_VIEW.lock().unwrap() = Some(View {
        tmux: tmux.clone(),
        channel: neighbour,
        home,
        peers: Vec::new(),
    });
    // A: a candidate that holds output, its pin stalled until the test ends.
    let candidate = 7_494;
    let a_path = root.join("candidate.jsonl");
    std::fs::write(&a_path, "{\"type\":\"result\"}\n").unwrap();
    let source = crate::services::tui_o::shadow::binding_reader::source_id_for("t3bb", &a_path);
    let io = TestHost::new([(candidate, source.unwrap())]);
    *io.legacy.lock().unwrap() = Some(Arc::new(Started));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    test_hook::set(candidate, test_hook::Step::Snapshot, move || {
        entered_tx.send(()).unwrap();
        let _ = released.recv();
        Err("the test ended".into())
    });
    let _candidates = crate::services::tui_o::cutover::test_override::force_candidates(&[(
        candidate,
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
    gate.acquired();
    let parts = || HostParts {
        io,
        runtime_root: Some(root.clone()),
        gate,
        readiness: Arc::new(Readiness::default()),
    };
    // The host starts first, so an adoption blocking the runtime would stall the relay too.
    let hosts = host::start(ShadowProvider::Claude, true, parts);
    let shared = crate::services::discord::make_shared_data_for_tests();
    spawn_claude_idle_transcript_relay(shared);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let registered = loop {
        let bound = dedupe::runtime_binding_for_tmux_session(&tmux);
        if let Some(bound) = bound {
            break bound;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B was never rehydrated"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok(),
        "A's pin started"
    );
    let adoption = crate::services::tui_o::cutover::test_override::with_channels(|boot| {
        boot.unwrap().candidate(candidate).unwrap().peek()
    });
    assert_eq!(
        adoption,
        crate::services::tui_o::channel_policy::Adoption::Pending,
        "A's pin is still stalled"
    );
    let len = std::fs::metadata(&transcript).unwrap().len();
    assert_eq!(
        (registered.output_path.as_str(), registered.last_offset),
        (transcript.to_str().unwrap(), len),
        "B's first cursor is its transcript's length, as without the candidate"
    );
    let me = std::thread::current().id();
    let readers = SHARED_READERS.lock().unwrap().clone();
    assert!(
        readers.iter().any(|reader| *reader != me),
        "the relay's own rehydrate thread read the fixture: {readers:?}"
    );
    release.send(()).unwrap();
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[path = "../../discord/tui_prompt_relay/rehydration_pending_tests.rs"]
mod pending;
