//! The owner's busy Codex tmux branch from `provider_dispatch::execute`: a real pane drawing a
//! captured busy Codex screen under a traced tmux, the owner's actor, and the pane's own record.
use super::tests::{AUTHOR, NAME, PROMPT, Rig, owned};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::claude_tui::input::submission::tests::wait;
use crate::services::discord::queue_io::input_owner::admission::TurnBind;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::fs;
use std::path::Path;

/// The open model turn of the captured rollout's first eleven records.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";
const ROWS: usize = 40;

fn fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    fs::read_to_string(dir.join(name)).unwrap()
}

/// A pane that draws the bottom of a captured busy, or idle, Codex screen with the cursor in its
/// empty composer; a paste is recorded and drawn in the composer, and each Enter is recorded.
fn codex_renderer(f: &Fixture, name: &str, idle: bool) -> std::path::PathBuf {
    let root = f.root.path().join(name);
    fs::create_dir(&root).unwrap();
    // An idle pane is a captured idle Codex composer at 80 columns, drawn at the pane's bottom.
    let (shown, width) = if idle {
        let idle = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
        let pad = "\n".repeat(ROWS);
        (pad + &fs::read_to_string(idle).unwrap(), "80")
    } else {
        (fixture("busy_empty.ansi"), "160")
    };
    let lines: Vec<&str> = shown.lines().collect();
    let rows = &lines[lines.len() - ROWS..];
    let plain = |row: &str| crate::services::codex_tui::input::strip_ansi_escape_sequences(row);
    let prompt = rows.iter().rposition(|row| plain(row).starts_with('›'));
    fs::write(root.join("screen"), rows.join("\n")).unwrap();
    fs::write(root.join("prompt-row"), prompt.unwrap().to_string()).unwrap();
    let rollout: String = fixture("model_turn.jsonl")
        .lines()
        .take(11)
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(root.join("transcript"), rollout).unwrap();
    let script = f.root.path().join("codex-busy-renderer.py");
    fs::write(
        &script,
        r#"import os, pathlib, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
rows = (root / 'screen').read_text().split('\n')
at = int((root / 'prompt-row').read_text())
def render(draft):
    shown = list(rows)
    if draft: shown[at] = '› ' + draft
    out = '\x1b[?2004h\x1b[2J' + ''.join('\x1b[%d;1H%s' % (i + 1, row) for i, row in enumerate(shown))
    os.write(1, (out + '\x1b[%d;%dH' % (at + 1, 3 + len(draft))).encode())
def event(name):
    with (root / 'events').open('a') as out: out.write(name + '\n')
render('')
(root / 'ready').write_text('ready')
pending = b''
while True:
    pending += os.read(0, 65536)
    if pending.startswith(b'\x1b[200~') and b'\x1b[201~' in pending:
        body, pending = pending[6:].split(b'\x1b[201~', 1)
        (root / 'body').write_bytes(body); event('paste')
        render(body.decode().split('\n')[0])
    if b'\r' in pending:
        event('Enter'); render(''); pending = b''
"#,
    )
    .unwrap();
    let command = format!("python3 '{}' '{}'", script.display(), root.display());
    let height = ROWS.to_string();
    let pane = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        name,
        "-x",
        width,
        "-y",
        &height,
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &pane, "window-size", "manual"]);
    f.tmux(&["resize-window", "-t", &pane, "-x", width, "-y", &height]);
    wait("codex-renderer", || root.join("ready").exists());
    wait("codex-renderer-capture", || {
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .contains("Ask Codex")
    });
    root
}

fn codex_rig(name: &str, channel: u64) -> Rig {
    codex_rig_on(name, channel, false)
}

fn codex_rig_on(name: &str, channel: u64, idle: bool) -> Rig {
    let f = Fixture::after_shared_test_env_lock();
    let renderer = codex_renderer(&f, name, idle);
    owned(f, name, channel, renderer, "codex")
}

