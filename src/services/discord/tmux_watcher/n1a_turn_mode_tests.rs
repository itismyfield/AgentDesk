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
    let tick = n1a_tick_signal(h.channel.get());
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
    let tick = n1a_tick_signal(legacy.channel.get());
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

#[cfg(test)]
static N1A_TICK: std::sync::Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn n1a_tick_signal(channel: u64) -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    *N1A_TICK.lock().unwrap_or_else(|e| e.into_inner()) = Some((channel, tx));
    rx
}

#[cfg(test)]
pub(in crate::services::discord::tmux::tmux_watcher) fn n1a_tick_completed(channel: u64) {
    let mut tick = N1A_TICK.lock().unwrap_or_else(|e| e.into_inner());
    if tick.as_ref().is_some_and(|(id, _)| *id == channel) {
        let (_, tx) = tick.take().unwrap();
        let _ = tx.send(());
    }
}
