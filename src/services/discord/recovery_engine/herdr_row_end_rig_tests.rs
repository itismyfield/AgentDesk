//! A prior process's Herdr row through boot restore and the source tick, against the shared Herdr
//! fixture, a Discord fake and a fresh database, each scenario in its own child process.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::super::*;
use super::{CURRENT, PRIOR, on};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::{self, DiscordMockState};
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::tui_prompt_dedupe::binding_events::{CauseSource, Proposal, record_verified};

const NONCE: &str = "turn-row-end";
const ANSWER: &str = "the restarted turn's own answer";
const STALE: &str = "a wrapper file's stale answer";
const SENTINEL: u64 = 1_479_671_399_000_001;
const READY: &str = "Claude Code v2.1.141\n\n\u{276f} \nstatus";
const BUSY: &str = "\u{2733} Architecting\u{2026}";
const CODEX_READY: &str = "earlier output\n\n\u{203a}\n\n  gpt-5.5 xhigh \u{b7} /fixture/workspace";
const PHASE: &str = "ADK_HERDR_ROW_END_PHASE";
const DATABASE: &str = "ADK_HERDR_ROW_END_DATABASE";

/// What the relay in front of the fixture's socket answers instead of the fixture.
#[derive(Clone, Default)]
struct Pane {
    screen: Option<String>,
    /// The pane's foreground is back at its shell.
    exited: bool,
    /// Every reply is unparseable.
    garbled: bool,
}

/// Sits at the fixture's socket path and passes each request through, answering screen reads and
/// rewriting process reads as `Pane` says.
struct Relay {
    pane: Arc<Mutex<Pane>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    real: PathBuf,
    path: PathBuf,
}

impl Relay {
    fn at(path: &Path) -> Self {
        let real = path.with_extension("real");
        std::fs::rename(path, &real).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pane = Arc::new(Mutex::new(Pane::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, stopped, target) = (pane.clone(), stop.clone(), real.clone());
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let pane = seen.lock().unwrap().clone();
                        serve(client, &target, &pane);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            pane,
            stop,
            thread: Some(thread),
            real,
            path: path.to_path_buf(),
        }
    }

    fn set(&self, change: impl FnOnce(&mut Pane)) {
        change(&mut self.pane.lock().unwrap());
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::rename(&self.real, &self.path);
    }
}

fn serve(client: UnixStream, real: &Path, pane: &Pane) {
    let _ = client.set_nonblocking(false);
    let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
    let mut request = String::new();
    let _ = BufReader::new(&client).read_line(&mut request);
    let parsed: Value = serde_json::from_str(&request).unwrap_or(Value::Null);
    let reply = if pane.garbled {
        "not a herdr reply\n".to_string()
    } else if let (Some(screen), "pane.read") =
        (&pane.screen, parsed["method"].as_str().unwrap_or(""))
    {
        let read = json!({"pane_id": parsed["params"]["pane_id"], "source": parsed["params"]["source"],
            "text": screen, "revision": 7, "truncated": false});
        format!(
            "{}\n",
            json!({"id": parsed["id"], "result": {"type": "pane_read", "read": read}})
        )
    } else {
        let Ok(mut server) = UnixStream::connect(real) else {
            return;
        };
        let _ = server.write_all(request.as_bytes());
        let mut reply = String::new();
        let _ = BufReader::new(&server).read_line(&mut reply);
        match serde_json::from_str::<Value>(&reply) {
            Ok(mut answer) if pane.exited && answer["result"]["type"] == "pane_process_info" => {
                let info = &mut answer["result"]["process_info"];
                let shell = info["shell_pid"].clone();
                info["foreground_process_group_id"] = shell.clone();
                info["foreground_processes"] = json!([{"pid": shell, "name": "zsh"}]);
                format!("{answer}\n")
            }
            _ => reply,
        }
    };
    let mut client = client;
    let _ = client.write_all(reply.as_bytes());
    let _ = client.shutdown(std::net::Shutdown::Write);
}

