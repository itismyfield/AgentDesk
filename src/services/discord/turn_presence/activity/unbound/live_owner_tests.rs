//! A session-less turn-mode channel on a host that also runs unrelated tmux sessions.
use super::review_tests::Rig;
use super::*;
use crate::services::discord::health::{
    HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,
    external_turn_hold_for_start, start_without_gateway,
};
use crate::services::platform::tmux::session_inventory::{EmptyReason, Inventory};
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};
use poise::serenity_prelude::{MessageId, UserId};
use std::path::PathBuf;

const MANUAL: &str = "claude";
const ROUTINE: &str = "AgentDesk-claude-routine-brief---counsel";
const STRAY: &str = "AgentDesk-claude-stray";
const THREAD: u64 = 69_183_950;

/// Real tmux sessions on the rig's private socket, which is never the host's own server.
struct HostSessions(PathBuf);
impl HostSessions {
    async fn new(rig: &Rig) -> Self {
        let watch = Arc::new(Mutex::new(Watch::default()));
        let (_, stamp) = read(&rig.shared, &ProviderKind::Claude, rig.channel, watch).await;
        let socket = match &stamp.namespace.as_ref().unwrap().inventory {
            Inventory::Empty(EmptyReason::MissingSocket(path)) => path.clone(),
            other => panic!("fresh private namespace expected: {other:?}"),
        };
        assert!(socket.starts_with(rig.root.path()), "{socket:?}");
        let dir = socket.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(socket)
    }
    fn tmux(&self, args: &[&str]) {
        let output = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .arg("-S")
            .arg(&self.0)
            .args(["-u", "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
    fn start(&self, name: &str) {
        self.tmux(&["new-session", "-d", "-s", name, "sleep 600"]);
    }
}
impl Drop for HostSessions {
    fn drop(&mut self) {
        let _ = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .arg("-S")
            .arg(&self.0)
            .arg("kill-server")
            .output();
    }
}

/// A finished fresh routine run that recorded `name` and ran its turn in `channel`.
async fn routine_ran(rig: &Rig, name: &str, channel: u64) {
    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let key = adk_session::build_namespaced_session_key(
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        name,
    );
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('brief', 'b.js', 'brief')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, status, turn_id, result_json,
                                   owned_tmux_session, finished_at)
         VALUES ('run', 'brief', 'succeeded', 'turn', $1, $2, NOW())",
    )
    .bind(serde_json::json!({"channel_id": channel.to_string()}))
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

async fn unbound(rig: &Rig) -> (Observed, Stamp, Arc<Mutex<Watch>>) {
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_none()
    );
    let watch = Arc::new(Mutex::new(Watch::default()));
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    let live = match &stamp.namespace.as_ref().unwrap().inventory {
        Inventory::Sessions(names) => names.clone(),
        other => panic!("live host inventory expected: {other:?}"),
    };
    assert!(live.iter().any(|name| name == MANUAL), "{live:?}");
    (answer, stamp, watch)
}

/// A manual session and a routine session that ran elsewhere leave the channel idle; an unowned
/// AgentDesk session, a failed routine read or a routine moved here keeps it unknown.
#[tokio::test(flavor = "current_thread")]
async fn unrelated_host_sessions_leave_a_session_less_channel_idle_pg() {
    let rig = Rig::new(69_183_101, false, false).await;
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    let unknown = observed(Activity::Unknown, "session_identity_unresolved");
    assert_eq!(unbound(&rig).await.0, unknown, "routine without evidence");
    routine_ran(&rig, ROUTINE, THREAD).await;
    let (answer, stamp, watch) = unbound(&rig).await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(stamp.publish(&watch, || ()).await.is_some());

    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let moved = format!(r#"{{"channel_id": "{}"}}"#, rig.channel);
    let moved_here = sqlx::query("UPDATE routine_runs SET result_json = $1::jsonb").bind(&moved);
    moved_here.execute(pool).await.unwrap();
    assert!(
        stamp.publish(&watch, || ()).await.is_none(),
        "an ownership change after the read published the old absence"
    );
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Unknown, "watcher_unbound")
    );

