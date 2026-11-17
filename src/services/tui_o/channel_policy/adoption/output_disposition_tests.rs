use super::*;

fn id(nonce: &str) -> Identity {
    Identity {
        channel: 8165,
        input: 1,
        nonce: nonce.into(),
        execution: "execution-hash".into(),
        source: "source-hash".into(),
    }
}
#[tokio::test(start_paused = true)]
async fn one_total_deadline_covers_every_retry_and_missing_wake() {
    let output = Output::new(id("a"), Instant::now());
    assert_eq!(output.phase(), Phase::Normalizing);
    assert!(output.take_repair());
    for _ in 0..24 {
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!output.expire());
        assert!(!output.take_repair());
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(output.expire());
    assert_eq!(output.phase(), Phase::RejectPendingNotice(Reason::Deadline));
    assert!(!output.reject(Reason::SourceRejected));
}
#[tokio::test]
async fn a_new_input_leaves_the_old_episode_terminal() {
    let a = Output::new(id("a"), Instant::now());
    assert!(a.reject(Reason::Invalidated));
    let b = Output::new(id("b"), Instant::now());
    assert_ne!(a.id().key(), b.id().key());
    assert_eq!(b.phase(), Phase::Normalizing);
    assert_eq!(a.phase(), Phase::RejectPendingNotice(Reason::Invalidated));
}
#[tokio::test]
async fn concurrent_terminal_reasons_have_one_winner_and_no_body_payload() {
    let output = Output::new(id("a"), Instant::now());
    let mut tasks = Vec::new();
    for reason in [
        Reason::Deadline,
        Reason::Invalidated,
        Reason::SourceRejected,
        Reason::TrustUnavailable,
    ] {
        let o = output.clone();
        tasks.push(tokio::spawn(async move { o.reject(reason) }));
    }
    let mut winners = 0;
    for task in tasks {
        winners += usize::from(task.await.unwrap());
    }
    assert_eq!(winners, 1);
    let encoded = serde_json::to_value(output.id()).unwrap();
    assert_eq!(encoded.as_object().unwrap().len(), 5);
    assert!(encoded.get("body").is_none());
    if let Phase::RejectPendingNotice(reason) = output.phase() {
        assert!(reason.message().contains("터미널"));
    } else {
        panic!("no terminal rejection");
    }
}
