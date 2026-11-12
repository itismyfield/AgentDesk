//! Owned busy admission shared by the early and late entries.
use super::*;
use crate::services::claude_tui::host_input::busy::{BusyObservation, BusyState};
use crate::services::discord::queue_io::input_owner::held_watch;

const SOURCE_HELD: &str = "busy_source_unavailable";

impl OwnerTurn {
    pub(in crate::services::discord) async fn early(
        shared: &Shared,
        provider: &ProviderKind,
        channel: ChannelId,
        session: Option<&str>,
        key: Option<&str>,
        owner: Option<Self>,
        fresh: bool,
    ) -> Result<(Option<Self>, bool), ()> {
        if fresh
            || *provider != ProviderKind::Claude
            || !owner.as_ref().is_some_and(|owner| owner.origin.is_person())
        {
            return Ok((owner, true));
        }
        #[cfg(test)]
        probes::observed(channel.get(), false);
        let Some(session) = session.filter(|session| !session.is_empty()) else {
            return Ok((owner, true));
        };
        let host = crate::services::turn_host::for_turn(
            shared.pg_pool.as_ref(),
            provider,
            channel.get(),
            key,
        )
        .await;
        let observation =
            BusyOperation::observe_hosted(channel.get(), session, &host, &shared.token_hash);
        match observation.state() {
            BusyState::Legacy => return Ok((owner, true)),
            BusyState::Idle => return Ok((owner, false)),
            _ => {}
        }
        let Some(owner) = owner else {
            return Ok((None, true));
        };
        if observation.state() == BusyState::Held {
            hold(
                shared,
                &owner.mailbox,
                channel,
                owner.front(),
                observation.probe(),
            )
            .await;
            owner.defer_before_submit(SOURCE_HELD);
            return Err(());
        }
        let registered = prepare(
            owner.mailbox.clone(),
            provider,
            owner.origin,
            &host,
            session,
            owner.front(),
            observation.clone(),
        );
        let registered = match registered {
            Ok(registered) => registered,
            Err(error) => {
                hold(
                    shared,
                    &owner.mailbox,
                    channel,
                    owner.front(),
                    observation.probe(),
                )
                .await;
                owner.defer_before_submit(&error);
                return Err(());
            }
        };
        let OwnerTurn {
            claim,
            settled,
            runtime,
            ..
        } = owner;
        let runtime = runtime.or_else(|| Some(shared.clone()));
        let _ = paste_busy(
            (shared.pg_pool.clone(), provider.clone(), runtime),
            (channel.get(), session.to_string(), host),
            claim,
            registered,
            settled,
        )
        .await;
        Err(())
    }

    pub(in crate::services::discord) async fn legacy_diagnostic(
        self,
        shared: &Shared,
        provider: &ProviderKind,
        channel: ChannelId,
        (session, key): (Option<String>, Option<String>),
        (dir, native): (&str, Option<String>),
    ) {
        let host = crate::services::turn_host::for_turn(
            shared.pg_pool.as_ref(),
            provider,
            channel.get(),
            key.as_deref(),
        )
        .await;
        let live = session
            .as_deref()
            .filter(|session| {
                *provider == ProviderKind::Claude
                    && BusyOperation::observe_hosted(
                        channel.get(),
                        session,
                        &host,
                        &shared.token_hash,
                    )
                    .state()
                        == BusyState::Legacy
            })
            .and_then(|session| {
                super::super::super::tui_followup::claude_followup_transcript(
                    Some(dir),
                    native.as_deref(),
                    Some(session),
                )
            })
            .zip(session);
        match live {
            Some((transcript, session)) => {
                self.into_busy_pane(
                    (shared.pg_pool.clone(), provider.clone()),
                    (channel.get(), session, key),
                    (transcript.display().to_string(), native),
                )
                .await;
            }
            None => self.defer_before_submit("hosted_tui_busy_pre_submit"),
        }
    }
}