    let back = format!(r#"{{"channel_id": "{THREAD}"}}"#);
    let back = sqlx::query("UPDATE routine_runs SET result_json = $1::jsonb").bind(&back);
    back.execute(pool).await.unwrap();
    let hidden = "ALTER TABLE routine_runs RENAME TO routine_runs_hidden";
    sqlx::query(hidden).execute(pool).await.unwrap();
    assert_eq!(unbound(&rig).await.0, unknown, "a failed routine read");
    let shown = "ALTER TABLE routine_runs_hidden RENAME TO routine_runs";
    sqlx::query(shown).execute(pool).await.unwrap();

    host.start(STRAY);
    assert_eq!(
        unbound(&rig).await.0,
        unknown,
        "an unowned AgentDesk session"
    );
}

fn queued(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(200),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Input queued while the channel read unknown drains in order, once each, by the automatic
/// dequeue once only unrelated host sessions remain.
#[tokio::test(flavor = "current_thread")]
async fn queued_input_drains_in_order_once_unrelated_sessions_remain_pg() {
    let rig = Rig::new(69_183_102, false, false).await;
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    routine_ran(&rig, ROUTINE, THREAD).await;
    let mailbox = rig.shared.mailbox(rig.channel);
    for (id, text) in [(701, "A"), (702, "B")] {
        let token = &rig.shared.token_hash;
        let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, token, None);
        let enqueued = mailbox.enqueue(queued(id, text), persistence).await;
        assert!(enqueued.enqueued);
    }
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Idle, "no_session")
    );
    let mut taken = Vec::new();
    for _ in 0..3 {
        let take = crate::services::discord::mailbox_take_next_automatic_intervention;
        let outcome = take(&rig.shared, &ProviderKind::Claude, rig.channel).await;
        let Some(intervention) = outcome.intervention else {
            break;
        };
        taken.push(intervention.text.clone());
        crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
            &rig.shared,
            &ProviderKind::Claude,
            rig.channel,
            intervention.message_id,
            outcome.dispatch_lease.expect("automatic dequeue lease"),
        )
        .await;
    }
    assert_eq!(taken, ["A", "B"]);
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
}

/// The start gate and the human-input deliver both pass such a channel, and the deliver starts
/// exactly one turn.
#[tokio::test(flavor = "current_thread")]
async fn start_gate_and_deliver_start_once_beside_unrelated_sessions_pg() {
    let rig = Rig::new(69_183_103, false, false).await;
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    routine_ran(&rig, ROUTINE, THREAD).await;
    let channel = rig.channel.get();
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = vec![channel];
    }
    let registry = HealthRegistry::new();
    registry.register("claude".into(), rig.shared.clone()).await;
    let begun = start_without_gateway(channel);
    let provider = ProviderKind::Claude;
    let hold = external_turn_hold_for_start(Some(&registry), &provider, channel).await;
    assert_eq!(hold, None);
    let request = || HumanInputRequest {
        channel_id: rig.channel,
        provider: ProviderKind::Claude,
        text: "status?".into(),
        author_id: 200,
        source: "imessage".into(),
        metadata: None,
        channel_name_hint: None,
    };
    let first = deliver_human_input(&registry, request()).await.unwrap();
    let HumanInputDelivery::Started { turn_id } = &first else {
        panic!("{first:?}");
    };
    assert!(
        turn_id.starts_with(&format!("discord:{channel}:")),
        "{turn_id}"
    );
    let started = tokio::time::timeout(std::time::Duration::from_secs(1), begun.notified());
    assert!(started.await.is_ok(), "the deliver never began its start");
    let second = deliver_human_input(&registry, request()).await.unwrap();
    assert!(
        matches!(&second, HumanInputDelivery::Queued { reason, .. } if reason == "turn_active"),
        "a second start ran beside the first: {second:?}"
    );
}
