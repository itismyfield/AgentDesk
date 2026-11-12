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