/// How a scenario's child sets its threads up.
#[derive(Clone, Copy, PartialEq)]
enum Owner {
    Legacy,
    O,
}

/// Runs `scenario` in this test's child on a runtime whose every thread has the fixture's
/// registry, settlement off as in production and, unless O, no O channel.
fn in_child<F, Fut>(path: &str, channel: u64, provider: ProviderKind, owner: Owner, scenario: F)
where
    F: FnOnce(herdr::P11TerminationRig) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if !herdr::in_child(path, channel) {
        return;
    }
    child(channel, provider, owner, scenario);
}

fn child<F, Fut>(channel: u64, provider: ProviderKind, owner: Owner, scenario: F)
where
    F: FnOnce(herdr::P11TerminationRig) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let rig = herdr::P11TerminationRig::boot(channel);
    let fixture = rig.thread_installer();
    let codex = provider == ProviderKind::Codex;
    // A restarted phase keeps the root its parent gave both phases.
    let (dir, _env) = match std::env::var_os(PHASE) {
        Some(_) => (None, None),
        None => {
            let dir = tempfile::tempdir().unwrap();
            let env = crate::config::set_agentdesk_root_for_test(dir.path());
            (Some(dir), Some(env))
        }
    };
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let install = Arc::new(move || {
        fixture();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&root));
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        if owner == Owner::Legacy {
            std::mem::forget(crate::services::tui_o::cutover::test_override::force_channels(&[]));
        }
        if codex {
            std::mem::forget(crate::services::turn_host::force_codex_switch_for_test(
                Some(true),
            ));
        }
    });
    install();
    let each = install.clone();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(move || each())
        .build()
        .unwrap()
        .block_on(scenario(rig));
    drop(dir);
}

/// One channel's runtime with a Herdr execution bound, its source logged and a prior process's
/// path-less row on disk.
struct Run {
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    db: Option<TestPostgresDb>,
    mock: DiscordMockState,
    server: tokio::task::JoinHandle<()>,
    provider: ProviderKind,
    channel: ChannelId,
    key: String,
    logical: String,
    source: PathBuf,
    row: inflight::InflightTurnState,
    relay: Relay,
    _switches: herdr::LiveSwitches,
}