impl Rig {
    fn codex(&self) -> Result<(), String> {
        self.dispatch(ProviderKind::Codex, target::Phase::Busy, &self.transcript())
    }

    /// The pane's tmux runs `args` on the real server once, after the wrapper's `step`.
    fn after(&self, step: &str, args: &[&str]) {
        let line = format!(
            "'{}' -L '{}' -f /dev/null {}",
            self.f.real.display(),
            self.f.socket,
            args.join(" ")
        );
        self.mark(&format!("after-{step}"), &line);
    }

    fn buffered(&self) -> bool {
        let buffers = self.f.tmux(&["list-buffers", "-F", "#{buffer_name}"]);
        buffers.contains(&format!("agentdesk-busy-inject-{}", self.nonce))
    }

    fn pasted(&self) -> String {
        fs::read_to_string(self.renderer.join("body")).unwrap_or_default()
    }

    fn frame(&self) -> String {
        busy_inject::frame("discord", AUTHOR, NAME, &self.nonce, PROMPT)
    }
}

/// A busy Codex pane takes main's frame as one paste and one Enter under the owner scope, and the
/// head settles without a receipt.
#[test]
fn owner_busy_codex_pastes_and_enters_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig("oc-busy", 7_272_521);
    assert_eq!(rig.codex(), Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(rig.pasted(), rig.frame());
    rig.delivered();
}

/// An owner refusal after the buffer is loaded and before the paste proves the same attempt was
/// not sent: the original goes back to custody unarmed, with no key, no buffer and no ledger row.
#[test]
fn owner_ready_refusal_before_the_codex_paste_is_known_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig("oc-ready", 7_272_522);
    // Another incarnation's marker: the owner's epoch refuses the pane, the composer still reads.
    let marker = crate::services::tmux_common::session_temp_path(&rig.name, "spawn_nonce");
    rig.mark("after-load", &format!("printf other > '{marker}'"));
    let result = rig.codex();
    assert_eq!(result, Err("NotSent(\"PaneUnavailable\")".into()));
    rig.held_not_sent("owner_refused");
    let snapshot = rig.snapshot();
    assert_eq!(
        snapshot["retained_inputs"][0]["input"]["attempt"]["nonce"],
        rig.nonce.as_str(),
        "{snapshot}"
    );
    assert_eq!(rig.bind(), TurnBind::NotArmed);
    assert!(!rig.buffered(), "the loaded buffer is dropped");
    let register = crate::services::tui_prompt_dedupe::register_injected_steer;
    assert!(
        register("codex", &rig.name, &rig.nonce, T, None),
        "the legacy ledger row was withdrawn"
    );
    crate::services::tui_prompt_dedupe::withdraw_injected_steer("codex", &rig.name, &rig.nonce);
}

/// After the paste nothing is read before Enter: a resized pane still gets its one Enter, and a
/// replaced pane process gets none and stays an unconfirmed write that is never resent.
#[test]
fn after_the_codex_paste_only_a_replaced_pane_stops_the_enter() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let resized = codex_rig("oc-resize", 7_272_523);
    let target = format!("={}:", resized.name);
    resized.after(
        "paste",
        &["resize-window", "-t", &target, "-x", "150", "-y", "38"],
    );
    assert_eq!(resized.codex(), Ok(()));
    assert_eq!(resized.events(), ["paste", "Enter"]);
    resized.delivered();
    let swapped = codex_rig("oc-swap", 7_272_524);
    swapped.mark("swap-after", "paste");
    let result = swapped.codex();
    assert_eq!(result, Err("WriteUncertain(\"enter_failed\")".into()));
    wait("swapped-pane-ready", || {
        swapped.renderer.join("ready").exists()
    });
    assert_eq!(swapped.events(), ["paste"]);
    swapped.uncertain();
    assert!(matches!(swapped.bind(), TurnBind::Armed(_)));
}

