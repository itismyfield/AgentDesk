//! Installed reader policy through the real host, with one BOOT per child process.

use super::*;
use crate::services::tui_o::channel_policy;
use crate::services::tui_o::store::fault::{self, Keep, Step as FaultStep};
use crate::services::tui_o::store::rotation::{Boundary, Rotation, SourceLink, Successor};
use crate::services::tui_o::store::spool::source_key;
use sha2::Digest;
use std::time::{Duration, SystemTime};

fn child_case(name: &str) -> bool {
    const CHILD: &str = "ADK_TEST_READERS_HOST_BOOT_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let qualified = name.split_once("::").unwrap().1;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture"])
        .env(CHILD, name)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    false
}

fn install_boot(on: bool) {
    let mut writer = serde_json::json!({"channels": [CHANNEL]});
    if on {
        writer["dormant_channels"] = serde_json::json!([CHANNEL]);
    }
    let config = serde_json::from_value(serde_json::json!({
        "server": {}, "tui_o": {"writer": writer},
        "agents": [{"id": "readers", "name": "Readers", "channels": {
            "claude": {"id": CHANNEL.to_string(), "runtime": "tui"}
        }}]
    }))
    .unwrap();
    channel_policy::install(&config).unwrap();
    crate::config_live_reload::install(config);
    assert!(
        channel_policy::boot()
            .unwrap()
            .selected()
            .contains(&CHANNEL)
    );
}

/// Counts this fixture's inode, so other tests' readers cannot satisfy the assertion.
fn inode_fds(source: &SourceId) -> usize {
    fn identity<D: Into<i128>, I: Into<i128>>(dev: D, ino: I) -> (u64, u64) {
        (dev.into() as u64, ino.into() as u64)
    }
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::c_int>().ok())
        .filter(|fd| {
            // macOS /dev/fd path metadata reports fdescfs dev, not the opened file's dev.
            // fstat borrows the descriptor and never opens, closes or changes its offset.
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(*fd, stat.as_mut_ptr()) } != 0 {
                return false;
            }
            let stat = unsafe { stat.assume_init() };
            identity(stat.st_dev, stat.st_ino) == (source.dev, source.ino)
        })
        .count()
}

fn logged(seq: u64, old: Option<&SourceId>, new: &SourceId) -> Vec<u8> {
    logged_on(CHANNEL, seq, old, new)
}

fn logged_on(channel: u64, seq: u64, old: Option<&SourceId>, new: &SourceId) -> Vec<u8> {
    let event = p5::BindingEvent {
        seq,
        channel_id: channel,
        provider: "claude".into(),
        tmux_session: "readers-host".into(),
        execution_nonce: None,
        old: old.cloned(),
        new: p5::BindingTarget::Source(new.clone()),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: None,
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    };
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    bytes
}

struct ReaderHost {
    h: Harness,
    old: SourceId,
    latest: SourceId,
    io: Arc<TestIo>,
    ready: Arc<Readiness>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _selected: test_override::ChannelsGuard,
}

