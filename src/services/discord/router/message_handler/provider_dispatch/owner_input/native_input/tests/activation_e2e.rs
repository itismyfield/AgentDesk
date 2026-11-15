use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::turn_finalizer::native_prepare;
use crate::services::tui_o::{
    shadow::{ShadowProvider, SourceId},
    store::{InitSource, Initialized, OStore, StoreConfig},
    writer::{
        self,
        binding::{
            BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord,
            BindingTarget,
        },
        host::test_io::{Alarms, AnyLease, Posts},
    },
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::watch;

const TERMINAL: &str = r#"import os,sys,tty,json,termios
path,ready,strict,nonce=sys.argv[1:]
fd=sys.stdin.fileno(); tty.setraw(fd)
open(ready,'w').write(str(os.getpid()))
os.write(1,b'\x1b[?2004h')
seen=b''; number=0
while True:
    data=os.read(fd,4096)
    if not data: break
    seen+=data
    while b'\x1b[201~' in seen or b'\r' in seen:
        if b'\x1b[200~' in seen and b'\x1b[201~' not in seen: break
        marker=b'\x1b[201~' if b'\x1b[201~' in seen else b'\r'
        before,seen=seen.split(marker,1)
        frame=before.split(b'\x1b[200~')[-1].decode()
        if not frame: continue
        number+=1; ident=nonce+'-'+str(number)
        rows=[{'type':'queue-operation','operation':'enqueue','content':frame},
              {'type':'queue-operation','operation':'dequeue'},
              {'type':'user','uuid':ident,'sessionId':'fixture-native','timestamp':'2026-11-15T00:00:00Z','message':{'content':frame}},
              {'type':'assistant','apiBlockIndex':0,'uuid':'a-'+ident,'sessionId':'fixture-native','message':{'id':'a-'+ident,'role':'assistant','content':[{'type':'text','text':'body-'+ident}],'stop_reason':'end_turn'}}]
        rows.append({'type':'system','subtype':'turn_duration','durationMs':5,'timestamp':'2026-11-15T00:00:01Z'} if strict!='no' else {'type':'progress','data':{'type':'hook_progress','hookEvent':'Stop'}})
        for i in range(3):
            rows.extend([{'type':'user','uuid':ident+'-external-'+str(i),'sessionId':'fixture-native','origin':'human','promptSource':'typed','turnOrigin':'human','timestamp':'2026-11-15T00:00:02Z','message':{'content':'external-'+ident+'-'+str(i)}},
                         {'type':'assistant','apiBlockIndex':0,'uuid':'f-'+ident+'-'+str(i),'message':{'id':'f-'+ident+'-'+str(i),'content':[{'type':'text','text':'foreign-reply-'+ident+'-'+str(i)}],'stop_reason':'end_turn'}},
                         {'type':'system','subtype':'turn_duration','timestamp':'2026-11-15T00:00:03Z','durationMs':5}])
        withdrawn=strict=='withdraw' and number==1
        if withdrawn: rows=[{'type':'queue-operation','operation':'enqueue','content':frame},{'type':'queue-operation','operation':'remove','content':frame}]
        with open(path,'a') as f:
            for row in rows: f.write(json.dumps(row,ensure_ascii=False)+'\n')
            f.flush(); os.fsync(f.fileno())
        open(ready+'.row','w').write(('withdrawn-' if withdrawn else '')+ident)
"#;

struct Server {
    root: PathBuf,
    bin: PathBuf,
    child: Option<Child>,
    pids: Vec<u32>,
    epoch: u64,
}
impl Server {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env_clear();
        c.env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("HOME", self.root.join("home"))
            .env("HERDR_CONFIG_PATH", self.root.join("config.toml"))
            .env("HERDR_SOCKET_PATH", self.root.join("herdr.sock"));
        for (key, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            c.env(key, self.root.join(dir));
        }
        if self.epoch > 1 {
            c.env("HOME", self.root.join(format!("home-{}", self.epoch)));
            for (key, dir) in [
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_RUNTIME_DIR", "runtime"),
            ] {
                c.env(key, self.root.join(format!("{dir}-{}", self.epoch)));
            }
        }
        c
    }
    fn cli(&self, args: &[&str]) -> serde_json::Value {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if out.stdout.is_empty() && args.starts_with(&["pane", "run"]) {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}, stdout={:?}", out.stdout))
    }
    async fn start() -> Self {
        let root = Path::new("/Users/itismyfield/.adk/lane-runs/i8148/e2e-b2b")
            .join(uuid::Uuid::new_v4().to_string());
        for dir in ["home", "config", "state", "data", "cache", "runtime"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let bin = root.join("herdr");
        std::fs::copy("/opt/homebrew/Cellar/herdr/0.9.5/bin/herdr", &bin)
            .expect("isolated herdr 0.9.5");
        std::fs::write(root.join("config.toml"),"onboarding=false\n[update]\nversion_check=false\nmanifest_check=false\n[terminal]\ndefault_shell='/bin/sh'\nshell_mode='non_login'\n").unwrap();
        std::fs::write(root.join("terminal.py"), TERMINAL).unwrap();
        let mut s = Self {
            root,
            bin,
            child: None,
            pids: Vec::new(),
            epoch: 1,
        };
        s.spawn().await;
        s
    }
    async fn spawn(&mut self) {
        for dir in ["home", "config", "state", "data", "cache", "runtime"] {
            std::fs::create_dir_all(self.root.join(format!("{dir}-{}", self.epoch))).unwrap();
        }
        let log = std::fs::File::options()
            .append(true)
            .create(true)
            .open(self.root.join("server.log"))
            .unwrap();
        let child = self
            .command()
            .arg("server")
            .stdout(log)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        println!(
            "ISOLATED_START server {} {}",
            child.id(),
            self.root.display()
        );
        self.pids.push(child.id());
        self.child = Some(child);
        await_within(
            async {
                while std::os::unix::net::UnixStream::connect(self.root.join("herdr.sock")).is_err()
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
    }
    async fn terminal(&mut self, source: &Path, mode: &str, nonce: &str) -> String {
        let created = self.cli(&[
            "workspace",
            "create",
            "--cwd",
            self.root.to_str().unwrap(),
            "--no-focus",
        ]);
        let pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .expect("created pane id")
            .to_owned();
        let ready = self.root.join(format!("ready-{nonce}"));
        let command = format!(
            "exec /usr/bin/python3 '{}' '{}' '{}' '{}' '{}'",
            self.root.join("terminal.py").display(),
            source.display(),
            ready.display(),
            mode,
            nonce
        );
        self.cli(&["pane", "run", &pane, &command]);
        let pid = await_within(
            async {
                loop {
                    if let Ok(value) = std::fs::read_to_string(&ready)
                        && let Ok(pid) = value.parse::<u32>()
                    {
                        break pid;
                    }
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        self.pids.push(pid);
        println!("ISOLATED_START provider {pid} {nonce} {pane}");
        pane
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let _ = self.command().args(["server", "stop"]).output();
            let status = crate::test_support::wait::join_within(
                std::thread::spawn(move || child.wait()),
                WAIT_CAP,
            );
            println!("ISOLATED_STOP {pid} {status:?}");
        }
    }
    fn host(&self) -> Command {
        self.command()
    }
    fn clean(&mut self) {
        self.stop();
        let ids = self
            .pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let out = Command::new("/bin/ps")
            .args(["-p", &ids, "-o", "pid="])
            .output()
            .unwrap();
        let left = String::from_utf8(out.stdout).unwrap();
        println!(
            "ISOLATED_REMAINING count={} pids={left:?}",
            left.split_whitespace().count()
        );
        std::fs::write(
            self.root.join("pid-ledger.json"),
            serde_json::to_vec(&json!({"started":self.pids,"remaining":left})).unwrap(),
        )
        .unwrap();
        assert!(left.trim().is_empty());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Log {
    events: Mutex<Vec<BindingEvent>>,
    changed: watch::Sender<u64>,
}
impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.channel_id == channel && e.seq > after)
            .cloned()
            .collect())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}
fn bind(log: &Log, channel: u64, source: &SourceId, nonce: &str) {
    let mut events = log.events.lock().unwrap();
    let seq = events.len() as u64 + 1;
    events.push(BindingEvent {
        seq,
        channel_id: channel,
        provider: ShadowProvider::Claude,
        tmux_session: "fixture-pane".into(),
        execution_nonce: nonce.into(),
        committed_at: chrono::Utc::now(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(source.clone()),
            cause: if seq == 1 {
                BindingCause::Startup
            } else {
                BindingCause::Resume
            },
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            },
        },
    });
    log.changed.send_replace(seq);
}

async fn prepare_actor(
    shared: &Shared,
    channel: ChannelId,
    id: u64,
    nonce: &str,
    path: &Path,
) -> (TurnKey, Arc<CancelToken>) {
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(format!(
        "turn-{id}"
    ))));
    let key = TurnKey::new(channel, id, 1).with_episode_nonce(actor.turn_nonce());
    shared
        .mailbox(channel)
        .restore_active_turn(
            actor.clone(),
            poise::serenity_prelude::UserId::new(7),
            poise::serenity_prelude::MessageId::new(id),
        )
        .await;
    shared.restart.global_active.fetch_add(1, Ordering::SeqCst);
    shared
        .turn_finalizer
        .register_start(key, ProviderKind::Claude, RelayOwnerKind::None, shared);
    let mut row = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        7,
        id,
        id,
        "native fixture".into(),
        Some("fixture-native".into()),
        Some("fixture-pane".into()),
        Some(path.display().to_string()),
        None,
        0,
    );
    row.turn_nonce = actor.turn_nonce().map(str::to_owned);
    row.born_generation = 1;
    row.hosted_execution_nonce = Some(nonce.into());
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    (key, actor)
}

