//! Owned busy admission shared by the early and late entries.
use super::*;
use crate::services::claude_tui::host_input::busy::{BusyObservation, BusyState};
use crate::services::discord::queue_io::input_owner::{claude_handoff, held_watch};

#[path = "owned_busy/missing_watch.rs"]
mod missing_watch;
pub(super) use missing_watch::guard as watch_guard;

const SOURCE_HELD: &str = "busy_source_unavailable";

enum RecoveryFailure {
    Reobserve(String),
    Retry(String),
    Refused(String),
}

enum HoldKind {
    Observe,
    Readiness,
    Identity,
}

fn source_hold(state: BusyState) -> HoldKind {
    if state == BusyState::Held {
        HoldKind::Readiness
    } else {
        HoldKind::Observe
    }
}

/// A managed running turn keeps its inflight cursor while hooks move the verified native source.
pub(super) async fn running(
    shared: Option<&Shared>,
    mailbox: &ChannelMailboxHandle,
    (provider, origin): (&ProviderKind, InputOrigin),
    (channel, session, host): (u64, &str, &TurnHost),
    front: &Front,
) -> Result<Option<Registration>, String> {
    if *provider != ProviderKind::Claude || !origin.is_person() || matches!(host, TurnHost::Tmux) {
        return Ok(None);
    }
    let observation = claude_handoff::observe_busy(
        channel,
        session,
        host,
        shared.map_or("", |shared| shared.token_hash.as_str()),
    );
    let state = observation.state();
    if state == BusyState::Busy
        && let Ok(registered) = prepare(
            mailbox.clone(),
            provider,
            origin,
            host,
            session,
            front,
            observation.clone(),
        )
    {
        finish_identity_hold(mailbox, front)
            .await
            .map_err(effect::unsent)?;
        return Ok(Some(registered));
    }
    let channel = ChannelId::new(channel);
    decided(channel, "hold", SOURCE_HELD, &observation);
    if let Some(shared) = shared {
        hold(
            (shared, source_hold(state)),
            mailbox,
            channel,
            front,
            recoverable_probe(
                host,
                &observation,
                shared.pg_pool.as_ref(),
                mailbox,
                Some(front),
            ),
        )
        .await;
    } else {
        hold_start(mailbox, channel, front).await;
        if let Some(probe) = recoverable_probe(host, &observation, None, mailbox, Some(front)) {
            held_watch::arm(
                channel.get(),
                held_front(probe, mailbox.clone(), front.clone()),
            );
        }
    }
    Err(effect::unsent(SOURCE_HELD.into()))
}

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
        let Some(owner) = owner else {
            return Ok((None, true));
        };
        if fresh || *provider != ProviderKind::Claude || !owner.origin.is_person() {
            return Ok((Some(owner), true));
        }
        #[cfg(test)]
        probes::observed(channel.get(), false);
        let Some(session) = session.filter(|session| !session.is_empty()) else {
            return Ok((Some(owner), true));
        };
        let host = crate::services::turn_host::for_turn(
            shared.pg_pool.as_ref(),
            provider,
            channel.get(),
            key,
        )
        .await;
        let observation =
            claude_handoff::observe_busy(channel.get(), session, &host, &shared.token_hash);
        if let TurnHost::Herdr(plan) = &host
            && let Some(row) = &plan.row
            && let crate::db::dispatched_sessions::hosted_execution::HostedRecord::Known(record) =
                &row.record
            && crate::services::herdr_launch::retire::recovered_missing(record)
        {
            owner.recovery.mark();
        }
        let state = observation.state();
        match state {
            BusyState::Legacy => return Ok((Some(owner), true)),
            BusyState::Idle if matches!(&host, TurnHost::Herdr(_)) => {
                return Ok((Some(owner.finish_reobservation().await?), false));
            }
            BusyState::Idle => return Ok((Some(owner), false)),
            _ => {}
        }
        let registered = if state == BusyState::Held {
            Err(SOURCE_HELD.into())
        } else {
            prepare(
                owner.mailbox.clone(),
                provider,
                owner.origin,
                &host,
                session,
                owner.front(),
                observation.clone(),
            )
        };
        let registered = match registered {
            Ok(registered) => registered,
            Err(error) => {
                if owner.mailbox.has_active_turn().await == Ok(false) {
                    return owner
                        .recover_or_discard(
                            shared,
                            (provider, channel, key, session),
                            &host,
                            &observation,
                        )
                        .await;
                }
                decided(channel, "hold", &error, &observation);
                hold(
                    (shared, source_hold(state)),
                    &owner.mailbox,
                    channel,
                    owner.front(),
                    recoverable_probe(
                        &host,
                        &observation,
                        shared.pg_pool.as_ref(),
                        &owner.mailbox,
                        Some(owner.front()),
                    ),
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
        } = owner.finish_reobservation().await?;
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

    async fn recover_or_discard(
        self,
        shared: &Shared,
        (provider, channel, key, session): (&ProviderKind, ChannelId, Option<&str>, &str),
        host: &TurnHost,
        observation: &BusyObservation,
    ) -> Result<(Option<Self>, bool), ()> {
        use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
        use crate::services::claude::herdr_turn::HoldRelease;
        use crate::services::herdr_launch::retire::{self, RetireRefusal, Scope};
        use RecoveryFailure::{Refused, Reobserve, Retry};
        let refused = |reason: &str| Refused(reason.into());
        let recovery = async {
            let TurnHost::Herdr(plan) = host else {
                return Err(refused("host is not a local Herdr execution"));
            };
            let row = plan
                .row
                .as_ref()
                .ok_or_else(|| refused("hosted row unavailable"))?;
            let HostedRecord::Known(record) = &row.record else {
                return Err(refused("hosted record unreadable"));
            };
            let root = crate::config::runtime_root()
                .ok_or_else(|| Retry("runtime root unavailable".into()))?;
            let node = crate::config::session_hosts::local_node()
                .ok_or_else(|| Retry("local node unavailable".into()))?;
            let at = record
                .location
                .as_ref()
                .ok_or_else(|| refused("recorded pane unavailable"))?;
            if record.state != HostedState::Bound
                || record.owner.provider != "claude"
                || record.owner.discord_token_hash != shared.token_hash
                || record.owner.channel_id != channel.get().to_string()
                || record.owner.logical_key != session
                || root != std::path::Path::new(&record.owner.runtime_root)
                || node != record.owner.owner_node
                || record.owner.owner_node != plan.endpoint.execution_node
                || at.execution_node != plan.endpoint.execution_node
                || at.endpoint_config_key != plan.endpoint.config_key
                || at.socket_addr != plan.endpoint.socket_addr
                || at.named_session != plan.endpoint.herdr_session
            {
                return Err(refused("local execution owner or endpoint differs"));
            }
            let pool = shared
                .pg_pool
                .as_ref()
                .ok_or_else(|| Retry("database unavailable".into()))?;
            crate::services::discord::herdr_terminate::root_replaced::current(pool, row)
                .await
                .map_err(Retry)?;
            let retired = retire::observed(pool, row, Scope::Missing)
                .await
                .map_err(|error| {
                    let reason = format!("retire: {error:?}");
                    match error {
                        RetireRefusal::Unproven(_) | RetireRefusal::ProviderRunning => {
                            Reobserve(reason)
                        }
                        RetireRefusal::Changed(_) | RetireRefusal::NoLocalEndpoint => Retry(reason),
                        RetireRefusal::NoRow(_) => Refused(reason),
                    }
                })?;
            if retired.hold != HoldRelease::Released {
                return Err(refused(
                    "retired execution input hold could not be released",
                ));
            }
            crate::services::tui_prompt_dedupe::withhold_herdr_execution(
                session,
                Some(&record.execution_nonce),
            );
            self.recovery.mark();
            let mut expected = record.clone();
            expected.state = HostedState::Retired;
            match crate::services::turn_host::for_turn(Some(pool), provider, channel.get(), key)
                .await
            {
                TurnHost::Herdr(next)
                    if next
                        .row
                        .as_ref()
                        .is_some_and(|row| row.record == HostedRecord::Known(expected.clone())) =>
                {
                    Ok(())
                }
                _ => Err(refused(
                    "fresh Herdr execution is not admissible after retirement",
                )),
            }
        }
        .await;
        let (reason, probe) = match recovery {
            Ok(()) => return Ok((Some(self.finish_reobservation().await?), false)),
            Err(Refused(reason)) => {
                decided(channel, "discard", &reason, observation);
                let failure = super::super::start_failure::Failure::owner(&self);
                let result = failure
                    .discard(Some(shared.clone()), channel, reason)
                    .await
                    .map(|_| ());
                let _ = self.settled.send(result.map_err(effect::unsent));
                return Err(());
            }
            Err(Reobserve(reason)) => (
                reason,
                recovery_probe(
                    host,
                    observation,
                    shared.pg_pool.as_ref(),
                    &self.mailbox,
                    self.front(),
                ),
            ),
            Err(Retry(reason)) => (reason, None),
        };
        decided(channel, "hold", &reason, observation);
        // When the exact read cannot be built, re-resolve on the next look.
        let probe = probe.unwrap_or_else(|| Box::new(|| Screen::Ready));
        hold(
            (shared, HoldKind::Identity),
            &self.mailbox,
            channel,
            self.front(),
            Some(probe),
        )
        .await;
        self.defer_before_submit(&reason);
        Err(())
    }

    async fn finish_reobservation(self) -> Result<Self, ()> {
        if let Err(error) = finish_identity_hold(&self.mailbox, self.front()).await {
            self.defer_before_submit(&error);
            return Err(());
        }
        Ok(self)
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
                    && claude_handoff::observe_busy(
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
    let observation = claude_handoff::observe_busy(
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
    let probe = recoverable_probe(turn.host, observation, turn.pool, mailbox, Some(front))
        .map(|probe| unwound(turn.provider.clone(), probe, mailbox.clone(), at));
    let handle = tokio::runtime::Handle::current();
    let kind = source_hold(observation.state());
    decided(channel, "hold", SOURCE_HELD, observation);
    if let Some(shared) = turn.shared {
        handle.block_on(hold((shared, kind), mailbox, channel, front, probe));
    } else {
        handle.block_on(hold_start(mailbox, channel, front));
        if let Some(probe) = probe {
            held_watch::arm(turn.channel_id, probe);
        }
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
    #[cfg(test)]
    probes::before_registration(front.1.channel_id);
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

async fn finish_identity_hold(mailbox: &ChannelMailboxHandle, front: &Front) -> Result<(), String> {
    let front = front.clone();
    mailbox
        .v2_request(move |slot| {
            slot.as_mut()
                .ok_or("V2 not adopted")?
                .finish_identity_hold(&front)
        })
        .await
}

/// Holds the front and gives the readiness deadline to the watch that owns it; an identity hold
/// gets that deadline too, so the hold ends by one delivery or one unsent settlement.
async fn hold(
    (shared, kind): (&Shared, HoldKind),
    mailbox: &ChannelMailboxHandle,
    channel: ChannelId,
    front: &Front,
    probe: Option<crate::services::claude_tui::host_input::busy::BusyProbe>,
) {
    let drive = drive(shared, channel);
    if matches!(kind, HoldKind::Identity) {
        held_watch::readiness::hold_identity(drive.clone(), front).await;
    } else {
        hold_start(mailbox, channel, front).await;
    }
    super::super::super::busy_retry::note_busy_result(
        shared,
        &ProviderKind::Claude,
        channel,
        std::slice::from_ref(&front.1),
        &[],
        &Err(effect::unsent(SOURCE_HELD.into())),
    )
    .await;
    let Some(probe) = probe else {
        return;
    };
    held_watch::arm(
        channel.get(),
        held_front(probe, mailbox.clone(), front.clone()),
    );
    if matches!(kind, HoldKind::Readiness) {
        held_watch::readiness::hold(drive.clone(), front).await;
    }
    held_watch::watch(drive, front.1.message_id);
}

/// One line per busy-entry disposition, so a discard or hold can be traced without its notice.
fn decided(channel: ChannelId, decision: &str, reason: &str, observation: &BusyObservation) {
    tracing::info!(
        channel_id = channel.get(),
        decision,
        reason,
        screen = ?observation.state(),
        "herdr busy entry decision"
    );
}

fn recoverable_probe(
    host: &TurnHost,
    observation: &BusyObservation,
    pool: Option<&sqlx::PgPool>,
    mailbox: &ChannelMailboxHandle,
    front: Option<&Front>,
) -> Option<crate::services::claude_tui::host_input::busy::BusyProbe> {
    observation.herdr()?;
    watch_guard(pool, host, mailbox.clone(), front).map(|guard| guard(observation.probe()))
}

fn recovery_probe(
    host: &TurnHost,
    observation: &BusyObservation,
    pool: Option<&sqlx::PgPool>,
    mailbox: &ChannelMailboxHandle,
    front: &Front,
) -> Option<crate::services::claude_tui::host_input::busy::BusyProbe> {
    use crate::services::discord::herdr_terminate::root_replaced::current;
    let TurnHost::Herdr(plan) = host else {
        return None;
    };
    let row = plan.row.as_ref()?.clone();
    let pool = pool?.clone();
    let mut read = recoverable_probe(host, observation, Some(&pool), mailbox, Some(front))?;
    Some(Box::new(move || {
        let screen = read();
        // A replaced row needs a fresh intake decision even if its old pane never looks Idle.
        if screen != Screen::Ready && futures::executor::block_on(current(&pool, &row)).is_err() {
            return Screen::Ready;
        }
        screen
    }))
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

    type BeforeRegistration = Option<(u64, Box<dyn FnOnce()>)>;
    thread_local! {
        static BEFORE_REGISTRATION: std::cell::RefCell<BeforeRegistration> = const {
            std::cell::RefCell::new(None)
        };
    }

    impl OwnerTurn {
        pub(in crate::services::discord) fn before_busy_registration(
            channel: u64,
            action: impl FnOnce() + 'static,
        ) {
            BEFORE_REGISTRATION.with_borrow_mut(|slot| *slot = Some((channel, Box::new(action))));
        }
    }

    pub(super) fn before_registration(channel: Option<u64>) {
        let action = BEFORE_REGISTRATION
            .with_borrow_mut(|slot| slot.take_if(|(expected, _)| Some(*expected) == channel));
        if let Some((_, action)) = action {
            action();
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

#[cfg(test)]
#[path = "owned_busy_missing_tests.rs"]
mod missing_tests;