/// An idle Codex pane takes the prompt's typed text as one pinned paste and one Enter after a
/// single composer read; nothing reads the pane after the keys, and the head settles unreceipted.
#[test]
fn owner_idle_codex_pastes_and_enters_once_without_a_later_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-idle", 7_272_525, true);
    // Every tmux call from here on, the composer snapshot's own included, lands in one log.
    let shim = rig.f.root.path().join("tmux");
    let inner = rig.f.root.path().join("tmux-inner");
    fs::rename(&shim, &inner).unwrap();
    let log = rig.f.root.path().join("oc-idle-calls");
    let wrapper = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        log.display(),
        inner.display()
    );
    fs::write(&shim, wrapper).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let result = rig.dispatch(ProviderKind::Codex, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(rig.pasted(), codex_herdr_frame(PROMPT).unwrap());
    rig.delivered();
    let dir = rig.f.root.path().join(format!("{}-owner", rig.name));
    let trace = fs::read_to_string(dir.join("trace")).unwrap();
    let keys: Vec<_> = (trace.lines())
        .filter(|line| line.contains("paste-buffer") || line.contains("send-keys"))
        .collect();
    assert_eq!(keys.len(), 2, "{trace}");
    assert!(
        keys.iter().all(|line| line.contains("if-shell")),
        "pinned: {trace}"
    );
    let last = trace.lines().last().unwrap_or_default();
    assert!(last.contains("send-keys"), "no read after Enter: {trace}");
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    let pasted = (calls.iter().position(|c| c.contains("paste-buffer"))).expect("one paste");
    let reads: Vec<usize> = (0..calls.len())
        .filter(|&i| calls[i].contains("capture-pane"))
        .collect();
    assert_eq!(reads.len(), 1, "one composer read: {calls:#?}");
    assert!(reads[0] < pasted, "read before the paste: {calls:#?}");
    let count = |key: &str| calls.iter().filter(|c| c.contains(key)).count();
    assert_eq!(
        (count("paste-buffer"), count("Enter")),
        (1, 1),
        "{calls:#?}"
    );
}

/// An idle pane's owner effect needs the node's execution capacity first: a refused capacity sends
/// no key and reserves nothing, so the handoff never returns ahead of the capacity it runs under.
#[test]
fn an_idle_owner_front_needs_execution_capacity_before_any_key() {
    let path = module_path!().split_once("::").unwrap().1;
    let test = format!("{path}::an_idle_owner_front_needs_execution_capacity_before_any_key");
    if !crate::services::discord::admin_host_guard::tests::api_child(&test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let data = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.data.dir = data.path().to_path_buf();
    config.cluster.enabled = true;
    config.cluster.execution_slots = Some(1);
    crate::config_live_reload::install(config);
    let rig = codex_rig_on("oc-capacity", 7_272_527, true);
    let front = rig.granted_front();
    let result = rig.handoff(
        front,
        ProviderKind::Codex,
        target::Phase::Idle,
        &rig.transcript(),
    );
    assert_eq!(
        result,
        Err("NotSent(\"execution capacity requires PostgreSQL\")".into())
    );
    assert!(rig.events().is_empty(), "{:?}", rig.events());
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
}

/// A submitted idle turn holds its execution slot and lease until its AI turn ends, so another
/// channel finds the node full meanwhile; the slot returns once, and only then may another run.
#[test]
fn an_idle_owner_turn_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let test = format!("{path}::an_idle_owner_turn_holds_its_capacity_until_its_ai_turn_ends_pg");
    if crate::services::discord::admin_host_guard::tests::api_child(&test) {
        holds_capacity_to_the_turn_end(Fault::None);
    }
}

/// A write that may have landed, its Enter failing after the paste, holds the slot the same way.
#[test]
fn an_uncertain_idle_write_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let test =
        format!("{path}::an_uncertain_idle_write_holds_its_capacity_until_its_ai_turn_ends_pg");
    if crate::services::discord::admin_host_guard::tests::api_child(&test) {
        holds_capacity_to_the_turn_end(Fault::Enter);
    }
}