fn payload(source: &SourceId, nonce: &str, id: u64) -> PreparedPayload {
    PreparedPayload {
        frame: format!("input-{id}"),
        sources: vec![id.to_string()],
        attempt: json!({"nonce":format!("attempt-{id}"),"provider":"claude","target":{"host":"herdr","owner":"fixture-pane","root":source.path.parent().unwrap(),"execution_nonce":nonce},"source":{"kind":"existing","session_id":source.session_id,"transcript":source.path,"file":[source.dev,source.ino],"offset":std::fs::metadata(&source.path).unwrap().len()}}),
    }
}

fn input_leaf(mut host: Command, pane: String, source: SourceId, ready: PathBuf) -> effect::Leaf {
    Box::new(move |frame| {
        let previous = std::fs::read_to_string(&ready).unwrap_or_default();
        scope::start_payload()?;
        scope::keyed();
        let sent = host.args(["pane", "run", &pane, &frame]).output().unwrap();
        assert!(
            sent.status.success(),
            "{}",
            String::from_utf8_lossy(&sent.stderr)
        );
        let handle = tokio::runtime::Handle::current();
        let row = handle.block_on(await_within(
            async {
                loop {
                    if let Ok(row) = std::fs::read_to_string(&ready)
                        && !row.is_empty()
                        && row != previous
                    {
                        break row;
                    }
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        ));
        let (history, namespace, ticket) =
            crate::services::tui_input::human_frame::history::v2::native::scoped_ticket()?;
        let bytes = std::fs::read_to_string(&source.path).unwrap();
        if row.starts_with("withdrawn-") {
            assert!(
                bytes
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .any(|v| v["operation"] == "remove" && v["content"] == frame)
            );
            return Ok(Completion::Submitted);
        }
        let mut offset = 0;
        let mut found = None;
        for line in bytes.split_inclusive('\n') {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["uuid"] == row {
                assert_eq!(v["message"]["content"], frame);
                found = Some(offset);
                break;
            }
            offset += line.len() as u64;
        }
        assert_eq!(
            history.receipt(
                &namespace,
                &ticket.key,
                (
                    &source.path,
                    found.expect("actual user receipt"),
                    "user",
                    Some(&row),
                    &frame
                )
            ),
            Some(true)
        );
        Ok(Completion::Submitted)
    })
}

struct Writer {
    task: tokio::task::JoinHandle<Option<writer::deliver::StopCause>>,
    stop: watch::Sender<bool>,
}
impl Writer {
    fn start(
        store: &OStore,
        shared: &Shared,
        channel: u64,
        log: Arc<Log>,
        posts: Arc<Posts>,
    ) -> Self {
        let era = store.read_era().unwrap().unwrap();
        let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
        gate.acquired();
        let writer = writer::deliver::ChannelWriter::new(
            store.open_channel(&era, channel).unwrap().unwrap(),
            gate,
            posts,
            AnyLease,
            Alarms::default(),
        );
        let (stop, rx) = watch::channel(false);
        let port = native_prepare::enabled()
            .then(|| native_prepare::supply_port(channel, shared.clone(), log.clone()));
        let task = writer::actor::native_input::spawn_native(
            &writer::WriterConfig {
                enabled: true,
                producer_absent: false,
            },
            None,
            writer,
            ShadowProvider::Claude,
            log,
            (
                rx,
                watch::channel(false).0,
                watch::channel(None).0,
                writer::actor::Owing::default(),
            ),
            port,
        )
        .unwrap();
        Self { task, stop }
    }
    async fn stop(self) {
        self.stop.send_replace(true);
        assert!(await_within(self.task, WAIT_CAP).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn native_8148_activation_isolated_herdr_strict_resume_and_event_wake_pg() {
    isolated("strict").await;
}
#[tokio::test]
async fn native_8148_activation_isolated_herdr_stop_only_preserves_original_pg() {
    isolated("stop").await;
}

#[tokio::test]
async fn native_8148_activation_isolated_herdr_withdraw_event_wake_pg() {
    isolated("withdraw").await;
}

async fn isolated(kind: &str) {
    let strict = kind != "stop";
    let withdrawn = kind == "withdraw";
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let channel = ChannelId::new(if withdrawn {
        81489933
    } else if strict {
        81489931
    } else {
        81489932
    });
    let mut server = Server::start().await;
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        &server.root,
    );
    let path = server.root.join("transcript.jsonl");
    std::fs::write(&path, "").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(&meta);
    let source = SourceId {
        session_id: "fixture-native".into(),
        path: path.clone(),
        dev,
        ino,
    };
    let pane = server
        .terminal(
            &path,
            if withdrawn {
                "withdraw"
            } else if strict {
                "yes"
            } else {
                "no"
            },
            "N1",
        )
        .await;
    let log = Arc::new(Log {
        events: Mutex::default(),
        changed: watch::channel(0).0,
    });
    bind(&log, channel.get(), &source, "N1");
    std::fs::create_dir_all(server.root.join("o-store")).unwrap();
    let store =
        OStore::open_if_enabled(&StoreConfig { enabled: true }, &server.root.join("o-store"))
            .unwrap()
            .unwrap();
    use sha2::Digest;
    store
        .begin_fixture_era(&[channel.get()], chrono::Utc::now(), |c| {
            Ok(Initialized {
                channel: c,
                sources: vec![InitSource {
                    source_id: source.clone(),
                    delivery_start: 0,
                    prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
                }],
                initial_anchor: 100,
                build_digest: "native-activation".into(),
                at: chrono::Utc::now(),
                policy_parent: None,
            })
        })
        .unwrap();
    let posts = Arc::new(Posts::default());
    let mailbox = shared.mailbox(channel);
    mailbox
        .v2_adopt(
            server.root.join("runtime/owner.json"),
            "isolated-native".into(),
        )
        .await
        .unwrap();
    let first = channel.get() + 100;
    let next = first + 1;
    {
        let id = first;
        let item=serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":format!("input-{id}")})).unwrap();
        mailbox
            .v2_admit_ticket(item, format!("original-{id}"))
            .await
            .unwrap();
    }
    let (key, actor) = prepare_actor(&shared, channel, first, "N1", &path).await;
    let first_row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        channel.get(),
    )
    .unwrap();
    let producer = Producer::at(shared.clone(), key, Some(actor.clone()));
    let intent = producer.intent.clone();
    let input = input_leaf(
        server.host(),
        pane.clone(),
        source.clone(),
        server.root.join("ready-N1.row"),
    );
    let result = pump::submit_native_front(
        &mailbox,
        None,
        payload(&source, "N1", first),
        intent,
        Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
        producer.wrap(input),
    )
    .await
    .unwrap();
    assert_eq!(result, Completion::Submitted);
    // The event listener reaches the real owner wake; only the provider handoff is a fixture.
    let (rest, _rest_guard) =
        crate::services::discord::shared_state::test_rest::recording_mock(900, channel.get()).await;
    shared
        .http
        .cached_bot_token
        .set("fixture-token".into())
        .unwrap();
    shared.settings.write().await.allowed_channel_ids = vec![channel.get()];
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let hand_mailbox = mailbox.clone();
    let (sent, received) = tokio::sync::oneshot::channel();
    let sent = Arc::new(Mutex::new(Some(sent)));
    let hand_shared = shared.clone();
    let hand_source = source.clone();
    let hand_pane = pane.clone();
    let hand_root = server.root.clone();
    let driven = Arc::new(
        crate::services::discord::queue_io::input_owner::drive::Drive {
            mailbox: mailbox.clone(),
            channel: channel.get(),
            settled: None,
            handoff: Arc::new(move |front| {
                let seen = seen.clone();
                let sent = sent.clone();
                let mailbox = hand_mailbox.clone();
                let shared = hand_shared.clone();
                let source = hand_source.clone();
                let pane = hand_pane.clone();
                let root = hand_root.clone();
                Box::pin(async move {
                    assert_eq!(front.1.message_id, next);
                    let claim = crate::services::discord::queue_io::input_owner::drive::claim_turn(
                        &mailbox,
                        front.1.message_id,
                        &front,
                    )
                    .await?
                    .ok_or("claim")?;
                    let (key, actor) =
                        prepare_actor(&shared, channel, next, "N1", &source.path).await;
                    let producer = Producer::at(shared.clone(), key, Some(actor));
                    let intent = producer.intent.clone();
                    let endpoint = Server {
                        root: root.clone(),
                        bin: root.join("herdr"),
                        child: None,
                        pids: Vec::new(),
                        epoch: 1,
                    };
                    let host = endpoint.command();
                    let input = input_leaf(host, pane, source.clone(), root.join("ready-N1.row"));
                    assert_eq!(
                        pump::submit_native_front(
                            &mailbox,
                            Some(claim),
                            payload(&source, "N1", next),
                            intent,
                            Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
                            producer.wrap(input)
                        )
                        .await?,
                        Completion::Submitted
                    );
                    seen.fetch_add(1, Ordering::SeqCst);
                    if let Some(tx) = sent.lock().unwrap().take() {
                        let _ = tx.send(key);
                    }
                    Ok(())
                })
            }),
        },
    );
    let wake = driven.clone();
    crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            crate::services::discord::queue_io::input_owner::drive::wake(wake.clone())
        })),
    );
    let listening=crate::services::discord::queue_io::cancel_backstop_test_support::KickoffCompletion::install(channel);
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        shared.clone(),
        ProviderKind::Claude,
    );
    listening.listener_ready(WAIT_CAP).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "startup snapshot must not inject through the native row barrier"
    );
    let item=serde_json::from_value(json!({"author_id":7,"message_id":next,"channel_id":channel.get(),"source_message_ids":[next],"queued_generation":1,"text":format!("input-{next}")})).unwrap();
    mailbox
        .v2_admit_ticket(item, format!("original-{next}"))
        .await
        .unwrap();
    let writer = Writer::start(&store, &shared, channel.get(), log.clone(), posts.clone());
    if strict {
        let _next_key = await_within(received, WAIT_CAP / 2).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        listening.processed(1, WAIT_CAP).await;
        await_within(
            async {
                while shared.restart.global_active.load(Ordering::SeqCst) != 0 {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
        assert!(
            shared
                .turn_finalizer
                .native_settled(key, &actor)
                .await
                .unwrap()
        );
    } else {
        await_within(
            async {
                while !posts
                    .to(channel.get())
                    .iter()
                    .any(|p| p.contains("body-N1-1"))
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
        assert!(
            !shared
                .turn_finalizer
                .native_settled(key, &actor)
                .await
                .unwrap()
        );
    }
    let last_body = if strict { "body-N1-2" } else { "body-N1-1" };
    await_within(
        async {
            while !posts
                .to(channel.get())
                .iter()
                .any(|p| p.contains(last_body))
            {
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;
    let mut foreign = (0..3)
        .map(|i| format!("foreign-reply-N1-1-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        crate::services::discord::turn_bridge::native_input::reject_legacy_body(
            &shared,
            &ProviderKind::Claude,
            &first_row,
            &actor,
            &mut foreign
        )
        .await
    );
    assert!(foreign.is_empty());
    assert!(
        crate::services::discord::turn_bridge::native_input::neutral_exit(
            &shared,
            &ProviderKind::Claude,
            &first_row,
            &actor
        )
        .await,
        "native pending and settled episodes both leave the bridge neutrally"
    );
    let own: Vec<&str> = if !strict {
        vec!["N1-1"]
    } else if withdrawn {
        vec!["N1-2"]
    } else {
        vec!["N1-1", "N1-2"]
    };
    let mut units = Vec::new();
    for id in &own {
        units.push(format!("a-{id}"));
        for i in 0..3 {
            units.push(format!("f-{id}-{i}"));
        }
    }
    activation_units(&store, &posts, channel.get(), &units).await;
    if withdrawn {
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE target=$1 AND source='retained_input_notice'").bind(format!("channel:{channel}")).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
    writer.stop().await;
    if kind == "strict" {
        crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
            channel,
            Some(Arc::new(|| {})),
        );
        server.stop();
        server.epoch = 2;
        server.spawn().await;
        let resumed = server.terminal(&path, "yes", "N2").await;
        assert_eq!(
            resumed, pane,
            "the isolated replacement reuses the pane number"
        );
        bind(&log, channel.get(), &source, "N2");
        let third = next + 1;
        let item=serde_json::from_value(json!({"author_id":7,"message_id":third,"channel_id":channel.get(),"source_message_ids":[third],"queued_generation":1,"text":format!("input-{third}")})).unwrap();
        mailbox
            .v2_admit_ticket(item, format!("original-{third}"))
            .await
            .unwrap();
        let (new_key, new_actor) = prepare_actor(&shared, channel, third, "N2", &path).await;
        let before = serde_json::to_value(
            crate::services::discord::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                channel.get(),
            )
            .unwrap(),
        )
        .unwrap();
        shared
            .turn_finalizer
            .submit_terminal(
                key,
                ProviderKind::Claude,
                crate::services::discord::turn_finalizer::TerminalEvent::GateTimeout {
                    pane_quiescent: Some(true),
                },
                crate::services::discord::turn_finalizer::FinalizeContext::bridge(),
                shared.clone(),
            )
            .await;
        assert_eq!(
            serde_json::to_value(
                crate::services::discord::inflight::load_inflight_state_read_only(
                    &ProviderKind::Claude,
                    channel.get()
                )
                .unwrap()
            )
            .unwrap(),
            before
        );
        assert!(Arc::ptr_eq(
            mailbox.snapshot().await.cancel_token.as_ref().unwrap(),
            &new_actor
        ));
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
        let producer = Producer::at(shared.clone(), new_key, Some(new_actor));
        let intent = producer.intent.clone();
        assert_eq!(
            pump::submit_native_front(
                &mailbox,
                None,
                payload(&source, "N2", third),
                intent,
                Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
                producer.wrap(input_leaf(
                    server.host(),
                    resumed,
                    source.clone(),
                    server.root.join("ready-N2.row")
                ))
            )
            .await
            .unwrap(),
            Completion::Submitted
        );
        let resumed_writer =
            Writer::start(&store, &shared, channel.get(), log.clone(), posts.clone());
        await_within(
            async {
                while shared.restart.global_active.load(Ordering::SeqCst) != 0 {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        units.push("a-N2-1".into());
        for i in 0..3 {
            units.push(format!("f-N2-1-{i}"));
        }
        activation_units(&store, &posts, channel.get(), &units).await;
        assert_eq!(
            posts
                .to(channel.get())
                .iter()
                .filter(|p| p.contains("body-N2-1"))
                .count(),
            1
        );
        assert_eq!(
            posts
                .to(channel.get())
                .iter()
                .filter(|p| p.contains("body-N1-1"))
                .count(),
            1
        );
        assert_eq!(
            posts
                .to(channel.get())
                .iter()
                .filter(|p| p.contains("body-N1-2"))
                .count(),
            1
        );
        resumed_writer.stop().await;
    }
    println!(
        "ISOLATED_NATIVE kind={kind} posts={:?} next={}",
        posts.to(channel.get()),
        count.load(Ordering::SeqCst)
    );
    crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
        channel, None,
    );
    assert!(
        rest.lock().unwrap().is_empty(),
        "no REST sends outside O MockPort"
    );
    drop(driven);
    server.clean();
    pool.close().await;
    db.drop().await;
}