fn generation(value: u64) {
    let path = super::super::super::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

/// `started_at`'s second plus one, as the provider stamps the turn's records.
fn stamp(row: &inflight::InflightTurnState) -> String {
    let at = inflight::parse_started_at_unix(&row.started_at).unwrap() + 1;
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap()
        .to_rfc3339()
}

/// The turn's records up to its last output, no terminal yet.
fn open_turn(provider: &ProviderKind, row: &inflight::InflightTurnState) -> String {
    let at = stamp(row);
    match provider {
        ProviderKind::Codex => format!(
            "{}\n{}\n",
            json!({"type": "event_msg", "timestamp": at, "payload": {"type": "task_started", "turn_id": "t1"}}),
            json!({"type": "event_msg", "timestamp": at, "payload": {"type": "user_message", "message": "question"}}),
        ),
        _ => format!(
            "{}\n{}\n",
            json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": "question"}}),
            json!({"type": "assistant", "timestamp": at, "message": {"content": [{"type": "text", "text": "partial"}]}}),
        ),
    }
}

/// The provider's own terminal for the open turn.
fn terminal(provider: &ProviderKind, row: &inflight::InflightTurnState) -> String {
    let at = stamp(row);
    match provider {
        ProviderKind::Codex => format!(
            "{}\n",
            json!({"type": "event_msg", "timestamp": at, "payload": {"type": "task_complete", "turn_id": "t1"}})
        ),
        _ => format!(
            "{}\n",
            json!({"type": "result", "timestamp": at, "subtype": "success", "result": ANSWER})
        ),
    }
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

struct Setup {
    provider: ProviderKind,
    channel: u64,
    /// The Discord fake answers the channel, so routing reads it as a direct message.
    routed: bool,
}

impl Run {
    /// Binds the execution, logs its source, saves the prior process's row and restart mark,
    /// then moves this process to the next generation.
    async fn start(rig: &herdr::P11TerminationRig, setup: Setup) -> Self {
        let (db, pool) = match std::env::var(DATABASE) {
            Ok(url) => {
                let pool =
                    crate::db::postgres::connect_test_pool_with_max_connections(&url, "row-end", 4)
                        .await
                        .unwrap();
                (None, pool)
            }
            Err(_) => {
                let db = TestPostgresDb::create().await;
                let pool = db.connect_and_migrate().await;
                (Some(db), pool)
            }
        };
        let mock = DiscordMockState::new();
        if setup.routed {
            mock.allow_channel(setup.channel);
        }
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let mut shared =
            super::super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .expect("fresh shared")
            .restart
            .current_generation = CURRENT;
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(7);
            settings.allow_all_users = true;
            if !setup.routed {
                settings.allowed_channel_ids = vec![discord_mock::CHANNEL_ID];
            }
        }
        shared.http.cached_serenity_ctx.set(ctx).unwrap();
        let provider = setup.provider;
        let logical = provider.build_tmux_session_name("row-end");
        let key = format!(
            "{}/{}/{}:{logical}",
            provider.as_str(),
            shared.token_hash,
            herdr::NODE
        );
        let token = shared.token_hash.clone();
        let restarted = std::env::var(PHASE).as_deref() == Ok("after");
        if restarted {
            // The old launch's context; the fixture writes the same one again.
            let context = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap())
                .join("runtime/binding_contexts/claude")
                .join(format!("{}.json", herdr::EXECUTION));
            std::fs::remove_file(context).unwrap();
            // The same execution as the old process recorded it, at this process's fixture socket.
            sqlx::query("DELETE FROM sessions WHERE session_key = $1")
                .bind(&key)
                .execute(&pool)
                .await
                .unwrap();
        }
        let bound = herdr::HostedState::Bound;
        let switches = match provider {
            ProviderKind::Codex => {
                let context = crate::services::tui_prompt_dedupe::binding_context::BindingContext {
                    schema: 1,
                    provider: "codex".into(),
                    created_at: chrono::Utc::now(),
                    execution_nonce: herdr::EXECUTION.into(),
                    tmux_session: logical.clone(),
                    channel_id: Some(setup.channel),
                    owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
                    host: None,
                    expected_native_session_id: None,
                    launch_mode: "fresh".into(),
                    provider_root: None,
                    first_prompt_digest: None,
                    source_policy: None,
                };
                let prepared =
                    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(context)
                        .unwrap();
                let switches = rig
                    .bind_as(&pool, (&token, &key), bound, "codex", &prepared.path)
                    .await;
                crate::services::discord::stamp_spawn_markers(&logical, Some(&prepared)).unwrap();
                switches
            }
            _ => {
                let switches = rig.bind(&pool, &token, &key, bound).await;
                let marker =
                    crate::services::tmux_common::session_temp_path(&logical, "spawn_nonce");
                std::fs::write(marker, herdr::EXECUTION).unwrap();
                switches
            }
        };
        let record = match provider {
            ProviderKind::Codex => rig.record_as((&token, &key), bound, "codex"),
            _ => rig.record(&token, &key),
        };
        let relay = Relay::at(Path::new(&record.location.unwrap().socket_addr));
        let root_dir = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
        let source = root_dir.join(format!("{}-source.jsonl", provider.as_str()));
        let row = if restarted {
            inflight::load_inflight_state_read_only(&provider, setup.channel)
                .expect("the old process's row")
        } else {
            generation(PRIOR);
            let mut row = inflight::InflightTurnState::new(
                provider.clone(),
                setup.channel,
                Some("row-end".into()),
                7,
                51,
                52,
                "question".into(),
                None,
                Some(logical.clone()),
                None,
                None,
                0,
            );
            row.runtime_kind = Some(match provider {
                ProviderKind::Codex => {
                    crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                }
                _ => crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            });
            row.turn_nonce = Some(NONCE.into());
            inflight::save_inflight_state(&row).unwrap();
            std::fs::write(&source, open_turn(&provider, &row)).unwrap();
            let meta = std::fs::metadata(&source).unwrap();
            let path = source.display().to_string();
            let source_id = crate::services::tui_prompt_dedupe::binding_events::SourceId {
                session_id: "session-row-end".into(),
                path: source.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
            };
            let proposal = Proposal {
                channel_id: setup.channel,
                provider: provider.as_str(),
                tmux_session: &logical,
                session_id: Some("session-row-end"),
                path: &path,
                replaced: None,
                cause: CauseSource::Observed,
                hook: None,
            };
            record_verified(&proposal, &source_id).unwrap();
            // The shutdown's own mark, as the old process's SIGTERM path writes it.
            let marked = inflight::mark_restart_mode_blocking(
                provider.clone(),
                InflightRestartMode::DrainRestart,
            )
            .await;
            assert_eq!(marked.marked, 1, "the shutdown marks the row");
            inflight::load_inflight_state_read_only(&provider, setup.channel).unwrap()
        };
        generation(CURRENT);
        Self {
            shared,
            pool,
            db,
            mock,
            server,
            provider,
            channel: ChannelId::new(setup.channel),
            key,
            logical,
            source,
            row,
            relay,
            _switches: switches,
        }
    }

    /// Boot restore, then the provider's source tick, as a restarted process runs them.
    async fn boot(&self) {
        let http = self.shared.serenity_http_or_token_fallback().unwrap();
        restore_inflight_turns(&http, &self.shared, &self.provider).await;
        crate::services::discord::tui_prompt_relay::spawn_tui_prompt_relay(
            self.shared.clone(),
            self.provider.clone(),
        );
    }

    /// One end pass, returning once every task it started has finished.
    async fn pass(&self) {
        row_end_pass(&self.shared, &self.provider).await;
        settled_tasks().await;
    }

    fn durable(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get())
    }

    fn texts(&self) -> Vec<String> {
        self.mock.written_texts()
    }

    async fn hosted(&self) -> Option<(herdr::HostedState, String)> {
        herdr::row(&self.pool, &self.key).await
    }

    fn interrupted(&self) -> String {
        interrupted_recovery_message(&self.row, &self.row.full_response)
    }

    /// The next input waiting in the queue, and each time the idle kickoff takes it with whether
    /// the row was still on disk then.
    async fn sentinel(
        &self,
    ) -> (
        Arc<Mutex<Vec<bool>>>,
        crate::services::discord::queue_io::IdleQueueKickHookResetForTests,
    ) {
        let intervention = crate::services::discord::Intervention {
            author_id: serenity::model::id::UserId::new(7),
            author_is_bot: false,
            message_id: serenity::model::id::MessageId::new(SENTINEL),
            queued_generation: CURRENT,
            source_message_ids: vec![serenity::model::id::MessageId::new(SENTINEL)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "sentinel".into(),
            mode: crate::services::discord::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let persistence = crate::services::discord::queue_persistence_context(
            &self.shared,
            &self.provider,
            self.channel,
        );
        self.shared
            .mailbox(self.channel)
            .replace_queue(vec![intervention], persistence)
            .await;
        let taken = Arc::new(Mutex::new(Vec::new()));
        let (seen, channel) = (taken.clone(), self.channel);
        let reset = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(
            Arc::new(move |shared, provider, kicked, _reason| {
                let seen = seen.clone();
                Box::pin(async move {
                    if kicked != channel {
                        return None;
                    }
                    let present =
                        inflight::load_inflight_state_read_only(&provider, kicked.get()).is_some();
                    let next = crate::services::discord::mailbox_take_next_automatic_intervention(
                        &shared, &provider, kicked,
                    )
                    .await;
                    if let Some(item) = next.intervention.as_ref() {
                        seen.lock().unwrap().push(present);
                        crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                            &shared,
                            &provider,
                            kicked,
                            item.message_id,
                            next.dispatch_lease.as_ref().expect("dequeue lease").clone(),
                        )
                        .await;
                    }
                    Some(crate::services::discord::IdleQueueKickoffChannelOutcome {
                        started: next.intervention.is_some(),
                    })
                })
            }),
        );
        (taken, reset)
    }

    async fn close(self) {
        self.server.abort();
        drop(self.relay);
        self.pool.close().await;
        if let Some(db) = self.db {
            db.drop().await;
        }
    }
}