/// A turn whose Enter went but whose settlement could not be stored holds the slot the same way.
#[test]
fn a_sent_idle_turn_whose_settlement_fails_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let name =
        "a_sent_idle_turn_whose_settlement_fails_holds_its_capacity_until_its_ai_turn_ends_pg";
    if crate::services::discord::admin_host_guard::tests::api_child(&format!("{path}::{name}")) {
        holds_capacity_to_the_turn_end(Fault::Settle);
    }
}

/// A refusal before the first key whose not-sent custody cannot be stored either still proves
/// nothing went: no AI turn is awaited and the slot returns at once.
#[test]
fn an_unsent_idle_write_whose_custody_fails_returns_its_capacity_at_once_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let name = "an_unsent_idle_write_whose_custody_fails_returns_its_capacity_at_once_pg";
    if crate::services::discord::admin_host_guard::tests::api_child(&format!("{path}::{name}")) {
        unsent_returns_its_capacity_at_once();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    Enter,
    Settle,
}

/// A one-slot node with a database for its leases, and an idle Codex owner on `channel`.
fn slot_rig(name: &str, channel: u64) -> (tempfile::TempDir, Rig, TestPostgresDb, sqlx::PgPool) {
    let data = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.data.dir = data.path().to_path_buf();
    config.cluster.enabled = true;
    config.cluster.execution_slots = Some(1);
    crate::config_live_reload::install(config);
    let rig = codex_rig_on(name, channel, true);
    let db = rig.runtime.block_on(TestPostgresDb::create());
    let pool = rig.runtime.block_on(db.connect_and_migrate());
    rig.pool.set(pool.clone()).unwrap();
    (data, rig, db, pool)
}

fn leases(rig: &Rig, pool: &sqlx::PgPool) -> i64 {
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM node_execution_leases");
    rig.runtime.block_on(count.fetch_one(pool)).unwrap()
}

type Capacity =
    Result<Option<crate::services::cluster::execution_capacity::ExecutionGuard>, String>;

fn other_run(rig: &Rig, pool: &sqlx::PgPool, channel: u64) -> Capacity {
    let _entered = rig.runtime.enter();
    let cancel = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    crate::services::cluster::execution_capacity::acquire(Some(pool), "codex", channel, cancel)
}

/// A shell line that changes the owner's snapshot file behind it, so each later commit fails.
fn outside_write(channel: u64) -> String {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let path = root.join(format!("claude/input-owner-dispatch/{channel}.json"));
    format!("printf '\\n' >> '{}'", path.display())
}