pub(super) fn late(
    turn: &StreamingTurn<'_>,
    origin: InputOrigin,
    mailbox: &ChannelMailboxHandle,
    front: &Front,
    legacy: impl FnOnce() -> Result<Registration, String>,
) -> Result<Registration, String> {
    if turn.force_fresh || *turn.provider != ProviderKind::Claude || !origin.is_person() {
        return legacy();
    }
    #[cfg(test)]
    probes::observed(turn.channel_id, true);
    let Some(session) = turn.tmux_session_name.filter(|session| !session.is_empty()) else {
        return legacy();
    };
    let observation = BusyOperation::observe_hosted(
        turn.channel_id,
        session,
        turn.host,
        turn.shared
            .map(|shared| shared.token_hash.as_str())
            .unwrap_or_default(),
    );
    match observation.state() {
        BusyState::Legacy | BusyState::Idle => legacy(),
        BusyState::Busy => {
            let registered = prepare(
                mailbox.clone(),
                turn.provider,
                origin,
                turn.host,
                session,
                front,
                observation.clone(),
            );
            if registered.is_err() {
                hold_late(turn, mailbox, front, &observation);
            }
            registered
        }
        BusyState::Held => {
            hold_late(turn, mailbox, front, &observation);
            Err(effect::unsent(SOURCE_HELD.into()))
        }
    }
}

fn hold_late(
    turn: &StreamingTurn<'_>,
    mailbox: &ChannelMailboxHandle,
    front: &Front,
    observation: &BusyObservation,
) {
    let channel = ChannelId::new(turn.channel_id);
    let at = (
        turn.channel_id,
        turn.cancel.turn_nonce().map(str::to_string),
    );
    let probe = unwound(
        turn.provider.clone(),
        observation.probe(),
        mailbox.clone(),
        at,
    );
    let handle = tokio::runtime::Handle::current();
    if let Some(shared) = turn.shared {
        handle.block_on(hold(shared, mailbox, channel, front, probe));
    } else {
        handle.block_on(hold_start(mailbox, channel, front));
        held_watch::arm(turn.channel_id, probe);
    }
}

fn prepare(
    mailbox: ChannelMailboxHandle,
    provider: &ProviderKind,
    origin: InputOrigin,
    host: &TurnHost,
    session: &str,
    front: &Front,
    observation: BusyObservation,
) -> Result<Registration, String> {
    let unavailable = || effect::unsent(SOURCE_HELD.into());
    let transcript = observation
        .transcript()
        .and_then(|path| path.to_str())
        .ok_or_else(unavailable)?;
    let native = observation.session_id().ok_or_else(unavailable)?;
    let target = observation.target().ok_or_else(unavailable)?;
    let source = observation.source().ok_or_else(unavailable)?;
    let herdr = observation.herdr().ok_or_else(unavailable)?;
    let mut registered = Registration::observed(
        mailbox,
        (provider.as_str(), origin, true),
        (host, session, Some((transcript, Some(native)))),
        &front.1,
        None,
    )
    .map_err(effect::unsent)?;
    if registered.phase != target::Phase::Busy || registered.attempt["target"] != target {
        return Err(unavailable());
    }
    registered.attempt["source"] = source;
    registered.transcript = transcript.into();
    registered.herdr = Some(herdr);
    registered.until_done = None;
    registered.observation = Some(observation);
    Ok(registered)
}

pub(super) fn unwound(
    provider: ProviderKind,
    mut read: crate::services::claude_tui::host_input::busy::BusyProbe,
    mailbox: ChannelMailboxHandle,
    (channel, nonce): (u64, Option<String>),
) -> crate::services::claude_tui::host_input::busy::BusyProbe {
    use crate::services::discord::inflight::load_inflight_state_read_only_result as row_now;
    Box::new(move || match read() {
        Screen::Ready => {
            let active = futures::executor::block_on(mailbox.has_active_turn());
            let own = |row: &crate::services::discord::inflight::InflightTurnState| {
                nonce.is_some() && row.turn_nonce == nonce
            };
            match (active, row_now(&provider, channel)) {
                (Ok(false), Ok(row)) if !row.as_ref().is_some_and(own) => Screen::Ready,
                _ => Screen::Unknown,
            }
        }
        read => read,
    })
}

async fn hold(
    shared: &Shared,
    mailbox: &ChannelMailboxHandle,
    channel: ChannelId,
    front: &Front,
    probe: crate::services::claude_tui::host_input::busy::BusyProbe,
) {
    hold_start(mailbox, channel, front).await;
    super::super::super::busy_retry::note_busy_result(
        shared,
        &ProviderKind::Claude,
        channel,
        std::slice::from_ref(&front.1),
        &[],
        &Err(effect::unsent(SOURCE_HELD.into())),
    )
    .await;
    held_watch::arm(
        channel.get(),
        held_front(probe, mailbox.clone(), front.clone()),
    );
    held_watch::watch(drive(shared, channel), front.1.message_id);
}