async fn settled_tasks() {
    let idle = wait(Duration::from_secs(30), || {
        ENDING.lock().unwrap().is_empty()
    })
    .await;
    assert!(idle, "the end task finished");
}

async fn wait(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

/// The row ended: one notice of `text`, the row gone, the hold released and the next input taken
/// once, after the clear.
async fn assert_ended(run: &Run, text: &str, taken: &Mutex<Vec<bool>>) {
    let delivered = wait(Duration::from_secs(30), || run.durable().is_none()).await;
    assert!(delivered, "the row ends: {:?}", run.texts());
    let texts = run.texts();
    assert_eq!(
        texts.iter().filter(|t| t.contains(text)).count(),
        1,
        "one notice of {text:?}: {texts:?}"
    );
    assert!(!texts.iter().any(|t| t.contains(STALE)), "{texts:?}");
    assert!(!herdr::held(), "the execution's input hold is released");
    let once = wait(Duration::from_secs(20), || {
        !taken.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(once, "the next input was taken");
    assert_eq!(
        *taken.lock().unwrap(),
        vec![false],
        "taken once, after the clear"
    );
}

/// Nothing changed: the row as saved, no Discord text, the execution bound and held.
async fn assert_kept(run: &Run, saved: &inflight::InflightTurnState, case: &str) {
    let durable = run
        .durable()
        .unwrap_or_else(|| panic!("{case}: the row stays"));
    assert_eq!(durable.save_generation, saved.save_generation, "{case}");
    assert_eq!(durable.turn_nonce, saved.turn_nonce, "{case}");
    assert_eq!(run.texts(), Vec::<String>::new(), "{case}");
    let bound = (herdr::HostedState::Bound, herdr::EXECUTION.to_string());
    assert_eq!(run.hosted().await, Some(bound), "{case}");
    assert!(herdr::held(), "{case}: the hold stays");
}

const R1: u64 = 1_479_671_399_000_101;

/// A Claude turn dispatched before a restart and finished after it: the restarted process's boot
/// restore leaves the row, and the next source tick shows the transcript's own answer once,
/// clears the row, releases the hold and takes the next input once.
#[test]
fn r1_restarted_claude_turn_ends_with_its_transcript_answer_pg() {
    if herdr::is_child() {
        let phase = std::env::var(PHASE).unwrap();
        return child(R1, ProviderKind::Claude, Owner::Legacy, |rig| {
            r1(rig, phase)
        });
    }
    let path = concat!(
        module_path!(),
        "::r1_restarted_claude_turn_ends_with_its_transcript_answer_pg"
    );
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for phase in ["before", "after"] {
            let home = tempfile::tempdir().unwrap();
            let output = herdr::child_command(path, R1, home.path())
                .env(PHASE, phase)
                .env(DATABASE, &db.database_url)
                .env("AGENTDESK_ROOT_DIR", root.path())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{phase}: {}\n{stdout}\n{stderr}",
                output.status
            );
            if phase == "after" {
                assert!(
                    stdout
                        .lines()
                        .any(|line| line.starts_with("test result: ok. 1 passed;")),
                    "{stdout}\n{stderr}"
                );
                eprintln!(
                    "{}",
                    stdout
                        .lines()
                        .filter(|l| l.starts_with("ROW_END"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
        pool.close().await;
        db.drop().await;
    });
}

async fn r1(rig: herdr::P11TerminationRig, phase: String) {
    let carried = herdr::held();
    let run = Run::start(
        &rig,
        Setup {
            provider: ProviderKind::Claude,
            channel: R1,
            routed: true,
        },
    )
    .await;
    if phase == "before" {
        // A wrapper file the session name once had: the wrapper path would show it.
        let stale = crate::services::tmux_common::session_temp_path(&run.logical, "jsonl");
        let line = json!({"type": "result", "subtype": "success", "result": STALE});
        std::fs::write(stale, format!("{line}\n")).unwrap();
        println!("ROW_END before: row saved and marked");
        std::io::stdout().flush().unwrap();
        // The SIGTERM path's own exit: nothing of this process survives but its files and rows.
        std::process::exit(0);
    }
    assert!(carried, "the hold outlived the old process");
    append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
    run.relay.set(|pane| pane.screen = Some(READY.into()));
    let (taken, _reset) = run.sentinel().await;
    let started = Instant::now();
    run.boot().await;
    assert_ended(&run, ANSWER, &taken).await;
    assert_eq!(
        run.hosted().await,
        Some((herdr::HostedState::Bound, herdr::EXECUTION.to_string())),
        "an idle execution stays bound"
    );
    println!("ROW_END observed->settled {:?}", started.elapsed());
    run.close().await;
}

const R2: u64 = 1_479_671_399_000_102;

/// A Codex Herdr turn the same way: the interrupt notice, its rollout untouched.
#[test]
fn r2_restarted_codex_turn_ends_with_the_recovery_notice_pg() {
    let path = concat!(
        module_path!(),
        "::r2_restarted_codex_turn_ends_with_the_recovery_notice_pg"
    );
    in_child(
        path,
        R2,
        ProviderKind::Codex,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Codex,
                    channel: R2,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Codex, &run.row));
            let rollout = std::fs::read(&run.source).unwrap();
            run.relay.set(|pane| pane.screen = Some(CODEX_READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.boot().await;
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            assert_eq!(
                std::fs::read(&run.source).unwrap(),
                rollout,
                "the rollout is only read"
            );
            run.close().await;
        },
    );
}

const R3: u64 = 1_479_671_399_000_103;

/// Busy at boot: passes keep the row while the provider works, and the first pass after it goes
/// idle ends it.
#[test]
fn r3_a_turn_busy_at_boot_ends_on_a_later_pass_pg() {
    let path = concat!(
        module_path!(),
        "::r3_a_turn_busy_at_boot_ends_on_a_later_pass_pg"
    );
    in_child(
        path,
        R3,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: R3,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.boot().await;
            // Two source ticks at least.
            tokio::time::sleep(Duration::from_secs(11)).await;
            assert_kept(&run, &run.row, "busy").await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            assert_ended(&run, ANSWER, &taken).await;
            run.close().await;
        },
    );
}

const R4: u64 = 1_479_671_399_000_104;

/// The execution ended while the process was down: the interrupt notice once, the execution
/// retired, the hold released and the next input taken once.
#[test]
fn r4_an_ended_execution_retires_and_ends_its_row_pg() {
    let path = concat!(
        module_path!(),
        "::r4_an_ended_execution_retires_and_ends_its_row_pg"
    );
    in_child(
        path,
        R4,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: R4,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let (taken, _reset) = run.sentinel().await;
            run.boot().await;
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
            assert_eq!(run.hosted().await, Some(retired));
            run.close().await;
        },
    );
}

const NEGATIVE: u64 = 1_479_671_399_000_105;

/// Every read short of a clear idle or end keeps the row, its execution and its hold, and shows
/// nothing: a long busy turn, a stale ready screen over a turn still open, an unreadable endpoint
/// and a replaced source.
#[test]
fn unresolved_reads_keep_the_row_as_production_does_pg() {
    let path = concat!(
        module_path!(),
        "::unresolved_reads_keep_the_row_as_production_does_pg"
    );
    in_child(
        path,
        NEGATIVE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: NEGATIVE,
                    routed: true,
                },
            )
            .await;
            let saved = run.row.clone();
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            run.pass().await;
            assert_kept(&run, &saved, "long busy turn").await;
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert_kept(&run, &saved, "stale ready screen, turn still open").await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.garbled = true);
            run.pass().await;
            assert_kept(&run, &saved, "unreadable endpoint").await;
            // The same records under a new file: not the source the execution logged.
            run.relay.set(|pane| {
                pane.garbled = false;
                pane.screen = Some(READY.into());
            });
            let bytes = std::fs::read(&run.source).unwrap();
            std::fs::remove_file(&run.source).unwrap();
            std::fs::write(&run.source, bytes).unwrap();
            run.pass().await;
            assert_kept(&run, &saved, "replaced source").await;
            run.close().await;
        },
    );
}

