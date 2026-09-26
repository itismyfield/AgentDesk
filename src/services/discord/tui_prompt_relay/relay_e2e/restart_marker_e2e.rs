//! Planned restart mid-turn: a TUI-direct turn marked `drain_restart` is adopted by the
//! replacement, closes when it finishes, and blocks neither the next turn nor restart.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::json;

use super::discord_mock::{self, CHANNEL_ID};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{self, RelayOwnerKind};
use crate::services::discord::{InflightRestartMode, SharedData};
use crate::services::provider::ProviderKind::Claude;
use crate::services::tmux_common as markers;
use crate::services::tui_prompt_dedupe::ExternalInputRelayLease;

const PHASE: &str = "ADK_RESTART_MARKER_E2E_PHASE";
const PHASE_OK: &str = "restart-marker phase ok:";
const SESSION_UUID: &str = "62940000-0000-4000-8000-000000000002";
const ANCHOR: u64 = 629_400_000_000_000_001;
const PLACEHOLDER: u64 = 629_400_000_000_000_101;
const RUNNING_PROMPT: &str = "turn spanning the restart prompt 6294";
const RUNNING_BODY: &str = "turn spanning the restart body 6294";
const NEXT_PROMPT: &str = "turn after the restart prompt 6294";
const NEXT_BODY: &str = "turn after the restart body 6294";

// Global flags are skipped; one live pane, whose screen is `$TMUX_TMPDIR/pane` (idle when absent).
const FAKE_TMUX: &str = r#"#!/bin/sh
while [ "${1#-}" != "$1" ]; do shift; done
case "$1" in
  list-sessions) echo "SESSION" ;;
  list-panes) echo 0 ;;
  capture-pane) [ -f "$TMUX_TMPDIR/pane" ] && while read -r line; do echo "$line"; done <"$TMUX_TMPDIR/pane" ;;
esac
exit 0
"#;

type Channel = Vec<(u64, String)>;
type Mock = discord_mock::DiscordMockState;
const LOGS: &str = "agentdesk::services::discord::tmux=info,agentdesk::relay_flight_recorder=info";

/// Inside a child, returns its phase. The parent runs one fresh process per phase over
/// one runtime root and fake `tmux`; each phase after the first is a dcserver restart.
fn phase(test: &str, phases: &[&str], session: &str) -> Option<String> {
    if let Some(phase) = std::env::var_os(PHASE) {
        let logs = tracing_subscriber::fmt().with_env_filter(LOGS);
        let _ = logs.with_writer(std::io::stderr).try_init();
        return Some(phase.to_string_lossy().into_owned());
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().expect("child root");
    let r = root.path();
    let (bin, tmp) = (r.join("bin"), r.join("tmp"));
    std::fs::create_dir_all(&bin).expect("bin dir");
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let tmux = bin.join("tmux");
    std::fs::write(&tmux, FAKE_TMUX.replace("SESSION", session)).expect("fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let module = module_path!().split_once("::").expect("crate path").1;
    let env = [
        ("AGENTDESK_ROOT_DIR", r.to_path_buf()),
        ("AGENTDESK_CONFIG", r.join("config").join("agentdesk.yaml")),
        ("CLAUDE_CONFIG_DIR", r.join("claude")),
        ("HOME", r.to_path_buf()),
        ("TMPDIR", tmp.clone()),
        ("TMUX_TMPDIR", tmp),
        ("PATH", bin),
        ("AGENTDESK_STATUS_INTERVAL_SECS", PathBuf::from("0")),
    ];
    for phase in phases {
        let exact = format!("{module}::{test}");
        let args = ["--exact", &exact, "--nocapture", "--test-threads=1"];
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        child.args(args).env(PHASE, phase).envs(env.clone());
        let out = child.output().expect("child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let ok = out.status.success() && stdout.contains(&format!("{PHASE_OK} {phase}"));
        assert!(ok, "phase {phase}:\n{stdout}\n{stderr}");
    }
    None
}

fn prompt(id: &str, text: &str) -> String {
    let line = json!({"type": "user", "uuid": id, "message": {"role": "user", "content": text}});
    format!("{line}\n")
}

fn tool_call(id: &str) -> String {
    let content = json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {}}]);
    let message = json!({"role": "assistant", "content": content});
    format!(
        "{}\n",
        json!({"type": "assistant", "uuid": format!("{id}-tool"), "message": message})
    )
}

