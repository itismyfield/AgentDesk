use super::{Duration, inflight, wait_for};
use std::future::Future;
use tokio::sync::broadcast::{Receiver, error::TryRecvError};

pub(super) fn drain(finalized: &mut Receiver<inflight::InflightSignal>, channel: u64) -> Vec<u64> {
    let mut completed = Vec::new();
    loop {
        match finalized.try_recv() {
            Ok(inflight::InflightSignal::Completed {
                channel_id,
                turn_id,
            }) if channel_id == channel => {
                completed.push(turn_id);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => return completed,
            Err(error) => panic!("completion evidence was lost: {error:?}"),
        }
    }
}

fn add_completions(
    finalized: &mut Receiver<inflight::InflightSignal>,
    channel: u64,
    turn_ids: &[u64],
    counts: &mut [usize],
) {
    for completed in drain(finalized, channel) {
        if let Some(index) = turn_ids.iter().position(|turn_id| *turn_id == completed) {
            counts[index] += 1;
        }
    }
}

pub(super) async fn wait(
    finalized: &mut Receiver<inflight::InflightSignal>,
    channel: u64,
    turn_ids: &[u64],
    settle: impl Future<Output = ()>,
) -> usize {
    assert!(!turn_ids.is_empty(), "a positive wait needs an owning turn");
    for (index, turn_id) in turn_ids.iter().enumerate() {
        assert!(
            !turn_ids[..index].contains(turn_id),
            "completion targets must name distinct owning turns"
        );
    }
    let mut counts = vec![0; turn_ids.len()];
    assert!(
        wait_for(Duration::from_secs(5), || {
            add_completions(finalized, channel, turn_ids, &mut counts);
            counts.iter().all(|count| *count > 0)
        })
        .await,
        "the delivered answers must complete every owning turn: {turn_ids:?}, counts={counts:?}"
    );
    // Keep duplicate signals visible after the actual fixture's post-completion poll window.
    settle.await;
    add_completions(finalized, channel, turn_ids, &mut counts);
    counts.into_iter().sum()
}

#[tokio::test(flavor = "current_thread")]
async fn completion_wait_filters_channel_and_turn_and_waits_for_every_target() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (signals, mut finalized) = tokio::sync::broadcast::channel(16);
    for (channel_id, turn_id) in [(71, 42), (71, 99), (72, 43)] {
        signals
            .send(inflight::InflightSignal::Completed {
                channel_id,
                turn_id,
            })
            .unwrap();
    }
    let second_published = Arc::new(AtomicBool::new(false));
    let published = second_published.clone();
    let publisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        published.store(true, Ordering::SeqCst);
        signals
            .send(inflight::InflightSignal::Completed {
                channel_id: 71,
                turn_id: 43,
            })
            .unwrap();
        signals
    });
    let completed = wait(&mut finalized, 71, &[42, 43], async {
        assert!(
            second_published.load(Ordering::SeqCst),
            "another channel or turn must not satisfy the missing owning turn"
        );
    })
    .await;
    let _signals = publisher.await.unwrap();
    assert_eq!(completed, 2, "only the two expected bridge turns count");
}

#[tokio::test(flavor = "current_thread")]
async fn completion_wait_keeps_a_duplicate_arriving_during_settle_visible() {
    let (signals, mut finalized) = tokio::sync::broadcast::channel(16);
    signals
        .send(inflight::InflightSignal::Completed {
            channel_id: 71,
            turn_id: 42,
        })
        .unwrap();
    let completed = wait(&mut finalized, 71, &[42], async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        signals
            .send(inflight::InflightSignal::Completed {
                channel_id: 71,
                turn_id: 42,
            })
            .unwrap();
    })
    .await;
    assert_eq!(
        completed, 2,
        "the late duplicate must remain visible to the exact-one assertion"
    );
}