fn same_front(expected: &Front, current: Option<&Front>) -> bool {
    current.is_some_and(|current| {
        current.0 == expected.0
            && current.1.message_id == expected.1.message_id
            && current.1.queued_generation == expected.1.queued_generation
    })
}

fn held_front(
    mut read: crate::services::claude_tui::host_input::busy::BusyProbe,
    mailbox: ChannelMailboxHandle,
    expected: Front,
) -> crate::services::claude_tui::host_input::busy::BusyProbe {
    Box::new(move || {
        let owns = || {
            futures::executor::block_on(
                mailbox.v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.front()))),
            )
            .map(|front| same_front(&expected, front.as_ref()))
        };
        match owns() {
            Ok(false) => return Screen::Gone,
            Err(_) => return Screen::Unknown,
            _ => {}
        }
        let screen = read();
        match owns() {
            Ok(true) => screen,
            Ok(false) => Screen::Gone,
            Err(_) => Screen::Unknown,
        }
    })
}

#[cfg(test)]
type DriveResults = std::collections::HashMap<u64, Vec<String>>;
#[cfg(test)]
static DRIVE_RESULTS: std::sync::LazyLock<Mutex<DriveResults>> =
    std::sync::LazyLock::new(Default::default);

#[cfg(test)]
pub(super) fn note_drive_result(channel: u64, result: &Result<(), String>) {
    DRIVE_RESULTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(channel)
        .or_default()
        .push(format!("{result:?}"));
}

#[cfg(test)]
pub(super) fn observe_settlement(channel: u64, previous: Option<Owed>) -> Option<Owed> {
    previous.map(|previous| {
        let observed: Owed = std::sync::Arc::new(move || {
            let previous = previous.clone();
            Box::pin(async move {
                previous().await;
                DRIVE_RESULTS
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .entry(channel)
                    .or_default()
                    .push("loop-settled".into());
            })
        });
        observed
    })
}

#[cfg(test)]
mod probes {
    use super::*;

    impl OwnerTurn {
        pub(in crate::services::discord) fn drive_results(channel: u64) -> Vec<String> {
            DRIVE_RESULTS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&channel)
                .cloned()
                .unwrap_or_default()
        }

        #[cfg(test)]
        pub(in crate::services::discord) fn probes(channel: u64) -> (usize, usize) {
            COUNTS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&channel)
                .copied()
                .unwrap_or_default()
        }

        #[cfg(test)]
        pub(in crate::services::discord) fn pause_late(
            channel: u64,
        ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let (entered, observed) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            PAUSES
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(channel, (entered, wait));
            (observed, release)
        }
    }

    static COUNTS: std::sync::LazyLock<Mutex<std::collections::HashMap<u64, (usize, usize)>>> =
        std::sync::LazyLock::new(Default::default);

    type Pause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

    static PAUSES: std::sync::LazyLock<Mutex<std::collections::HashMap<u64, Pause>>> =
        std::sync::LazyLock::new(Default::default);

    pub(super) fn observed(channel: u64, late: bool) {
        {
            let mut counts = COUNTS.lock().unwrap_or_else(|error| error.into_inner());
            let counts = counts.entry(channel).or_default();
            if late {
                counts.1 += 1;
            } else {
                counts.0 += 1;
            }
        }
        if !late {
            return;
        }
        let pause = PAUSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&channel);
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(20));
        }
    }

    #[test]
    fn a_held_watch_requires_the_same_ticket_and_queued_generation() {
        let item: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7,"message_id":8114,"channel_id":8114,
            "source_message_ids":[8114],"queued_generation":1,"text":"same original"
        }))
        .unwrap();
        let front = (Some("one".to_string()), item);
        assert!(same_front(&front, Some(&front)));
        assert!(!same_front(&front, None));
        let mut changed = front.clone();
        changed.0 = Some("other".into());
        assert!(!same_front(&front, Some(&changed)));
        changed = front.clone();
        changed.1.queued_generation += 1;
        assert!(!same_front(&front, Some(&changed)));
        changed = front.clone();
        changed.1.message_id += 1;
        assert!(!same_front(&front, Some(&changed)));
    }
}
