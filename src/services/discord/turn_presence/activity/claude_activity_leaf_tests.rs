use super::*;
use crate::services::claude_tui::activity::{self as collector, Foreground};
use axum::http::HeaderMap;

impl Rig {
    fn raw(
        &self,
        ctx: &context::BindingContext,
        kind: &str,
        id: &str,
        child: bool,
        continuing: bool,
    ) {
        let mut headers = HeaderMap::new();
        let envelope = context::HookBindingEnvelope {
            context: context::CapturedContext::Captured(ctx.clone()),
            observed: context::ObservedHookProcess::default(),
        };
        headers.insert(
            context::BINDING_HEADER,
            envelope.encode().unwrap().parse().unwrap(),
        );
        headers.insert(
            "x-agentdesk-relay-published-at",
            Utc::now().to_rfc3339().parse().unwrap(),
        );
        let mut payload = json!({"session_id":self.ctx.expected_native_session_id,"transcript_path":self.path,
            "tool_use_id":id,"tool_name":"Bash"});
        if child {
            payload["agent_id"] = json!("child-a");
        }
        collector::note_hook(
            true,
            &headers,
            "claude",
            &hook_server::HookEventKind::from_path(kind),
            &payload,
            continuing,
        );
    }
    fn rotate_epoch(&mut self) {
        self.ctx.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        self.ctx.created_at = Utc::now();
        context::PreparedIncarnation::create(self.ctx.clone()).unwrap();
        fs::write(
            crate::services::tmux_common::session_temp_path(&self.session, "spawn_nonce"),
            &self.ctx.execution_nonce,
        )
        .unwrap();
        let channel = self.ctx.channel_id.unwrap();
        let log = self
            ._dir
            .path()
            .join(events::BINDING_EVENTS_DIR)
            .join(format!("{channel}.log"));
        fs::remove_file(log).unwrap();
        events::forget_channel_for_tests(channel);
        let pin = crate::services::tui_o::shadow::binding_reader::source_id_for(
            self.ctx.expected_native_session_id.as_deref().unwrap(),
            &self.path,
        )
        .unwrap();
        events::record_verified(
            &events::Proposal {
                channel_id: channel,
                provider: "claude",
                tmux_session: &self.session,
                session_id: self.ctx.expected_native_session_id.as_deref(),
                path: self.path.to_str().unwrap(),
                replaced: None,
                cause: events::CauseSource::Observed,
                hook: None,
            },
            &pin,
        )
        .unwrap();
    }
}

#[test]
fn post_before_root_turn_replay_keeps_a_tombstone_for_late_pre() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    let submitted = Utc::now();
    fs::write(
        &r.path,
        format!(
            "{}\n",
            json!({"type":"mode","sessionId":r.ctx.expected_native_session_id})
        ),
    )
    .unwrap();
    r.raw(&r.ctx, "PostToolUse", "tool-a", false, false);
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    for row in [
        json!({"type":"user","uuid":"delayed-root","timestamp":submitted,"sessionId":r.ctx.expected_native_session_id,"message":{"content":"work"}}),
        json!({"type":"assistant","timestamp":submitted,"sessionId":r.ctx.expected_native_session_id,"message":{"content":[{"type":"tool_use","id":"tool-a","name":"Bash","input":{}}]}}),
    ] {
        writeln!(
            fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            "{row}"
        )
        .unwrap();
    }
    r.raw(&r.ctx, "PreToolUse", "tool-a", false, false);
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown,
        "a Post observed before replay must close the exact later root invocation"
    );
}

#[tokio::test]
async fn post_first_and_child_completion_cannot_reopen_or_close_root_tool() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.hook("PostToolUse").await;
    r.hook("PreToolUse").await;
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    r.append(json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-b","name":"Bash","input":{}}]}}));
    r.raw(&r.ctx, "PreToolUse", "tool-b", false, false);
    let open = collector::observe(&r.session);
    assert_eq!(open.foreground, Foreground::Tool);
    r.raw(&r.ctx, "PostToolUse", "tool-b", true, false);
    r.raw(&r.ctx, "PostToolUse", "different-id", false, false);
    assert_eq!(collector::observe(&r.session).foreground, Foreground::Tool);
    r.raw(&r.ctx, "PostToolUse", "tool-b", false, false);
    assert!(
        !open.current(),
        "matching Post supersedes an older snapshot"
    );
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
}

#[tokio::test]
async fn same_id_other_epoch_cannot_lend_a_pre_to_current_source() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let mut r = Rig::new();
    r.hook("PreToolUse").await;
    let old_snapshot = collector::observe(&r.session);
    assert_eq!(old_snapshot.foreground, Foreground::Tool);
    let old_context = r.ctx.clone();
    r.rotate_epoch();
    assert!(!old_snapshot.current());
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    r.raw(&old_context, "PreToolUse", "tool-a", false, false);
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown,
        "old execution published after the new visit"
    );
    r.raw(&r.ctx, "PreToolUse", "tool-a", false, false);
    assert_eq!(collector::observe(&r.session).foreground, Foreground::Tool);
}