const LIVE: u64 = 1_479_671_399_000_106;

/// A clear idle with a live turn on the channel's mailbox, or that turn's nonce indexed, is
/// another turn's: the row stays.
#[test]
fn a_live_actor_keeps_an_idle_row_pg() {
    let path = concat!(module_path!(), "::a_live_actor_keeps_an_idle_row_pg");
    in_child(
        path,
        LIVE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: LIVE,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let token = Arc::new(
                crate::services::provider::CancelToken::from_persisted_turn_nonce(Some(
                    "live-turn".into(),
                )),
            );
            run.shared
                .mailbox(run.channel)
                .restore_active_turn(
                    token,
                    serenity::model::id::UserId::new(7),
                    serenity::model::id::MessageId::new(61),
                )
                .await;
            // A row from before generations were stamped, as the live turn's own row looks.
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &run.provider,
                run.channel.get(),
            );
            let mut json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            json["born_generation"] = 0.into();
            std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            let saved = run.durable().unwrap();
            run.pass().await;
            assert_kept(&run, &saved, "live mailbox turn").await;
            run.close().await;
        },
    );
}

const UNROUTED: u64 = 1_479_671_399_000_107;

/// A channel routing no longer admits keeps even an ended execution's row: nothing is retired.
#[test]
fn an_unrouted_channel_keeps_an_ended_row_pg() {
    let path = concat!(
        module_path!(),
        "::an_unrouted_channel_keeps_an_ended_row_pg"
    );
    in_child(
        path,
        UNROUTED,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: UNROUTED,
                    routed: false,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            run.pass().await;
            assert_kept(&run, &run.row, "unrouted").await;
            run.close().await;
        },
    );
}

