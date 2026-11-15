use super::*;

#[tokio::test]
async fn previous_terminal_in_partial_prefix_cannot_discard_current_post() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.append(json!({"type":"system","subtype":"turn_duration"}));
    for _ in 0..1050 {
        r.append(json!({"type":"mode","padding":"x".repeat(4096)}));
    }
    r.append(json!({"type":"user","uuid":"turn-b","message":{"content":"next"}}));
    r.append(json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-a","name":"Bash","input":{}}]}}));
    r.hook("PostToolUse").await;
    assert_eq!(
        collector::observe(&r.session).foreground,
        Foreground::Unknown
    );
    r.hook("PreToolUse").await;
    assert_ne!(
        collector::observe(&r.session).foreground,
        Foreground::Tool,
        "Post in an incomplete replay must survive the older terminal"
    );
}

#[tokio::test]
async fn matching_post_between_busy_read_and_publication_revokes_the_callback() {
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
    let mut file = fs::File::create(&r.path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    drop(file);
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
    };
    assert_eq!(ask().0.activity, Activity::Unknown);
    assert_eq!(ask().0.activity, Activity::Busy);
    lock(&watch).grew_at = Instant::now() - STALE_OPEN_AFTER - Duration::from_secs(1);
    r.hook("PreToolUse").await;
    let (observed, stamp) = ask();
    assert_eq!(observed.activity, Activity::Busy);
    let reading = Reading::bound(observed, stamp, Some(watch), None);
    assert_eq!(reading.publish_if_current(|| 1).await, Some(1));
    let barrier = tokio::sync::Barrier::new(2);
    let count = std::sync::atomic::AtomicUsize::new(0);
    let publish = async {
        barrier.wait().await;
        barrier.wait().await;
        reading
            .publish_if_current(|| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .await
    };
    let post = async {
        barrier.wait().await;
        r.hook("PostToolUse").await;
        barrier.wait().await;
    };
    let (result, ()) = tokio::join!(publish, post);
    assert!(result.is_none());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn contended_collector_publication_yields_to_busy_source_then_recovers() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    let mut rows: Vec<Value> = fs::read_to_string(&r.path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    rows[1]["uuid"] = json!("assistant-tool-a");
    rows[1]["apiBlockIndex"] = json!(0);
    rows[1]["message"]["id"] = json!("message-tool-a");
    let mut file = fs::File::create(&r.path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    drop(file);
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
    };
    assert_eq!(ask().0.activity, Activity::Unknown);
    assert_eq!(ask().0.activity, Activity::Busy);
    lock(&watch).grew_at = Instant::now() - STALE_OPEN_AFTER - Duration::from_secs(1);
    r.hook("PreToolUse").await;
    let (answer, stamp) = ask();
    let reading = Reading::bound(answer, stamp, Some(watch.clone()), None);
    let channel = r.ctx.channel_id.unwrap();
    let previous = lock(&WATCHES).insert(channel, watch.clone());
    let binding = dedupe::runtime_binding_for_tmux_session(&r.session).unwrap();
    let (held, ready) = mpsc::channel();
    let (release, go) = mpsc::channel();
    let (published, result) = mpsc::channel();
    let count = Arc::new(AtomicUsize::new(0));
    let root = events::test_root();
    let session = r.session.clone();
    let holder_root = root.clone();
    let owner = std::thread::spawn(move || {
        events::set_test_root(holder_root.as_deref());
        claude_activity::BUSY_SOURCE_ACQUIRED.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                held.send(()).unwrap();
                go.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
            }))
        });
        with_busy_source(
            &crate::services::provider::ProviderKind::Claude,
            channel,
            &session,
            &binding,
            holder_root,
            |_| {},
        )
    });
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("busy source holds authority");
    let attempts = count.clone();
    let publisher = std::thread::spawn(move || {
        events::set_test_root(root.as_deref());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(reading.publish_if_current(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
        }));
        published.send(outcome).unwrap();
    });
    let timely = result.recv_timeout(Duration::from_secs(2));
    release.send(timely.is_ok()).unwrap();
    let owner_result = crate::test_support::wait::join_within(owner, Duration::from_secs(5));
    crate::test_support::wait::join_within(publisher, Duration::from_secs(5));
    match previous {
        Some(old) => {
            lock(&WATCHES).insert(channel, old);
        }
        None => {
            lock(&WATCHES).remove(&channel);
        }
    }
    let outcome = timely.expect("publication timeout while source authority contended");
    assert!(outcome.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(owner_result, Some(true));
    let (answer, stamp) = ask();
    let fresh = Reading::bound(answer, stamp, Some(watch), None);
    assert_eq!(
        fresh
            .publish_if_current(|| {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .await,
        Some(())
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "next normal observation publishes"
    );
}