impl ReaderHost {
    fn legacy(on: bool, pending: bool) -> Self {
        let mut ids = Vec::new();
        let h = Harness::build(|root| {
            [
                ("old", row("before", "before the switch")),
                ("latest", Vec::new()),
            ]
            .into_iter()
            .map(|(session, body)| {
                let path = root.join(format!("{session}.jsonl"));
                std::fs::write(&path, &body).unwrap();
                let source_id = source_id_for(session, &path).unwrap();
                ids.push(source_id.clone());
                InitSource {
                    source_id,
                    delivery_start: body.len() as u64,
                    prefix_hash: hex::encode(sha2::Sha256::digest(&body)),
                }
            })
            .collect()
        });
        let old = ids.remove(0);
        let latest = ids.remove(0);
        std::fs::File::open(&old.path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
            .unwrap();
        p5_log(
            h._runtime.path(),
            CHANNEL,
            &[logged(1, None, &old), logged(2, Some(&old), &latest)].concat(),
        );
        let mut store = h.channel();
        let mut rotation = Rotation::default();
        rotation.successors.insert(
            source_key(&old),
            Successor {
                source: latest.clone(),
                seq: Some(2),
                tmux_session: Some("readers-host".into()),
                drain_to: Some(store.cursor(&old).unwrap().captured_through),
                proof: None,
            },
        );
        if pending {
            rotation.links.insert(
                source_key(&old),
                SourceLink {
                    source: old.clone(),
                    seq: 1,
                    parent: None,
                    hot_candidate: None,
                    committed_at: Utc::now(),
                    boundary: Boundary::Undecided,
                },
            );
        }
        store.write_rotation(&rotation).unwrap();
        store.set_binding_checkpoint(2).unwrap();
        drop(store);
        let selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
        install_boot(on);
        let io = TestIo::over(&h);
        let ready = Arc::new(Readiness::default());
        let scene = Self {
            h,
            old,
            latest,
            io,
            ready,
            tasks: Vec::new(),
            _selected: selected,
        };
        assert!(
            !scene.witness_dir().exists(),
            "fixture is an old store without witness storage"
        );
        let probe = std::fs::File::open(&scene.old.path).unwrap();
        assert_eq!(
            inode_fds(&scene.old),
            1,
            "FD probe sees this fixture's owned inode"
        );
        drop(probe);
        assert_eq!(inode_fds(&scene.old), 0, "FD probe sees its actual close");
        scene.h.gate.acquired();
        scene
    }

    fn witness_dir(&self) -> PathBuf {
        root(&self.h)
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cleanup_witness")
    }

    fn launch(&mut self) {
        assert!(self.tasks.is_empty());
        self.tasks = hosted(&self.h, &self.io, true, &self.ready);
        assert_eq!(self.tasks.len(), 1, "a real selected host was launched");
    }

    async fn wait(&self, reason: &str, satisfied: impl Fn() -> bool) {
        for _ in 0..180 {
            if satisfied() {
                return;
            }
            polls(1).await;
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "{reason}; ready={}; accepts={}; old_fds={}; latest_fds={}; calls={:?}; posts={:?}; alarms={:?}",
            self.ready.is_ready(CHANNEL),
            self.ready.accepts(CHANNEL),
            inode_fds(&self.old),
            inode_fds(&self.latest),
            self.io.calls(),
            self.h.port.posts(),
            self.io.alarms.0.lock().unwrap()
        );
    }

    async fn policy(&self, on: bool) {
        self.wait("actor ready and old FD matches installed BOOT", || {
            self.ready.accepts(CHANNEL)
                && inode_fds(&self.latest) > 0
                && if on {
                    inode_fds(&self.old) == 0 && self.witness_dir().is_dir()
                } else {
                    inode_fds(&self.old) > 0
                }
        })
        .await;
        let store = self.h.channel();
        assert_eq!(store.cursors().count(), 2);
        assert!(!store.cursor(&self.old).unwrap().retired);
        assert_eq!(
            store.rotation().unwrap().successors[&source_key(&self.old)].proof,
            None
        );
        if on {
            let witness = store.cleanup_witness(&self.old).unwrap().unwrap();
            assert!(witness.bound_to(store.cursor(&self.old).unwrap()));
        } else {
            assert!(!self.witness_dir().exists());
        }
    }

    async fn validated_reload_while_hosted(&self, on: bool) {
        use crate::config_live_reload::{self as live, ReloadOutcome};
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("agentdesk.yaml");
        let write = |dormant: bool| {
            let members = if dormant {
                format!("[{CHANNEL}]")
            } else {
                "[]".into()
            };
            std::fs::write(&path, format!(
                "server: {{}}\nagents:\n  - id: readers\n    name: Readers\n    channels:\n      claude: {{id: '{CHANNEL}', runtime: tui}}\ntui_o:\n  writer:\n    channels: [{CHANNEL}]\n    dormant_channels: {members}\n"
            )).unwrap();
        };
        let boot = channel_policy::boot().unwrap();
        let installed = boot.clone();
        write(!on);
        let candidate = crate::config::load_from_path(&path).unwrap();
        channel_policy::BootChannels::validate(&candidate).unwrap();
        let expected = if on {
            None
        } else {
            Some(serde_json::json!([CHANNEL]))
        };
        for attempt in 0..2 {
            assert!(
                matches!(live::reload_from_path(&path),
                ReloadOutcome::Applied { restart_required }
                    if restart_required.contains(&"tui_o.writer.dormant_channels")),
                "every opposite reload compares against the installed BOOT"
            );
            let published = serde_json::to_value(&*live::current().unwrap()).unwrap();
            assert_eq!(
                published.pointer("/tui_o/writer/dormant_channels"),
                expected.as_ref()
            );
            assert!(std::ptr::eq(channel_policy::boot().unwrap(), boot));
            assert_eq!(channel_policy::boot().unwrap(), &installed);
            // Cross the next 30-second quiet boundary while the live policy is opposite.
            polls(if attempt == 0 { 35 } else { 2 }).await;
            self.policy(on).await;
            assert_eq!(
                self.io.leases.lock().unwrap().len(),
                1,
                "reload neither replaced nor silently restarted this actor"
            );
        }
        append(
            &self.latest.path,
            &row("reload", "latest during validated reload"),
        );
        self.wait(
            "actual latest POST while live policy differs from BOOT",
            || self.posted("reload"),
        )
        .await;
        assert_eq!(
            self.h.port.posts(),
            [
                "latest runs",
                "old late tail",
                "latest during validated reload"
            ]
        );
        write(on);
        assert!(
            matches!(live::reload_from_path(&path),
            ReloadOutcome::Applied { restart_required }
                if !restart_required.contains(&"tui_o.writer.dormant_channels")),
            "returning to installed reader policy clears restart-required"
        );
        let published = serde_json::to_value(&*live::current().unwrap()).unwrap();
        let restored = if on {
            Some(serde_json::json!([CHANNEL]))
        } else {
            None
        };
        assert_eq!(
            published.pointer("/tui_o/writer/dormant_channels"),
            restored.as_ref()
        );
        assert!(std::ptr::eq(channel_policy::boot().unwrap(), boot));
        assert_eq!(channel_policy::boot().unwrap(), &installed);
        self.policy(on).await;
    }

    fn posted(&self, id: &str) -> bool {
        self.h
            .channel()
            .ledger()
            .latest_piece(&unit(&format!("{id}:0")), 0)
            .is_some_and(|(_, piece)| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
    }

    async fn shutdown(&mut self) {
        abort(std::mem::take(&mut self.tasks));
        self.wait("last writer and both source FDs ended", || {
            Arc::strong_count(&self.h.lease) == 2
                && inode_fds(&self.old) == 0
                && inode_fds(&self.latest) == 0
        })
        .await;
        self.ready = Arc::new(Readiness::default());
    }
}

impl Drop for ReaderHost {
    fn drop(&mut self) {
        abort(std::mem::take(&mut self.tasks));
    }
}

async fn policy_and_restart(on: bool) {
    let mut scene = ReaderHost::legacy(on, false);
    scene.launch();
    scene.policy(on).await;
    append(&scene.latest.path, &row("fresh", "latest runs"));
    scene
        .wait("latest POST proves the actor actually captured", || {
            scene.posted("fresh")
        })
        .await;
    assert_eq!(scene.h.port.posts(), ["latest runs"]);
    let before = scene.h.channel().cursor(&scene.old).unwrap().clone();
    let mut partial = row("late", "old late tail");
    assert_eq!(partial.pop(), Some(b'\n'));
    append(&scene.old.path, &partial);
    scene
        .wait("late partial reopened the old source", || {
            inode_fds(&scene.old) > 0
        })
        .await;
    polls(2).await;
    assert_eq!(scene.h.channel().cursor(&scene.old).unwrap(), &before);
    assert!(!scene.posted("late"));
    assert_eq!(scene.h.port.posts(), ["latest runs"]);
    append(&scene.old.path, b"\n");
    scene
        .wait("completed old tail posts once", || scene.posted("late"))
        .await;
    assert_eq!(scene.h.port.posts(), ["latest runs", "old late tail"]);
    assert_eq!(
        scene
            .h
            .channel()
            .cursor(&scene.old)
            .unwrap()
            .captured_through,
        std::fs::metadata(&scene.old.path).unwrap().len()
    );
    assert!(
        inode_fds(&scene.old) > 0,
        "late tail is active before the next quiet transition"
    );
    scene.validated_reload_while_hosted(on).await;
    scene.policy(on).await;
    let full = fault::plant(
        &root(&scene.h)
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("spool"),
        FaultStep::Append(Keep::Nothing),
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    append(
        &scene.latest.path,
        &row("restart", "after transient restart"),
    );
    scene
        .wait("host observed a real transient halt", || {
            !scene.io.alarms.halted().is_empty()
        })
        .await;
    drop(full);
    scene
        .wait("a second actor delivered after recovery", || {
            scene.io.leases.lock().unwrap().len() >= 2 && scene.posted("restart")
        })
        .await;
    scene.policy(on).await;
    assert!(
        scene
            .io
            .leases
            .lock()
            .unwrap()
            .iter()
            .all(|(_, writers, _)| *writers == 0)
    );
    assert_eq!(
        scene.h.port.posts(),
        [
            "latest runs",
            "old late tail",
            "latest during validated reload",
            "after transient restart"
        ]
    );
    scene.shutdown().await;
    scene.launch();
    scene.policy(on).await;
    assert_eq!(
        scene.io.leases.lock().unwrap().len(),
        3,
        "initial start, transient restart and external restart each started a writer"
    );
    polls(3).await;
    assert_eq!(
        scene.h.port.posts().len(),
        4,
        "external restart never reposts delivered rows"
    );
    scene.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_default_off_keeps_fds_across_both_restarts() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_default_off_keeps_fds_across_both_restarts"
    )) {
        return;
    }
    policy_and_restart(false).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_installed_on_closes_fds_across_both_restarts() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_installed_on_closes_fds_across_both_restarts"
    )) {
        return;
    }
    policy_and_restart(true).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_on_restores_init_only_eof_pending_and_posts_latest() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_on_restores_init_only_eof_pending_and_posts_latest"
    )) {
        return;
    }
    let mut scene = ReaderHost::legacy(true, true);
    let old_end = scene
        .h
        .channel()
        .cursor(&scene.old)
        .unwrap()
        .captured_through;
    assert!(
        !root(&scene.h)
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cursor")
            .join(format!("{}.json", source_key(&scene.old)))
            .exists()
    );
    scene.launch();
    scene.policy(true).await;
    assert_eq!(
        scene
            .h
            .channel()
            .rotation()
            .unwrap()
            .link(&scene.old)
            .unwrap()
            .boundary,
        Boundary::Pending {
            candidates: vec![0, old_end]
        }
    );
    assert!(scene.io.alarms.has(
        CHANNEL,
        &WriterAlarm::BoundaryPending {
            source: scene.old.clone()
        }
    ));
    append(
        &scene.latest.path,
        &row("fresh", "latest while old pending"),
    );
    scene
        .wait(
            "latest still delivers while old boundary is pending",
            || scene.posted("fresh"),
        )
        .await;
    assert_eq!(scene.h.port.posts(), ["latest while old pending"]);
    scene.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_on_real_unlink_retires_one_and_restart_keeps_it_retired() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_on_real_unlink_retires_one_and_restart_keeps_it_retired"
    )) {
        return;
    }
    let mut scene = ReaderHost::legacy(true, false);
    scene.launch();
    scene.policy(true).await;
    let before = scene
        .h
        .channel()
        .cursors()
        .filter(|cursor| cursor.retired)
        .count();
    assert_eq!(before, 0, "cleanup fixture starts with no retired sources");
    std::fs::remove_file(&scene.old.path).unwrap();
    scene
        .wait("real unlink commits exactly one cleanup retirement", || {
            scene.h.channel().cursor(&scene.old).unwrap().retired
        })
        .await;
    assert_eq!(
        scene
            .h
            .channel()
            .cursors()
            .filter(|cursor| cursor.retired)
            .count(),
        before + 1
    );
    assert!(scene.io.alarms.0.lock().unwrap().iter().any(|(channel, alarm)|
        *channel == CHANNEL && matches!(alarm, WriterAlarm::CleanupRetired { source, .. } if source == &scene.old)));
    append(&scene.latest.path, &row("fresh", "latest after cleanup"));
    scene
        .wait("cleanup leaves actual latest delivery running", || {
            scene.posted("fresh")
        })
        .await;
    assert_eq!(scene.h.port.posts(), ["latest after cleanup"]);
    scene.shutdown().await;
    scene.launch();
    scene
        .wait("actor resumed with retired missing source", || {
            scene.ready.accepts(CHANNEL)
        })
        .await;
    assert!(scene.h.channel().cursor(&scene.old).unwrap().retired);
    assert_eq!(inode_fds(&scene.old), 0);
    assert_eq!(scene.h.channel().cursors().count(), 2);
    append(
        &scene.latest.path,
        &row("following", "after cleanup restart"),
    );
    scene
        .wait(
            "retired missing source does not block restart delivery",
            || scene.posted("following"),
        )
        .await;
    assert_eq!(
        scene.h.port.posts(),
        ["latest after cleanup", "after cleanup restart"]
    );
    scene.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_host_dormant_membership_is_per_channel() {
    if !child_case(concat!(
        module_path!(),
        "::readers_s1b_host_dormant_membership_is_per_channel"
    )) {
        return;
    }
    fn fixture(channel: u64, name: &str) -> (Harness, SourceId, SourceId) {
        let mut ids = Vec::new();
        let h = Harness::build_channel(channel, |root| {
            [
                ("old", row(&format!("{name}-before"), "before the switch")),
                ("latest", Vec::new()),
            ]
            .into_iter()
            .map(|(kind, body)| {
                let session = format!("{name}-{kind}");
                let path = root.join(format!("{session}.jsonl"));
                std::fs::write(&path, &body).unwrap();
                let source_id = source_id_for(&session, &path).unwrap();
                ids.push(source_id.clone());
                InitSource {
                    source_id,
                    delivery_start: body.len() as u64,
                    prefix_hash: hex::encode(sha2::Sha256::digest(&body)),
                }
            })
            .collect()
        });
        let (old, latest) = (ids.remove(0), ids.remove(0));
        let mut channel_store = h.channel();
        let mut rotation = Rotation::default();
        rotation.successors.insert(
            source_key(&old),
            Successor {
                source: latest.clone(),
                seq: Some(2),
                tmux_session: Some("readers-host".into()),
                drain_to: Some(channel_store.cursor(&old).unwrap().captured_through),
                proof: None,
            },
        );
        channel_store.write_rotation(&rotation).unwrap();
        channel_store.set_binding_checkpoint(2).unwrap();
        drop(channel_store);
        (h, old, latest)
    }
    fn posted(channel: &ChannelStore, id: &str, text: &str) -> bool {
        let key = UnitKey {
            channel_id: channel.init().channel,
            ..unit(&format!("{id}:0"))
        };
        channel
            .ledger()
            .latest_piece(&key, 0)
            .is_some_and(|(_, piece)| {
                piece.payload == text && matches!(piece.outcome, Some(PieceOutcome::Posted(_)))
            })
    }
    let (a, a_old, a_latest) = fixture(CHANNEL, "a");
    let (b, b_old, b_latest) = fixture(OTHER, "b");
    assert_ne!((a_old.dev, a_old.ino), (b_old.dev, b_old.ino));
    let stores = root(&a).join("o_store");
    copy_dir(
        &root(&b).join("o_store").join(OTHER.to_string()),
        &stores.join(OTHER.to_string()),
    );
    // A sealed era may open a later channel with its own init; keep B's source files alive.
    let era = a.store.read_era().unwrap().unwrap();
    let channel_b = || a.store.open_channel(&era, OTHER).unwrap().unwrap();
    for (channel, old, latest) in [(CHANNEL, &a_old, &a_latest), (OTHER, &b_old, &b_latest)] {
        p5_log(
            a._runtime.path(),
            channel,
            &[
                logged_on(channel, 1, None, old),
                logged_on(channel, 2, Some(old), latest),
            ]
            .concat(),
        );
        assert!(
            !stores
                .join(channel.to_string())
                .join("cleanup_witness")
                .exists()
        );
        let probe = std::fs::File::open(&old.path).unwrap();
        assert_eq!(inode_fds(old), 1);
        drop(probe);
        assert_eq!(inode_fds(old), 0);
    }
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui), (OTHER, ClaudeTui)]);
    let config = serde_json::from_value(serde_json::json!({
        "server": {}, "tui_o": {"writer": {
            "channels": [CHANNEL, OTHER], "dormant_channels": [CHANNEL]
        }}, "agents": [
            {"id": "a", "name": "A", "channels": {"claude": {"id": CHANNEL.to_string(), "runtime": "tui"}}},
            {"id": "b", "name": "B", "channels": {"claude": {"id": OTHER.to_string(), "runtime": "tui"}}}
        ]
    })).unwrap();
    channel_policy::install(&config).unwrap();
    crate::config_live_reload::install(config);
    let boot = channel_policy::boot().unwrap();
    assert_eq!(
        boot.selected().iter().copied().collect::<Vec<_>>(),
        [CHANNEL, OTHER]
    );
    assert_eq!(
        boot.dormant_channels().iter().copied().collect::<Vec<_>>(),
        [CHANNEL]
    );
    let (io, ready) = (TestIo::over(&a), Arc::new(Readiness::default()));
    a.gate.acquired();
    let tasks = hosted(&a, &io, true, &ready);
    assert_eq!(
        tasks.len(),
        2,
        "one public host call starts both selected actors"
    );
    for _ in 0..180 {
        if ready.accepts(CHANNEL)
            && ready.accepts(OTHER)
            && inode_fds(&a_old) == 0
            && inode_fds(&b_old) > 0
            && inode_fds(&a_latest) > 0
            && inode_fds(&b_latest) > 0
        {
            break;
        }
        polls(1).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        ready.accepts(CHANNEL) && ready.accepts(OTHER),
        "both actors really resumed"
    );
    assert_eq!(
        io.leases.lock().unwrap().len(),
        2,
        "both actors own a writer"
    );
    assert_eq!(inode_fds(&a_old), 0, "A is selected for Dormant");
    assert!(
        inode_fds(&b_old) > 0,
        "B must stay OFF even while the Dormant set is nonempty"
    );
    assert!(inode_fds(&a_latest) > 0 && inode_fds(&b_latest) > 0);
    let a_store = a.channel();
    let witness = a_store.cleanup_witness(&a_old).unwrap().unwrap();
    assert!(witness.bound_to(a_store.cursor(&a_old).unwrap()));
    assert!(
        !stores
            .join(OTHER.to_string())
            .join("cleanup_witness")
            .exists()
    );
    assert_eq!(a_store.cursors().count(), 2);
    assert_eq!(channel_b().cursors().count(), 2);
    assert!(!a_store.cursor(&a_old).unwrap().retired);
    assert!(!channel_b().cursor(&b_old).unwrap().retired);
    drop(a_store);
    append(&a_latest.path, &row("a-fresh", "A latest delivered"));
    append(&b_latest.path, &row("b-fresh", "B latest delivered"));
    for _ in 0..180 {
        if posted(&a.channel(), "a-fresh", "A latest delivered")
            && posted(&channel_b(), "b-fresh", "B latest delivered")
        {
            break;
        }
        polls(1).await;
    }
    assert!(posted(&a.channel(), "a-fresh", "A latest delivered"));
    assert!(posted(&channel_b(), "b-fresh", "B latest delivered"));
    let mut bodies = a.port.posts();
    bodies.sort();
    assert_eq!(bodies, ["A latest delivered", "B latest delivered"]);
    assert_eq!(inode_fds(&a_old), 0);
    assert!(inode_fds(&b_old) > 0);
    assert!(
        !stores
            .join(OTHER.to_string())
            .join("cleanup_witness")
            .exists()
    );
    abort(tasks);
    for _ in 0..180 {
        if Arc::strong_count(&a.lease) == 2 {
            break;
        }
        polls(1).await;
    }
    assert_eq!(Arc::strong_count(&a.lease), 2, "both writers ended");
    assert_eq!(inode_fds(&a_latest), 0);
    assert_eq!(inode_fds(&b_latest), 0);
    assert_eq!(inode_fds(&b_old), 0);
}