const RETIRE_RACE: u64 = 1_479_671_399_000_108;

/// A successor launched between the reads and the retire: the CAS refuses, so the successor's
/// row, execution and hold stay and nothing is shown.
#[test]
fn a_successor_before_the_retire_keeps_everything_pg() {
    let path = concat!(
        module_path!(),
        "::a_successor_before_the_retire_keeps_everything_pg"
    );
    in_child(
        path,
        RETIRE_RACE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: RETIRE_RACE,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let mut successor = run.row.clone();
            successor.turn_nonce = Some("successor".into());
            successor.born_generation = CURRENT;
            let (pool, key, next) = (run.pool.clone(), run.key.clone(), successor.clone());
            on("before_retire", move || {
                let (pool, key, next) = (pool.clone(), key.clone(), next.clone());
                Box::pin(async move {
                    sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{execution_nonce}', '\"successor-execution\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                    inflight::save_inflight_state(&next).unwrap();
                })
            });
            run.pass().await;
            let durable = run.durable().expect("the successor's row");
            assert_eq!(durable.turn_nonce.as_deref(), Some("successor"));
            let hosted: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT hosted_execution->>'execution_nonce', hosted_execution->>'state' FROM sessions WHERE session_key = $1",
            )
            .bind(&run.key)
            .fetch_one(&run.pool)
            .await
            .unwrap();
            assert_eq!(hosted.0.as_deref(), Some("successor-execution"));
            assert_ne!(hosted.1.as_deref(), Some("retired"), "{hosted:?}");
            assert!(herdr::held());
            assert_eq!(run.texts(), Vec::<String>::new());
            run.close().await;
        },
    );
}

