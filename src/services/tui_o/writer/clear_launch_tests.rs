use super::*;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::writer::binding::BindingLog;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*};

fn binding(path: &Path, session: &str) -> dedupe::TuiRuntimeBinding {
    dedupe::TuiRuntimeBinding {
        runtime_kind: ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

fn context(pane: &ProducerPane, session: &str, mode: &str) -> BindingContext {
    BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: pane.tmux.into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: pane._env.0.path().display().to_string(),
        host: None,
        expected_native_session_id: Some(session.into()),
        launch_mode: mode.into(),
        provider_root: None,
    }
}

fn publish(context: BindingContext) {
    let prepared = PreparedIncarnation::create(context).unwrap();
    std::fs::write(
        tc::session_temp_path(&prepared.context.tmux_session, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
}

fn observed_context_path(tmux: &str) -> Option<PathBuf> {
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(tmux) else {
        return None;
    };
    let path = crate::config::runtime_root()?
        .join("runtime/binding_contexts/claude")
        .join(format!("{nonce}.json"));
    path.exists().then_some(path)
}

fn register(tmux: &str, channel: u64, binding: dedupe::TuiRuntimeBinding) -> bool {
    let path = observed_context_path(tmux);
    dedupe::pane_registration::register_launched_claude_pane(
        tmux,
        channel,
        binding,
        path.as_deref(),
    )
}

fn launched(a: &str) -> (Harness, PathBuf) {
    let mut path = None;
    let harness = Harness::build(|runtime| {
        let a_path = runtime.join(format!("{a}.jsonl"));
        let bytes = session_row(a);
        std::fs::write(&a_path, &bytes).unwrap();
        path = Some(a_path.clone());
        vec![InitSource {
            source_id: source_id_for(a, &a_path).unwrap(),
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    harness.gate.acquired();
    (harness, path.unwrap())
}

struct Drive {
    writer: Writer,
    sources: Sources<BindingLog>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
}

impl Drive {
    fn new(harness: &Harness) -> Self {
        let mut drive = Self {
            writer: harness.writer(),
            sources: Sources::new(CHANNEL, ShadowProvider::Claude, Arc::new(BindingLog)),
            deriver: UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            owed: VecDeque::new(),
        };
        drive
            .sources
            .resume(&mut drive.writer, &mut drive.deriver, &mut drive.owed)
            .unwrap();
        drive
    }

    fn capture(&mut self) {
        self.sources.follow(&mut self.writer).unwrap();
        self.sources
            .capture(&mut self.writer, &mut self.deriver, &mut self.owed)
            .unwrap();
    }

    async fn deliver(&mut self) {
        while let Some(item) = self.owed.pop_front() {
            assert_eq!(self.writer.deliver(&item).await, Step::Done);
        }
    }
}

#[tokio::test]
async fn fresh_launch_after_clear_posts_once_across_old_tail_and_restart() {
    let [a, b] = [(); 2].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let pane = ProducerPane::launch(&a, &a_path);
    let mut drive = Drive::new(&harness);
    drive.capture();
    append(&a_path, &row("a1", "old tail"));
    drive.capture();
    // The old reader has captured its tail but has not posted when the new launch binds.
    let b_path = a_path.with_file_name(format!("{b}.jsonl"));
    std::fs::write(&b_path, session_row(&b)).unwrap();
    append(&b_path, &row("b1", "after clear"));
    publish(context(&pane, &b, "fresh"));
    register(pane.tmux, CHANNEL, binding(&b_path, &b));
    drive.capture();
    drive.deliver().await;
    assert_eq!(
        harness.port.posts(),
        ["old tail", "after clear"],
        "clear body delivered exactly once"
    );
    register(pane.tmux, CHANNEL, binding(&b_path, &b));
    drive.capture();
    drive.deliver().await;
    drop(drive);
    p5::forget_channel_for_tests(CHANNEL);
    let mut drive = Drive::new(&harness);
    drive.capture();
    drive.deliver().await;
    assert_eq!(
        harness.port.posts(),
        ["old tail", "after clear"],
        "restart never reposts"
    );
    append(&b_path, &row("b2", "next prompt"));
    drive.capture();
    drive.deliver().await;
    assert_eq!(
        harness.port.posts(),
        ["old tail", "after clear", "next prompt"]
    );
    assert!(
        !harness
            .alarms
            .taken()
            .iter()
            .any(|a| matches!(a, WriterAlarm::BoundaryPending { .. }))
    );
}

#[tokio::test]
async fn unseen_resume_and_unproven_launches_keep_their_bodies_pending() {
    for scenario in [
        "resume",
        "expected",
        "channel",
        "pane",
        "owner",
        "missing",
        "same_nonce",
    ] {
        let [a, b, c] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
        let (harness, a_path) = launched(&a);
        let pane = ProducerPane::launch(&a, &a_path);
        let mut drive = Drive::new(&harness);
        drive.capture();
        let b_path = a_path.with_file_name(format!("{b}.jsonl"));
        std::fs::write(&b_path, session_row(&b)).unwrap();
        let mut ctx = context(&pane, &b, "fresh");
        match scenario {
            "resume" => ctx.launch_mode = "resume".into(),
            "expected" => ctx.expected_native_session_id = Some(c.clone()),
            "channel" => ctx.channel_id = Some(OTHER),
            "pane" => ctx.tmux_session = "other-pane".into(),
            "owner" => ctx.owner_runtime_root = "foreign-root".into(),
            _ => {}
        }
        if scenario != "missing" {
            let nonce = ctx.execution_nonce.clone();
            publish(ctx);
            std::fs::write(tc::session_temp_path(pane.tmux, "spawn_nonce"), nonce).unwrap();
        }
        register(pane.tmux, CHANNEL, binding(&b_path, &b));
        let (pending, path) = if scenario == "same_nonce" {
            drive.capture();
            let path = a_path.with_file_name(format!("{c}.jsonl"));
            std::fs::write(&path, session_row(&b)).unwrap();
            register(pane.tmux, CHANNEL, binding(&path, &b));
            (b, path)
        } else {
            (b, b_path)
        };
        append(&path, &row("held", "unproven body"));
        drive.capture();
        drive.deliver().await;
        assert_eq!(harness.port.posts(), Vec::<String>::new(), "{scenario}");
        let source = source_id_for(&pending, &path).unwrap();
        let rotation = harness.channel().rotation().unwrap();
        if matches!(scenario, "channel" | "pane" | "owner") {
            assert!(
                rotation.link(&source).is_none(),
                "{scenario}: no stale source attached"
            );
            assert_eq!(
                pane.bound().as_deref(),
                Some(a.as_str()),
                "{scenario}: old binding kept"
            );
            assert_eq!(
                p5::binding_events_since(CHANNEL, 0).unwrap().len(),
                1,
                "{scenario}: no nonce consumed"
            );
            continue;
        }
        assert_eq!(
            harness.channel().cursor(&source).unwrap().captured_through,
            std::fs::metadata(&path).unwrap().len(),
            "{scenario}: body retained in spool"
        );
        assert!(
            matches!(
                rotation.link(&source).unwrap().boundary,
                Boundary::Pending { .. }
            ),
            "{scenario}"
        );
        assert!(
            harness
                .alarms
                .taken()
                .contains(&WriterAlarm::BoundaryPending { source }),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn replaced_nonce_refuses_stale_launch_without_consuming_the_current_launch() {
    let [a, b, c] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let pane = ProducerPane::launch(&a, &a_path);
    let mut drive = Drive::new(&harness);
    drive.capture();
    let b_path = a_path.with_file_name(format!("{b}.jsonl"));
    let c_path = a_path.with_file_name(format!("{c}.jsonl"));
    std::fs::write(&b_path, session_row(&b)).unwrap();
    std::fs::write(&c_path, session_row(&c)).unwrap();
    append(&b_path, &row("stale", "stale body"));
    append(&c_path, &row("current", "current body"));
    publish(context(&pane, &b, "fresh"));
    let stale_context = observed_context_path(pane.tmux).unwrap();
    let next = PreparedIncarnation::create(context(&pane, &c, "fresh")).unwrap();
    let (started, arrived) = std::sync::mpsc::channel();
    let log_root = pane._root.path().to_path_buf();
    let stale = binding(&b_path, &b);
    // The registration waits on the source lock while its execution is replaced.
    let worker = tc::with_tmux_source_authority(pane.tmux, |_| {
        let worker = std::thread::spawn(move || {
            p5::set_test_root(Some(&log_root));
            tc::SOURCE_AUTHORITY_CONTENDED.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || started.send(()).unwrap()));
            });
            register("o-superseded-pane", CHANNEL, stale);
            p5::set_test_root(None);
        });
        arrived
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        std::fs::write(
            tc::session_temp_path(pane.tmux, "spawn_nonce"),
            &next.context.execution_nonce,
        )
        .unwrap();
        worker
    });
    worker.join().unwrap();
    drive.capture();
    drive.deliver().await;
    assert_eq!(
        harness.port.posts(),
        Vec::<String>::new(),
        "stale body withheld"
    );
    assert_eq!(
        pane.bound().as_deref(),
        Some(a.as_str()),
        "stale launch not published"
    );
    assert_eq!(
        p5::binding_events_since(CHANNEL, 0).unwrap().len(),
        1,
        "current nonce remains unconsumed"
    );
    assert!(
        !dedupe::pane_registration::register_launched_claude_pane(
            pane.tmux,
            CHANNEL,
            binding(&b_path, &b),
            Some(&stale_context),
        ),
        "a script captured after replacement still cannot name the old execution"
    );
    assert_eq!(p5::binding_events_since(CHANNEL, 0).unwrap().len(), 1);
    register(pane.tmux, CHANNEL, binding(&c_path, &c));
    drive.capture();
    drive.deliver().await;
    assert_eq!(
        harness.port.posts(),
        ["current body"],
        "matching current launch remains deliverable"
    );
}