#[tokio::test]
async fn absent_unreadable_and_replaced_sources_fail_closed_then_read_again() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.hook("PreToolUse").await;
    let marker = crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce");
    fs::remove_file(&marker).unwrap();
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    fs::write(&marker, "").unwrap();
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    fs::write(&marker, &r.ctx.execution_nonce).unwrap();
    assert_eq!(collector::observe(&r.session).foreground, Foreground::Tool);
    let bytes = fs::read(&r.path).unwrap();
    fs::rename(&r.path, r.path.with_extension("old")).unwrap();
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    fs::write(&r.path, bytes).unwrap();
    r.raw(&r.ctx, "PreToolUse", "tool-a", false, false);
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown,
        "replacement inode is not the pinned source"
    );
}

#[tokio::test]
async fn unchanged_frame_and_unreadable_tail_recover_via_the_scheduled_callback() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.hook("PreToolUse").await;
    let old = collector::observe(&r.session);
    let before = fs::metadata(&r.path).unwrap().modified().unwrap();
    write!(
        fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
        "{{\"type\":\"system\",\"subtype\":\"turn_duration\""
    )
    .unwrap();
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    writeln!(
        fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
        "}}"
    )
    .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&r.path)
        .unwrap()
        .set_modified(before)
        .unwrap();
    let revision = collector::revision_for_tests(&r.session).unwrap();
    collector::poll_for_tests();
    assert!(
        collector::revision_for_tests(&r.session).unwrap() > revision,
        "the scheduled callback must install the terminal before the next foreground read"
    );
    assert!(!old.current());
    assert_eq!(collector::observe(&r.session).foreground, Foreground::Idle);
}

#[tokio::test]
async fn local_stop_continuation_is_not_an_idle_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.append(json!({"type":"system","subtype":"turn_duration"}));
    let done = collector::observe(&r.session);
    assert_eq!(done.foreground, Foreground::Idle);
    r.raw(&r.ctx, "Stop", "tool-a", false, true);
    assert!(!done.current());
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    r.append(
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"continuing"}]}}),
    );
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Generating
    );
}

struct Immediate(LivePorts);
impl Ports for Immediate {
    fn binding_seq(&self, channel: u64) -> Result<u64, String> {
        self.0.binding_seq(channel)
    }
    fn binding_events(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<Vec<BindingEvent>, String> {
        self.0.binding_events(channel, provider)
    }
    fn session_present(&self, session: &str) -> bool {
        self.0.session_present(session)
    }
    fn final_ready(&self, session: &str) -> bool {
        self.0.final_ready(session)
    }
    fn claude_busy(&self, session: &str) -> claude_activity::Busy {
        self.0.claude_busy(session)
    }
    fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
        SessionLiveness::Alive
    }
    fn spawn(&self, job: Job) -> bool {
        job();
        true
    }
}
#[tokio::test]
async fn stale_six_hundred_second_presence_consumes_tools_and_closes_without_frame_changes() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    let mut rows: Vec<Value> = fs::read_to_string(&r.path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    rows[1]["uuid"] = json!("assistant-tool-a");
    rows[1]["apiBlockIndex"] = json!(0);
    rows[1]["message"]["id"] = json!("message-tool-a");
    let mut transcript = fs::File::create(&r.path).unwrap();
    for row in rows {
        writeln!(transcript, "{row}").unwrap();
    }
    drop(transcript);
    let static_frame = "✻ Working… (1s · esc to interrupt)\n";
    let tmux = ScriptedTmux::new(static_frame);
    let watch = Arc::new(Mutex::new(Watch::default()));
    let ports: Arc<dyn Ports> = Arc::new(Immediate(LivePorts {
        root: events::test_root(),
    }));
    let ask = || {
        judge_presence(
            &watch,
            &ports,
            ShadowProvider::Claude,
            r.ctx.channel_id.unwrap(),
            Target::Bound(r.session.clone()),
        )
        .0
    };
    assert_eq!(ask().activity, Activity::Unknown);
    assert_eq!(ask(), observed(Activity::Busy, "open"));
    lock(&watch).grew_at = Instant::now() - STALE_OPEN_AFTER - Duration::from_secs(1);
    assert_eq!(
        ask().activity,
        Activity::Unknown,
        "static busy chrome alone cannot supplement a stale turn"
    );
    r.hook("PreToolUse").await;
    assert_eq!(
        ask().activity,
        Activity::Busy,
        "real production leaf consumes the accepted pair"
    );
    let snapshot = collector::observe(&r.session);
    for width in [46, 18, 80, 46] {
        tmux.show(&format!(
            "{}\n❯\n{}\nmode\ncontext\nfooter\n",
            "─".repeat(width),
            "─".repeat(width)
        ));
        assert_eq!(collector::observe(&r.session), snapshot);
        assert_eq!(ask().activity, Activity::Busy);
    }
    r.append(json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a","is_error":true}]}}));
    r.append(json!({"type":"system","subtype":"turn_duration"}));
    tmux.show(static_frame);
    assert_eq!(ask().activity, Activity::Idle);
    assert!(tmux.sends().is_empty());
    assert!(
        tmux.pane_calls(&r.session).is_empty(),
        "activity/presence did not capture or send keys"
    );
}

#[path = "claude_activity_p2_tests.rs"]
mod p2;
