use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1a_collector_status_tick_cannot_recreate_confirmed_row() {
    let test = "n1a_collector_status_tick_cannot_recreate_confirmed_row";
    if !isolated_in("n1a_turn_mode_tests", test, &[]) {
        return;
    }
    let seed = format!("{}{}{}", user("earlier"), said("delivered"), stop());
    let mut h = Harness::new(6325, &seed).await;
    let confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(h.channel.get());
    let frontier = seed.len() as u64;
    h.commit(0, frontier);
    h.take_tmux_calls();
    let tick = crate::services::tui_o::turn_mode::test_tick::signal(h.channel.get());
    h.spawn(frontier);
    h.append(format!("{}{}", user("direct"), said("still streaming")).as_bytes());
    tokio::time::timeout(Duration::from_secs(30), tick)
        .await
        .unwrap()
        .unwrap();
    assert!(
        h.row().is_none(),
        "collector→status tick must leave confirmed channel row absent"
    );
    assert!(
        !h.take_tmux_calls()
            .iter()
            .any(|c| c.starts_with("capture-pane")),
        "turn mode gates pane capture"
    );
    h.cancel();
    let task = h.watcher.take().unwrap().task;
    task.abort();
    let _ = task.await;
    drop(confirmed);

    let mut legacy = Harness::new(6326, &seed).await;
    legacy.commit(0, frontier);
    let tick = crate::services::tui_o::turn_mode::test_tick::signal(legacy.channel.get());
    legacy.spawn(frontier);
    legacy.append(format!("{}{}", user("direct"), said("still streaming")).as_bytes());
    tokio::time::timeout(Duration::from_secs(30), tick)
        .await
        .unwrap()
        .unwrap();
    assert!(
        legacy.row().is_some(),
        "unconfirmed collector retains its row producer"
    );
    legacy.cancel();
    let task = legacy.watcher.take().unwrap().task;
    task.abort();
    let _ = task.await;
}
