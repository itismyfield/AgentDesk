use poise::serenity_prelude::ChannelId;

type OwnerRacePause = (
    ChannelId,
    &'static str,
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
);
static OWNER_RACE_PAUSES: std::sync::Mutex<Vec<OwnerRacePause>> = std::sync::Mutex::new(Vec::new());
static OWNER_RACE_KICKS: std::sync::Mutex<Vec<(ChannelId, usize)>> =
    std::sync::Mutex::new(Vec::new());

pub(in crate::services::discord) fn owner_race_kick_attempts(channel: ChannelId) -> usize {
    OWNER_RACE_KICKS
        .lock()
        .unwrap()
        .iter()
        .find(|entry| entry.0 == channel)
        .map_or(0, |entry| entry.1)
}

pub(in crate::services::discord) fn install_owner_race_pause(
    channel: ChannelId,
    stage: &'static str,
) -> (
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let resume = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut pauses = OWNER_RACE_PAUSES.lock().unwrap();
    assert!(
        !pauses
            .iter()
            .any(|pause| pause.0 == channel && pause.1 == stage)
    );
    pauses.push((channel, stage, entered.clone(), resume.clone()));
    if stage == "kickoff_before_take" {
        let mut counts = OWNER_RACE_KICKS.lock().unwrap();
        if !counts.iter().any(|entry| entry.0 == channel) {
            counts.push((channel, 0));
        }
    }
    (entered, resume)
}

pub(in crate::services::discord) async fn owner_race_pause(
    channel: ChannelId,
    stage: &'static str,
) {
    if stage == "kickoff_before_take"
        && let Some(entry) = OWNER_RACE_KICKS
            .lock()
            .unwrap()
            .iter_mut()
            .find(|entry| entry.0 == channel)
    {
        entry.1 += 1;
    }
    let pause = {
        let mut pauses = OWNER_RACE_PAUSES.lock().unwrap();
        pauses
            .iter()
            .position(|pause| pause.0 == channel && pause.1 == stage)
            .map(|at| pauses.remove(at))
    };
    if let Some((_, _, entered, resume)) = pause {
        entered.notify_one();
        resume.notified().await;
    }
}