const CLEAR_RACE: u64 = 1_479_671_399_000_109;

/// A successor row written after the notice: the clear sees another turn and keeps it.
#[test]
fn a_successor_before_the_clear_keeps_its_row_pg() {
    let path = concat!(
        module_path!(),
        "::a_successor_before_the_clear_keeps_its_row_pg"
    );
    in_child(
        path,
        CLEAR_RACE,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: CLEAR_RACE,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let mut successor = run.row.clone();
            successor.turn_nonce = Some("successor".into());
            successor.born_generation = CURRENT;
            on("before_clear", move || {
                let next = successor.clone();
                Box::pin(async move { inflight::save_inflight_state(&next).unwrap() })
            });
            run.pass().await;
            let durable = run.durable().expect("the successor's row");
            assert_eq!(durable.turn_nonce.as_deref(), Some("successor"));
            assert_eq!(run.texts().iter().filter(|t| t.contains(ANSWER)).count(), 1);
            run.close().await;
        },
    );
}

const SLOW: u64 = 1_479_671_399_000_110;

/// The pass only starts the end task: a slow end leaves the tick free for other channels.
#[test]
fn the_pass_never_waits_on_an_end_task_pg() {
    let path = concat!(module_path!(), "::the_pass_never_waits_on_an_end_task_pg");
    in_child(
        path,
        SLOW,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: SLOW,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            on("before_clear", || {
                Box::pin(tokio::time::sleep(Duration::from_secs(10)))
            });
            let started = Instant::now();
            row_end_pass(&run.shared, &run.provider).await;
            let pass = started.elapsed();
            assert!(pass < Duration::from_secs(2), "the pass took {pass:?}");
            settled_tasks().await;
            assert!(run.durable().is_none(), "the slow end still finished");
            run.close().await;
        },
    );
}

