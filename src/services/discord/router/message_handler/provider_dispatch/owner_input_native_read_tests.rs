//! A start whose pane's native Claude turn runs or cannot be read waits unclaimed, or unsent once
//! claimed, with a watch of that pane; when the pane reads idle it runs once as its own turn.
use super::*;

#[path = "owner_input_codex_native_tests.rs"]
pub(super) mod codex;

const PERMISSION_MODE: &str =
    "{\"type\":\"permission-mode\",\"permissionMode\":\"bypassPermissions\"}\n";
const START_WAITS: &str = "NotSent(\"synthetic_waits_idle\")";

/// The shim counts native source and liveness reads separately; only a third source lookup fails.
/// A selected liveness read can wait for an explicit release.
struct Shim {
    dir: std::path::PathBuf,
}

impl Shim {
    fn install(rig: &IntakeRig) -> Self {
        let root = rig._tmux.root.path();
        let dir = root.join("native-read");
        fs::create_dir_all(&dir).unwrap();
        let (shim, inner) = (root.join("tmux"), root.join("tmux-inner"));
        fs::rename(&shim, &inner).unwrap();
        let script = format!(
            "#!/bin/sh\nd='{dir}'\n\
             case \"$*\" in *capture-pane*) [ -f \"$d/screen\" ] && {{ cat \"$d/screen\"; exit 0; }} ;; esac\n\
             case \"$*\" in *pane_current_path*)\n\
               c=$(cat \"$d/cwds\" 2>/dev/null || echo 0); echo $((c+1)) > \"$d/cwds\"\n\
               if [ -f \"$d/source-limit\" ] && [ ! -f \"$d/failed\" ] && \
                  [ \"$c\" -ge \"$(cat \"$d/source-limit\")\" ]; then\n\
                 : > \"$d/failed\"; echo 'native source requery refused' >&2; exit 1\n\
               fi\n\
               if [ -f \"$d/busy-armed\" ] && [ -f \"$(cat \"$d/row-path\")\" ]; then\n\
                 rm -f \"$d/busy-armed\"; cat \"$d/busy-rows\" >> \"$(cat \"$d/transcript\")\"\n\
               fi ;;\n\
             esac\n\
             case \"$*\" in *list-panes*pane_dead*)\n\
               n=$(cat \"$d/reads\" 2>/dev/null || echo 0); n=$((n+1)); echo \"$n\" > \"$d/reads\"\n\
               if [ \"$(cat \"$d/park-at\" 2>/dev/null)\" = \"$n\" ]; then\n\
                 : > \"$d/parked\"; while [ ! -f \"$d/release\" ]; do sleep 0.02; done\n\
               fi ;;\n\
             esac\n\
             exec '{inner}' \"$@\"\n",
            dir = dir.display(),
            inner = inner.display()
        );
        fs::write(&shim, script).unwrap();
        fs::set_permissions(&shim, fs::metadata(&inner).unwrap().permissions()).unwrap();
        Self { dir }
    }

    /// The pane draws a running spinner, so its composer would not take a send.
    fn composer_busy(&self) {
        fs::write(self.dir.join("screen"), "✻ Thinking… (esc to interrupt)\n").unwrap();
    }

    fn composer_ready(&self) {
        let _ = fs::remove_file(self.dir.join("screen"));
    }

    /// The pane's second liveness read, the first after the early one, waits for `release`.
    fn park_second_read(&self) {
        fs::write(self.dir.join("park-at"), "2").unwrap();
    }

    /// A third source resolution fails; independent physical-liveness checks stay available.
    fn guard_cached_source(&self) {
        fs::write(self.dir.join("source-limit"), "2").unwrap();
    }