fn finish(id: &str, body: &str) -> String {
    let message = json!({"role": "assistant", "content": [{"type": "text", "text": body}]});
    let said = json!({"type": "assistant", "uuid": format!("{id}-said"), "message": message});
    let stop = json!({"type": "system", "subtype": "stop_hook_summary"});
    format!("{said}\n{stop}\n")
}

fn append(path: &Path, bytes: &str) -> u64 {
    use std::io::Write;
    let mut options = std::fs::File::options();
    let mut file = options.create(true).append(true).open(path).expect("open");
    file.write_all(bytes.as_bytes()).expect("append transcript");
    std::fs::metadata(path).expect("transcript").len()
}

/// The Claude TUI pane bound to the channel, launched once and alive across every
/// dcserver restart; returns its transcript path.
fn live_pane(root: &Path, tmux: &str, launch_now: bool) -> PathBuf {
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
    let transcript = transcript_path(&cwd, SESSION_UUID, None).expect("transcript path");
    if !launch_now {
        return transcript;
    }
    let role_map = crate::runtime_layout::role_map_path(root);
    std::fs::create_dir_all(role_map.parent().expect("role map dir")).expect("role map dir");
    let binding = json!({"byChannelId": {CHANNEL_ID.to_string():
        {"roleId": "adk-cc", "promptFile": "prompt.md", "provider": "claude"}}});
    std::fs::write(role_map, binding.to_string()).expect("role map");
    std::fs::write(markers::session_temp_path(tmux, "generation"), "1").expect("generation");
    let kind = markers::write_tmux_runtime_kind_marker(tmux, RuntimeHandoffKind::ClaudeTui);
    kind.and_then(|()| markers::write_tmux_channel_binding(tmux, CHANNEL_ID))
        .expect("binding");
    markers::write_tmux_owner_marker(tmux).expect("owner marker");
    let launch = markers::session_temp_path(tmux, markers::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
    let cd = cwd.display();
    let script = format!("#!/bin/sh\ncd {cd}\nexec claude --session-id {SESSION_UUID}\n");
    std::fs::write(launch, script).expect("launch script");
    std::fs::create_dir_all(transcript.parent().expect("project dir")).expect("project dir");
    transcript
}

/// Discord as a viewer sees it: every message not deleted, by id.
fn visible(mock: &Mock) -> Channel {
    let deleted = mock.deleted.lock().expect("mock deletions").clone();
    let messages = mock.messages.lock().expect("mock messages");
    let shown = messages.iter().filter(|(id, _)| !deleted.contains(id));
    shown
        .map(|(id, (_, content))| (*id, content.clone()))
        .collect()
}

fn copies(shown: &Channel, body: &str) -> Vec<u64> {
    let holding = shown.iter().filter(|(_, content)| content.contains(body));
    holding.map(|(id, _)| *id).collect()
}

/// Messages still showing the `...` placeholder or a streaming spinner glyph.
fn spinning(shown: &Channel) -> usize {
    let glyph = |c: char| ('\u{2801}'..='\u{28ff}').contains(&c);
    let spins = |content: &String| content == "..." || content.chars().any(glyph);
    shown.iter().filter(|(_, content)| spins(content)).count()
}

/// Each phase keeps the channel it leaves, so the next restart boots over the same Discord.
fn kept(root: &Path, phase: &str) -> Channel {
    let bytes = std::fs::read(root.join(format!("discord-{phase}.json"))).expect("kept channel");
    serde_json::from_slice(&bytes).expect("channel json")
}

fn keep(root: &Path, phase: &str, shown: &Channel) {
    let bytes = serde_json::to_vec(shown).expect("channel json");
    std::fs::write(root.join(format!("discord-{phase}.json")), bytes).expect("keep channel");
}

async fn eventually(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline && !done() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

/// Returns once the mock has taken no request-visible change for two seconds.
async fn settle(mock: &Mock) {
    let (mut last, mut quiet) = (Vec::new(), 0);
    for _ in 0..150 {
        let now = visible(mock);
        quiet = if now == last { quiet + 1 } else { 0 };
        if quiet == 10 {
            return;
        }
        last = now;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("Discord never went quiet: {last:?}");
}

async fn wait_for_body(mock: &Mock, body: &str) {
    let found = || !copies(&visible(mock), body).is_empty();
    eventually(Duration::from_secs(20), found).await;
    settle(mock).await;
}

type Observed =
    tokio::sync::broadcast::Receiver<crate::services::tui_prompt_dedupe::ObservedTuiPrompt>;

/// Boots a replacement dcserver over `channel` in boot order: the idle relay, then the
/// watcher restore, which re-adopts any surviving inflight row.
async fn boot(generation: u64, tmux: &str, channel: Channel) -> (Mock, Observed) {
    let mock = Mock::new();
    mock.placeholder_posts.store(1, Ordering::SeqCst);
    let minted = &mock.next_response_id;
    minted.fetch_add(generation * 1_000, Ordering::SeqCst);
    let carried = channel.into_iter().map(|(id, c)| (id, (None, c)));
    mock.messages.lock().expect("mock messages").extend(carried);
    let (proxy, gateway, _server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let mut shared: Arc<SharedData> = crate::services::discord::make_shared_data_for_tests();
    let fresh = Arc::get_mut(&mut shared).expect("fresh shared data");
    fresh.restart.current_generation = generation;
    let http = &shared.http;
    assert!(http.cached_serenity_ctx.set(ctx.clone()).is_ok());
    assert!(http.cached_bot_token.set("test-token".into()).is_ok());
    // Production runs the session-bound relay supervisor, which turns this on.
    let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
    crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health)
        .enable_delivery_for_test();
    let observed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
    super::super::spawn_tui_prompt_relay(shared.clone(), Claude);
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;
    let rehydrated = eventually(Duration::from_secs(20), || binding(tmux).is_some()).await;
    assert!(rehydrated, "the idle relay never rehydrated {tmux}");
    crate::services::discord::tmux::restore_tmux_watchers(&ctx.http, &shared).await;
    let channel = ChannelId::new(CHANNEL_ID);
    let watched = shared.tmux_watchers.contains_key(&channel);
    assert!(watched, "the restore attached no watcher to {tmux}");
    (mock, observed)
}

/// Ends this dcserver process the way a restart does: at once, without draining
/// the idle tails and watchers it still runs.
fn end_process(phase: &str, outcome: Result<(), String>) -> ! {
    let failed = outcome.is_err();
    let line = outcome.map_or_else(|detail| detail, |()| format!("{PHASE_OK} {phase}"));
    println!("{line}");
    std::process::exit(i32::from(failed))
}

/// The outgoing process marks every live row before it exits.
fn shut_down_for_restart() {
    let mark = inflight::mark_all_inflight_states_restart_mode_checked;
    mark(&Claude, InflightRestartMode::DrainRestart).expect("drain_restart marker");
}

/// An injected turn is running when the restart is requested: the row is the TUI-direct
/// synthetic claim's, anchored after the prompt line, and its placeholder is up.
fn mark_running_turn(tmux: &str, transcript: &Path, id: &str, text: &str, ids: (u64, u64)) {
    let turn_start = append(transcript, &prompt(id, text));
    append(transcript, &tool_call(id));
    let mut lease = ExternalInputRelayLease::unassigned(Some(CHANNEL_ID));
    lease.turn_id = Some(format!("external:claude:{CHANNEL_ID}:{tmux}:1"));
    lease.session_key = Some(format!("claude/test-token-hash/localhost:{tmux}"));
    lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let (channel, anchor) = (ChannelId::new(CHANNEL_ID), MessageId::new(ids.0));
    let build = super::super::synthetic_start::build_tui_direct_synthetic_inflight_state;
    let (owner, output) = (RelayOwnerKind::Watcher, Some(transcript));
    let mut row = build(
        Claude, channel, anchor, None, text, tmux, output, turn_start, &lease, owner,
    );
    row.turn_nonce = Some(format!("62940000-{id}"));
    row.current_msg_id = ids.1;
    row.watcher_owner_channel_id = Some(CHANNEL_ID);
    assert!(inflight::save_inflight_state_if_absent(&row).expect("running row"));
    shut_down_for_restart();
}

fn verdict<T: Debug + PartialEq>(actual: T, want: T, shown: &Channel) -> Result<(), String> {
    let wrong = format!("{actual:?}, want {want:?}; shown={shown:?}");
    (actual == want).then_some(()).ok_or(wrong)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_marked_drain_restart_is_closed_after_it_finishes_6294() {
    let test = "a_turn_marked_drain_restart_is_closed_after_it_finishes_6294";
    let tmux = Claude.build_tmux_session_name(&CHANNEL_ID.to_string());
    let Some(phase) = phase(test, &["old", "first", "second"], &tmux) else {
        return;
    };
    let root = PathBuf::from(std::env::var("AGENTDESK_ROOT_DIR").expect("child root"));
    let generation = crate::services::discord::runtime_store::allocate_process_generation();
    let transcript = live_pane(&root, &tmux, phase == "old");
    let report = root.join("first-replacement.json");
    if phase == "old" {
        let ids = (ANCHOR, PLACEHOLDER);
        mark_running_turn(&tmux, &transcript, "run", RUNNING_PROMPT, ids);
        end_process(&phase, Ok(()));
    }
    let carried = (phase != "first").then(|| kept(&root, "first"));
    let (mock, mut observed) = boot(generation, &tmux, carried.unwrap_or_default()).await;
    if phase == "first" {
        // The running turn finishes, then the next injected turn arrives: its prompt
        // lands first and its answer a moment later.
        append(&transcript, &finish("run", RUNNING_BODY));
        wait_for_body(&mock, RUNNING_BODY).await;
        append(&transcript, &prompt("next", NEXT_PROMPT));
        let mut seen = 0;
        let mut count = || {
            let prompts = std::iter::from_fn(|| observed.try_recv().ok());
            seen += prompts.filter(|p| p.prompt.contains(NEXT_PROMPT)).count();
            seen
        };
        eventually(Duration::from_secs(10), || count() > 0).await;
        append(&transcript, &finish("next", NEXT_BODY));
        wait_for_body(&mock, NEXT_BODY).await;
        std::fs::write(&report, count().to_string()).expect("first replacement report");
        keep(&root, &phase, &visible(&mock));
        shut_down_for_restart();
        end_process(&phase, Ok(()));
    }

    // The second replacement boots over whatever the first one left behind.
    settle(&mock).await;
    let shown = visible(&mock);
    let seen = std::fs::read_to_string(&report).expect("first replacement report");
    let row = inflight::load_inflight_state(&Claude, CHANNEL_ID);
    let row = row.map(|row| (row.user_msg_id, row.restart_generation));
    // (messages holding each body, messages still spinning, next prompt observed, row left)
    let bodies = (
        copies(&shown, RUNNING_BODY),
        copies(&shown, NEXT_BODY).len(),
    );
    let want = ((vec![PLACEHOLDER], 1), 0, "1".to_string(), None);
    end_process(
        &phase,
        verdict((bodies, spinning(&shown), seen, row), want, &shown),
    );
}

const DELIVERED_PROMPT: &str = "turn delivered before the restart prompt 6294";
const DELIVERED_BODY: &str = "turn delivered before the restart body 6294";
const MARKED_BODY: &str = "turn marked by the restart body 6294";
const LATER_BODIES: [&str; 2] = ["later turn one body 6294", "later turn two body 6294"];
const BUSY_BODY: &str = "turn running at the fix boot body 6294";
const BUSY_PANE: &str = "✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)\n";

/// The first boot of the fix over a row an older build kept marked: the row's turn and
/// later ones finished unrelayed, its placeholder shows the next turn, and the pane is busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_boot_over_a_marked_row_keeps_delivered_and_foreign_bodies_6294() {
    let test = "first_boot_over_a_marked_row_keeps_delivered_and_foreign_bodies_6294";
    let tmux = Claude.build_tmux_session_name(&CHANNEL_ID.to_string());
    let Some(phase) = phase(test, &["old", "first", "fix", "second"], &tmux) else {
        return;
    };
    let root = PathBuf::from(std::env::var("AGENTDESK_ROOT_DIR").expect("child root"));
    let generation = crate::services::discord::runtime_store::allocate_process_generation();
    let transcript = live_pane(&root, &tmux, phase == "old");
    let pane = PathBuf::from(std::env::var("TMUX_TMPDIR").expect("tmux tmpdir")).join("pane");
    let (report, target) = (root.join("fix-boot.json"), PLACEHOLDER + 1);
    let foreign = format!("{}\n\n⠋ {}", LATER_BODIES[0], LATER_BODIES[0]);
    // A row stays only for the busy turn, which starts at the offset the report pins.
    let busy_row = |start: u64| {
        let row = inflight::load_inflight_state(&Claude, CHANNEL_ID);
        row.is_none_or(|row| row.turn_start_offset.is_some_and(|offset| offset >= start))
    };
    if phase == "old" {
        let ids = (ANCHOR, PLACEHOLDER);
        mark_running_turn(&tmux, &transcript, "run", DELIVERED_PROMPT, ids);
        end_process(&phase, Ok(()));
    }
    if phase == "first" {
        // The running turn is delivered; the next one is streaming at the next restart.
        let (mock, _) = boot(generation, &tmux, Vec::new()).await;
        append(&transcript, &finish("run", DELIVERED_BODY));
        wait_for_body(&mock, DELIVERED_BODY).await;
        let shown = [visible(&mock), vec![(target, "...".to_string())]].concat();
        keep(&root, &phase, &shown);
        let marked = "turn marked by the restart prompt 6294";
        mark_running_turn(&tmux, &transcript, "marked", marked, (ANCHOR + 1, target));
        end_process(&phase, Ok(()));
    }
    if phase == "fix" {
        // What older builds left: every turn finished unrelayed and the marked row's
        // placeholder streamed the next turn; the pane now runs one more.
        append(&transcript, &finish("marked", MARKED_BODY));
        for (index, body) in LATER_BODIES.iter().enumerate() {
            let id = format!("later{index}");
            let turn = [prompt(&id, "later turn 6294"), finish(&id, body)];
            append(&transcript, &turn.concat());
        }
        let busy_start = append(&transcript, "");
        let turn = [prompt("busy", "busy turn 6294"), tool_call("busy")];
        append(&transcript, &turn.concat());
        std::fs::write(&pane, BUSY_PANE).expect("busy pane");
        let mut channel = kept(&root, "first");
        channel.retain(|(id, _)| *id != target);
        channel.push((target, foreign.clone()));
        let (mock, _) = boot(generation, &tmux, channel.clone()).await;
        settle(&mock).await;
        let shown = visible(&mock);
        let written: Vec<_> = shown.iter().filter(|m| !channel.contains(m)).collect();
        println!("MEASURE fix boot wrote {written:?}");
        let pinned = json!([busy_start, busy_row(busy_start)]).to_string();
        std::fs::write(&report, pinned).expect("fix boot report");
        keep(&root, &phase, &shown);
        shut_down_for_restart();
        end_process(&phase, Ok(()));
    }

    // The next restart boots over the fix boot's channel; the busy turn then finishes.
    let report = std::fs::read(&report).expect("fix boot report");
    let (busy_start, row_at_fix): (u64, bool) = serde_json::from_slice(&report).expect("json");
    let before = kept(&root, "fix");
    let (mock, _) = boot(generation, &tmux, before.clone()).await;
    settle(&mock).await;
    let row_at_restart = busy_row(busy_start);
    std::fs::remove_file(&pane).expect("idle pane");
    append(&transcript, &finish("busy", BUSY_BODY));
    wait_for_body(&mock, BUSY_BODY).await;
    let shown = visible(&mock);
    let changed = shown.iter().filter(|entry| !before.contains(entry));
    let rewritten = changed.filter(|(_, c)| !c.contains(BUSY_BODY)).count();
    let at_target = shown.iter().find(|(id, _)| *id == target).cloned();
    let closed = inflight::load_inflight_state(&Claude, CHANNEL_ID).is_none();
    // (delivered body copies, marked row's placeholder, rows only for the busy turn at
    // each boot, messages the next restart rewrote, busy body copies, row closed)
    let delivered = copies(&shown, DELIVERED_BODY).len();
    let actual = (delivered, at_target, [row_at_fix, row_at_restart]);
    let after = (rewritten, copies(&shown, BUSY_BODY).len(), closed);
    let want = ((1, Some((target, foreign)), [true, true]), (0, 1, true));
    end_process(&phase, verdict((actual, after), want, &shown));
}