fn holds_capacity_to_the_turn_end(fault: Fault) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_data, rig, db, pool) = slot_rig("oc-slot", 7_272_539);
    match fault {
        Fault::None => {}
        Fault::Enter => rig.mark("swap-after", "paste"),
        Fault::Settle => rig.mark("after-Enter", &outside_write(7_272_539)),
    }
    let (settled, answer) = tokio::sync::oneshot::channel();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let transcript = rig.transcript();
    std::thread::scope(|scope| {
        // Inside the scope, so a failed assertion drops it and ends the held AI turn.
        let (release, turn) = std::sync::mpsc::channel::<()>();
        let until_done: super::UntilDone = Box::new(move || {
            let _ = turn.recv();
            Ok(())
        });
        let first = scope.spawn(|| {
            let how = (
                ProviderKind::Codex,
                target::Phase::Idle,
                transcript.as_path(),
            );
            rig.execute_until(claim, how, settled, Some(until_done))
        });
        let wait = async { tokio::time::timeout(std::time::Duration::from_secs(20), answer).await };
        let answered = rig.runtime.block_on(wait);
        let (held, refused) = (leases(&rig, &pool), other_run(&rig, &pool, 7_272_540).err());
        let waiting = release.send(()).is_ok();
        assert!(waiting, "the capacity holder still waits for the AI turn");
        let first = first.join().unwrap();
        match fault {
            Fault::None => assert_eq!(first, Ok(())),
            Fault::Enter => assert_eq!(first, Err("WriteUncertain(\"enter_failed\")".into())),
            // The store's refusal after the Enter, never read as a write that surely did not land.
            Fault::Settle => assert!(
                first.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
                "{first:?}"
            ),
        }
        assert_eq!(answered.map(Result::ok), Ok(Some(first)));
        assert_eq!(held, 1, "the lease stays while the AI turn runs");
        assert!(
            refused.is_some_and(|e| e.contains("exhausted")),
            "the slot stays taken"
        );
    });
    if fault == Fault::Settle {
        assert_eq!(rig.events(), ["paste", "Enter"], "the turn was sent");
    }
    assert_eq!(leases(&rig, &pool), 0, "released once the turn ended");
    let next = other_run(&rig, &pool, 7_272_540).expect("the returned slot");
    assert!(next.is_some());
    assert!(
        other_run(&rig, &pool, 7_272_546).is_err(),
        "returned once: one slot, now taken"
    );
    drop(next);
    rig.runtime.block_on(db.drop());
}

fn unsent_returns_its_capacity_at_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_data, rig, db, pool) = slot_rig("oc-unsent", 7_272_547);
    // After the buffer loads and before the payload commit, so no key ever follows.
    rig.mark("after-load", &outside_write(7_272_547));
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = observed.clone();
    let until_done: super::UntilDone = Box::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    let (settled, _answer) = tokio::sync::oneshot::channel();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let how = (ProviderKind::Codex, target::Phase::Idle, rig.transcript());
    let result = rig.execute_until(claim, (how.0, how.1, &how.2), settled, Some(until_done));
    let observed = observed.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(observed, 0, "no AI turn is awaited: {result:?}");
    assert!(
        result.as_ref().is_err_and(|e| effect::surely_unsent(e)),
        "{result:?}"
    );
    assert!(!rig.events().contains(&"paste".to_string()), "no content");
    assert_eq!(leases(&rig, &pool), 0, "returned at once");
    let next = other_run(&rig, &pool, 7_272_540).expect("the returned slot");
    assert!(next.is_some());
    drop(next);
    rig.runtime.block_on(db.drop());
}

/// A first paste held by a pane replaced before it ran settles not sent with one notice; a pane
/// replaced after the paste holds only the Enter, a write that may have landed.
#[test]
fn an_idle_codex_paste_held_before_it_ran_settles_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-held", 7_272_528, true);
    let pane = rig.f.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={}:", rig.name),
        "#{pane_id}",
    ]);
    let swap = format!(
        "'{}' -L '{}' -f /dev/null respawn-pane -k -t '{pane}'\n",
        rig.f.real.display(),
        rig.f.socket
    );
    rig.mark("before-paste", &swap);
    let result = rig.dispatch(ProviderKind::Codex, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Err("NotSent(\"pane_moved\")".into()));
    wait("held-pane-ready", || rig.renderer.join("ready").exists());
    rig.held_not_sent("pane_moved");
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    let notice = &snapshot["retained_inputs"][0]["notice"];
    assert_eq!(notice["kind"], "retained_retry_required", "{snapshot}");

    let after = codex_rig_on("oc-held-enter", 7_272_529, true);
    after.mark("swap-after", "paste");
    let result = after.dispatch(
        ProviderKind::Codex,
        target::Phase::Idle,
        &after.transcript(),
    );
    assert_eq!(result, Err("WriteUncertain(\"enter_failed\")".into()));
    wait("enter-pane-ready", || after.renderer.join("ready").exists());
    assert_eq!(after.events(), ["paste"]);
    after.uncertain();
}