    /// The next read made while the channel's inflight row `row` exists, so after a claim, finds a
    /// native turn opened in `transcript`, once.
    fn busy_once_claimed(&self, row: &Path, transcript: &Path) {
        fs::write(self.dir.join("row-path"), row.display().to_string()).unwrap();
        fs::write(
            self.dir.join("transcript"),
            transcript.display().to_string(),
        )
        .unwrap();
        fs::write(self.dir.join("busy-rows"), BUSY).unwrap();
        fs::write(self.dir.join("busy-armed"), "").unwrap();
    }

    async fn parked(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self.dir.join("parked").exists() {
            assert!(std::time::Instant::now() < deadline, "no read parked");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn release(&self) {
        fs::write(self.dir.join("release"), "").unwrap();
    }
}

/// How the start's pane reads when it meets it, and how that read clears.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Read {
    /// Unknown before the claim: session markers with a composer that would not take a send.
    Early,
    /// Idle at the early read, unknown at the late read after the claim.
    Late,
    /// A native turn opens between the early and the late read.
    LateBusy,
    /// Busy at the early read; its turn ends while the pane reads unknown.
    Armed,
    /// Session markers over an idle composer read idle.
    ReadyTail,
    /// Idle at both reads; a later read of the pane's liveness fails once.
    CachedSource,
    /// A native turn opens after each claim, three times; the pane then reads idle.
    LateAgain,
    /// As `Early`, for a start naming no pane: its session has no name, and Discord names it.
    Nameless,
    /// As `Nameless`, with only the channel's registered name to go by.
    Registered,
    /// As `Nameless` on an idle pane, while Discord's lookup of the channel fails.
    Unlooked,
}

async fn start_meets(id: u64, case: Read) {
    use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
    const ORIGIN: &str = "G-native-read";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = IntakeRig::turn_start_stored(id, pool.clone(), ORIGIN).await;
    let early = matches!(
        case,
        Read::Early | Read::Nameless | Read::Registered | Read::Unlooked
    );
    let key = discord::adk_session::build_adk_session_key(
        &rig.shared,
        rig.channel,
        &ProviderKind::Claude,
        None,
    );
    let key = key.await.expect("channel session key");
    discord::inflight::seed_session_row_keyed(&pool, &key, rig.channel.get(), None).await;
    if matches!(case, Read::Nameless | Read::Registered | Read::Unlooked) {
        let mut core = rig.shared.core.lock().await;
        core.sessions.get_mut(&rig.channel).unwrap().channel_name = None;
    }
    if case == Read::Registered {
        rig.mock.unname_channel(id);
        let config = rig._tmux.root.path().join("cg-d1-fixture.yaml");
        let yaml = fs::read_to_string(&config).unwrap();
        let bound = format!("claude: {{id: '{id}', runtime: tui}}");
        let named = format!("claude: {{id: '{id}', name: 'a1-rollback-{id}', runtime: tui}}");
        assert!(yaml.contains(&bound));
        fs::write(&config, yaml.replace(&bound, &named)).unwrap();
    }
    let native = rig.renderer.join("transcript");
    if case == Read::Unlooked {
        rig.mock.fail_channel(id);
    }
    let initial = match case {
        Read::Armed => BUSY.to_string(),
        Read::Unlooked => IDLE.to_string(),
        _ if early => format!("{IDLE}{PERMISSION_MODE}"),
        Read::ReadyTail => format!("{PERMISSION_MODE}{IDLE}"),
        _ => IDLE.to_string(),
    };
    fs::write(&native, initial).unwrap();
    fs::remove_file(&rig.transcript).unwrap();
    fs::hard_link(&native, &rig.transcript).unwrap();
    let append = |rows: &str| {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&rig.transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, rows.as_bytes()).unwrap();
    };
    let shim = Shim::install(&rig);
    match case {
        Read::Unlooked => {}
        _ if early => shim.composer_busy(),
        Read::Late | Read::LateBusy => shim.park_second_read(),
        Read::CachedSource => shim.guard_cached_source(),
        _ => {}
    }
    let channels = [OriginChannel {
        provider: "claude".into(),
        channel_id: rig.channel.get(),
    }];
    let answer = || lookup(&pool, "agent", ORIGIN, &channels);
    assert!(matches!(
        answer().await,
        Ok(Some(InputResult::Accepted { .. }))
    ));
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{id}"));
    let headless = format!("[session-strategy] channel={id}");
    let (drive, mut settled) = rig.drive();
    if matches!(case, Read::ReadyTail | Read::CachedSource) {
        let (completed, bridge) = tokio::sync::oneshot::channel();
        *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((rig.channel, completed));
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        runs_once(&rig, &pool, ORIGIN, &mut settled, bridge, append).await;
        if case == Read::CachedSource {
            assert_eq!(
                fs::read_to_string(shim.dir.join("cwds")).unwrap().trim(),
                "2",
                "native source must use the cached late read"
            );
            assert!(
                !shim.dir.join("failed").exists(),
                "native source must not be resolved a third time"
            );
        }
        drop(rig);
        pool.close().await;
        db.drop().await;
        return;
    }
    if case == Read::LateAgain {
        let row = discord::runtime_store::discord_inflight_root().unwrap();
        let row = row.join("claude").join(format!("{id}.json"));
        shim.busy_once_claimed(&row, &rig.transcript);
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        for round in 0..3 {
            // Each late hold is a scheduling hold: no start failure, retained input or notice.
            let held = next_settle(&mut settled, &rig.trace).await;
            assert_eq!(held, Err(START_WAITS.into()), "round {round}");
            assert!(
                !shim.dir.join("busy-armed").exists(),
                "round {round}: no claimed read"
            );
            let state = rig.state().await;
            let count = |key: &str| state[key].as_array().unwrap().len();
            let queued = (count("items"), count("retained_inputs"));
            assert_eq!(queued, (1, 0), "round {round}: {state}");
            let owed = "SELECT count(*) FROM message_outbox WHERE source='retained_input_notice'";
            let owed: i64 = sqlx::query_scalar(owed).fetch_one(&pool).await.unwrap();
            assert_eq!(owed, 0, "round {round}");
            wait("the held attempt's row cleared", || !row.exists());
            if round < 2 {
                shim.busy_once_claimed(&row, &rig.transcript);
            }
            append(IDLE);
        }
        let (completed, bridge) = tokio::sync::oneshot::channel();
        *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((rig.channel, completed));
        runs_once(&rig, &pool, ORIGIN, &mut settled, bridge, append).await;
        drop(rig);
        pool.close().await;
        db.drop().await;
        return;
    }
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    if matches!(case, Read::Late | Read::LateBusy) {
        shim.parked().await;
        match case {
            Read::Late => {
                append(PERMISSION_MODE);
                shim.composer_busy();
            }
            _ => append(BUSY),
        }
        shim.release();
    }
    let held = next_settle(&mut settled, &rig.trace).await;
    let live = matches!(
        crate::services::platform::tmux::pane_liveness(&name),
        crate::services::platform::tmux::PaneLiveness::Live
    );
    let busy = (
        held,
        events(&rig.renderer),
        live,
        rig.trace.text().contains("killing tmux session"),
        origin_receipts(&pool, ORIGIN).await,
    );
    let expected = (
        Err(START_WAITS.to_string()),
        Vec::<String>::new(),
        true,
        false,
        Vec::<String>::new(),
    );
    assert_eq!(busy, expected, "{}", rig.trace.text());
    if early || case == Read::Armed {
        // Held before its claim: no headless turn began and nothing was posted.
        let started = rig.trace.text().contains(&headless);
        let posted = (rig.mock.written_texts().len(), rig.mock.deleted().len());
        assert_eq!((started, posted), (false, (0, 0)), "{}", rig.trace.text());
    }
    if case == Read::Unlooked {
        assert!(rig.mock.failed_channel_reads() > 0, "no lookup failed");
    }
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert!(state["head_injection"].is_null(), "unattempted: {state}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !matches!(answer().await, Ok(Some(InputResult::Accepted { .. }))) {
        assert!(std::time::Instant::now() < deadline, "not accepted again");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if case == Read::Armed {
        // The native turn ends while the pane reads unknown.
        append(&format!("{IDLE}{PERMISSION_MODE}"));
        shim.composer_busy();
    }
    // While the pane reads busy or unknown, the watch hands nothing.
    let again = tokio::time::timeout(Duration::from_secs(6), settled.recv()).await;
    assert!(again.is_err(), "handed while unreadable: {again:?}");
    assert!(events(&rig.renderer).is_empty());
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    match case {
        Read::LateBusy => append(IDLE),
        Read::Unlooked => rig.mock.heal_channel(id),
        _ => {
            append(IDLE);
            shim.composer_ready();
        }
    }
    runs_once(&rig, &pool, ORIGIN, &mut settled, bridge, append).await;
    drop(rig);
    pool.close().await;
    db.drop().await;
}

/// The start, now on an idle pane, runs once as its own turn with no other wake: one Enter, one
/// completed receipt, and an empty queue.
async fn runs_once(
    rig: &IntakeRig,
    pool: &sqlx::PgPool,
    origin: &str,
    settled: &mut tokio::sync::mpsc::UnboundedReceiver<Result<(), String>>,
    bridge: tokio::sync::oneshot::Receiver<()>,
    append: impl Fn(&str),
) {
    let outbox = pool.clone();
    let worker = tokio::spawn(async move {
        loop {
            let sent = "UPDATE message_outbox SET status = 'sent' WHERE status = 'pending'";
            let _ = sqlx::query(sent).execute(&outbox).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let ran = next_settle(settled, &rig.trace).await;
    assert_eq!(ran, Ok(()), "{}", rig.trace.text());
    wait("the start's own user row", || {
        fs::read_to_string(&rig.transcript)
            .unwrap()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|row| {
                    row["type"] == "user"
                        && row["message"]["content"]
                            .as_str()
                            .is_some_and(|s| s.contains(PROMPT))
                })
            })
    });
    let answer = json!({"type":"assistant","uuid":"own-answer","apiBlockIndex":0,"message":
        {"id":"own-answer","role":"assistant","content":[{"type":"text","text":"own turn answer"}],
        "stop_reason":"end_turn"}});
    append(&format!("{answer}\n{IDLE}"));
    tokio::time::timeout(Duration::from_secs(90), bridge)
        .await
        .unwrap_or_else(|_| panic!("{}", rig.trace.text()))
        .unwrap();
    let again = tokio::time::timeout(Duration::from_secs(6), settled.recv()).await;
    worker.abort();
    let enters = events(&rig.renderer)
        .iter()
        .filter(|e| *e == "Enter")
        .count();
    let after = (enters, origin_receipts(pool, origin).await);
    assert_eq!(
        after,
        (1, vec!["completed".to_string()]),
        "{}",
        rig.trace.text()
    );
    assert!(!matches!(again, Ok(Some(_))), "a second handoff: {again:?}");
    assert!(rig.state().await["items"].as_array().unwrap().is_empty());
}

/// Runs `start_meets` in its own child process, as its tmux and Postgres fixtures need.
async fn native_read_pg(name: &str, id: u64, case: Read) {
    let test =
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::";
    if creator_child(&format!("{test}rollback::native_read::{name}")) {
        start_meets(id, case).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_waits_unclaimed_while_its_pane_reads_unknown_pg() {
    native_read_pg(
        "a_start_waits_unclaimed_while_its_pane_reads_unknown_pg",
        7_772_101,
        Read::Early,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_start_waits_unsent_when_its_late_read_is_unknown_pg() {
    native_read_pg(
        "a_claimed_start_waits_unsent_when_its_late_read_is_unknown_pg",
        7_772_102,
        Read::Late,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_held_late_by_a_native_turn_runs_once_after_it_ends_pg() {
    native_read_pg(
        "a_start_held_late_by_a_native_turn_runs_once_after_it_ends_pg",
        7_772_103,
        Read::LateBusy,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_armed_watch_keeps_a_start_while_its_pane_reads_unknown_pg() {
    native_read_pg(
        "an_armed_watch_keeps_a_start_while_its_pane_reads_unknown_pg",
        7_772_104,
        Read::Armed,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_markers_over_an_idle_composer_let_a_start_run_pg() {
    native_read_pg(
        "session_markers_over_an_idle_composer_let_a_start_run_pg",
        7_772_105,
        Read::ReadyTail,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_late_read_is_registered_without_reading_the_pane_again_pg() {
    native_read_pg(
        "an_idle_late_read_is_registered_without_reading_the_pane_again_pg",
        7_772_107,
        Read::CachedSource,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_late_holds_spend_no_start_failure_and_post_no_notice_pg() {
    native_read_pg(
        "repeated_late_holds_spend_no_start_failure_and_post_no_notice_pg",
        7_772_110,
        Read::LateAgain,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_naming_no_pane_waits_on_the_pane_discord_names_pg() {
    native_read_pg(
        "a_start_naming_no_pane_waits_on_the_pane_discord_names_pg",
        7_772_108,
        Read::Nameless,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_naming_no_pane_waits_on_its_registered_pane_pg() {
    native_read_pg(
        "a_start_naming_no_pane_waits_on_its_registered_pane_pg",
        7_772_109,
        Read::Registered,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_pane_name_lookup_failed_waits_unclaimed_until_named_pg() {
    native_read_pg(
        "a_start_whose_pane_name_lookup_failed_waits_unclaimed_until_named_pg",
        7_772_111,
        Read::Unlooked,
    )
    .await;
}

/// The late hold's read wakes the drive only after the held attempt's own unwind: while the
/// channel's mailbox still holds a turn, or that attempt's inflight row is left, it reads unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_hold_reads_unknown_until_its_attempt_unwinds() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::",
        "rollback::native_read::a_late_hold_reads_unknown_until_its_attempt_unwinds"
    )) {
        return;
    }
    use crate::services::discord::inflight;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let id = 7_772_106;
    let rig = IntakeRig::new(id, false).await;
    fs::create_dir_all(rig.transcript.parent().unwrap()).unwrap();
    fs::write(&rig.transcript, IDLE).unwrap();
    let tmux = ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{id}"));
    let target = |own_pane| super::super::super::NativeTarget {
        tmux: tmux.clone(),
        dir: None,
        session_id: None,
        own_pane,
    };
    let nonce = "late-held-attempt".to_string();
    let read = |mailbox: ChannelMailboxHandle| {
        let probe = super::super::super::late_probe(
            ProviderKind::Claude,
            target(true),
            mailbox,
            (id, Some(nonce.clone())),
        );
        tokio::task::spawn_blocking(move || {
            let mut probe = probe;
            probe()
        })
    };
    use crate::services::claude_tui::busy_inject::Screen;
    assert_eq!(read(rig.mailbox.clone()).await.unwrap(), Screen::Ready);
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        id,
        None,
        7,
        9,
        0,
        "held".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some(nonce.clone());
    inflight::save_inflight_state_create_new(&row).unwrap();
    assert_eq!(read(rig.mailbox.clone()).await.unwrap(), Screen::Unknown);
    assert!(inflight::clear_inflight_state(&ProviderKind::Claude, id));
    assert_eq!(read(rig.mailbox.clone()).await.unwrap(), Screen::Ready);
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    let started = rig.mailbox.try_start_turn(
        token,
        poise::serenity_prelude::UserId::new(7),
        poise::serenity_prelude::MessageId::new(9),
    );
    assert!(started.await);
    assert_eq!(read(rig.mailbox.clone()).await.unwrap(), Screen::Unknown);
}