const O_OWNED: u64 = 1_479_671_399_000_111;

/// An O-owned channel shows only the body-free marker, never the recovered body, and still ends.
#[test]
fn an_o_owned_channel_shows_only_the_marker_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_channel_shows_only_the_marker_pg"
    );
    in_child(
        path,
        O_OWNED,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: O_OWNED,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.pass().await;
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            assert!(
                !run.texts().iter().any(|t| t.contains(ANSWER)),
                "{:?}",
                run.texts()
            );
            run.close().await;
        },
    );
}

const TAIL: u64 = 1_479_671_399_000_112;

/// The previous turn's tail stamped in the same second as the input mixes the read: the notice
/// falls back to the interrupt message instead of another turn's text.
#[test]
fn a_same_second_previous_tail_falls_back_to_the_notice_pg() {
    let path = concat!(
        module_path!(),
        "::a_same_second_previous_tail_falls_back_to_the_notice_pg"
    );
    in_child(
        path,
        TAIL,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: TAIL,
                    routed: true,
                },
            )
            .await;
            let tail = json!({"type": "assistant", "timestamp": stamp(&run.row),
            "message": {"content": [{"type": "text", "text": "previous turn"}]}});
            let open = std::fs::read_to_string(&run.source).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&run.source)
                .unwrap();
            file.write_all(format!("{tail}\n{open}").as_bytes())
                .unwrap();
            drop(file);
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.pass().await;
            let notice = run.interrupted();
            assert_ended(&run, &notice, &taken).await;
            assert!(
                !run.texts()
                    .iter()
                    .any(|t| t.contains(ANSWER) || t.contains("previous turn"))
            );
            run.close().await;
        },
    );
}
