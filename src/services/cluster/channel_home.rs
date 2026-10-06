//! A delegated channel's home gate. The `o_channel_homes` row decides who holds the channel;
//! this process opens its gate only from its own successful renewal write, never from a read.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use sqlx::PgPool;
use tokio::time::Instant;

use crate::db::o_channel_homes::{self, ChannelHome, HeldHome, HomeError, HomeState, HomeWrite};
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};

/// How often a holder renews its lease.
pub(crate) const RENEW_EVERY: Duration = Duration::from_secs(5);
/// A holder closes its own gate this long after sending its last successful renewal (H).
pub(crate) const HOLD_FOR: Duration = Duration::from_secs(20);
/// An operator may force a holder out only once its lease is this stale: F = H + the 180s O piece
/// lease, well past the 60s delivery abort that ends the holder's last POST.
pub(crate) const FORCE_AFTER: Duration = Duration::from_secs(HOLD_FOR.as_secs() + 180);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeIntake {
    /// New intake, turns, placement and commands are accepted.
    Open,
    /// Draining: only pieces already owed may still be posted.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeOwnership {
    /// `home_epoch` is the row's epoch; `gate_epoch` is the inner gate's local acquisition count,
    /// the value its admission hands to the writer. The two are linked only here.
    Owned {
        home_epoch: i64,
        gate_epoch: u64,
        intake: HomeIntake,
    },
    Lost,
}

/// Why a renewal result did not open or extend the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfirmRefused {
    /// Another channel's or holder's write.
    Foreign,
    /// Its send is already H old, or no newer than the last close.
    Late,
    /// The holder already gave this epoch up with a final `close`.
    Retired,
    /// An older epoch than the one held.
    Superseded,
}

struct Held {
    home_epoch: i64,
    gate_epoch: u64,
    renew_sent: Instant,
}

#[derive(Default)]
struct HomeLocal {
    held: Option<Held>,
    /// The last epoch this gate held, kept after a lapse so a final `close` still retires it.
    last_epoch: Option<i64>,
    /// Intake stays closed for this epoch once a drain closes it or the row says drain.
    intake_closed: Option<i64>,
    /// A final `close` retires the epoch for good.
    retired: Option<i64>,
    /// Renewals sent before the last close or loss never reopen the gate.
    closed_at: Option<Instant>,
    /// Replaced or unregistered: never opens again and its lease ends.
    withdrawn: bool,
    /// What the drain waits on, shown by health only.
    drain_blocker: Option<&'static str>,
}

pub(crate) struct HomeGate {
    channel_id: String,
    holder: String,
    gate: Arc<OwnershipGate>,
    local: Mutex<HomeLocal>,
}

impl HomeGate {
    /// Starts `Lost`; only [`HomeGate::confirm`] opens it.
    pub(crate) fn new(channel_id: &str, holder: &str) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            holder: holder.to_string(),
            gate: Arc::new(OwnershipGate::default()),
            local: Mutex::default(),
        }
    }

    /// The admission primitive a writer posts through; it is `Owned` exactly when this home is.
    pub(crate) fn gate(&self) -> Arc<OwnershipGate> {
        Arc::clone(&self.gate)
    }

    pub(crate) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub(crate) fn holder(&self) -> &str {
        &self.holder
    }

    fn locked(&self) -> MutexGuard<'_, HomeLocal> {
        self.local.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn drop_hold(&self, local: &mut HomeLocal, at: Instant) {
        local.held = None;
        local.closed_at = Some(at);
        self.gate.close();
    }

    fn expire(&self, local: &mut HomeLocal, now: Instant) -> bool {
        let due = local
            .held
            .as_ref()
            .is_some_and(|held| now.saturating_duration_since(held.renew_sent) >= HOLD_FOR);
        if due {
            self.drop_hold(local, now);
        }
        due
    }

    fn read(&self, local: &HomeLocal) -> HomeOwnership {
        let Some(held) = local.held.as_ref() else {
            return HomeOwnership::Lost;
        };
        if self.gate.current()
            != (GatewayOwnership::Owned {
                epoch: held.gate_epoch,
            })
        {
            return HomeOwnership::Lost;
        }
        let intake = if local.intake_closed == Some(held.home_epoch) {
            HomeIntake::Closed
        } else {
            HomeIntake::Open
        };
        HomeOwnership::Owned {
            home_epoch: held.home_epoch,
            gate_epoch: held.gate_epoch,
            intake,
        }
    }

    pub(crate) fn ownership(&self) -> HomeOwnership {
        let mut local = self.locked();
        self.expire(&mut local, Instant::now());
        self.read(&local)
    }

    /// The deadline at which the current hold lapses, if any.
    pub(crate) fn expiry(&self) -> Option<Instant> {
        self.locked()
            .held
            .as_ref()
            .map(|held| held.renew_sent + HOLD_FOR)
    }

    /// Closes the gate once H has passed since the last successful renewal was sent.
    pub(crate) fn expire_if_due(&self, now: Instant) -> bool {
        self.expire(&mut self.locked(), now)
    }

    /// Applies a successful renewal sent at `sent`: opens the gate on a new epoch, or extends
    /// the hold on the same one. A result arriving H after its send never opens anything.
    pub(crate) fn confirm(
        &self,
        written: &HeldHome,
        sent: Instant,
    ) -> Result<HomeOwnership, ConfirmRefused> {
        self.confirm_locked(&mut self.locked(), written, sent)
    }

    fn confirm_locked(
        &self,
        local: &mut HomeLocal,
        written: &HeldHome,
        sent: Instant,
    ) -> Result<HomeOwnership, ConfirmRefused> {
        let now = Instant::now();
        self.expire(local, now);
        if written.channel_id() != self.channel_id || written.holder() != self.holder {
            return Err(ConfirmRefused::Foreign);
        }
        let epoch = written.epoch();
        if local.withdrawn || local.retired.is_some_and(|retired| epoch <= retired) {
            return Err(ConfirmRefused::Retired);
        }
        let late = now.saturating_duration_since(sent) >= HOLD_FOR
            || local.closed_at.is_some_and(|closed| sent <= closed);
        if late {
            return Err(ConfirmRefused::Late);
        }
        match self.read(local) {
            HomeOwnership::Owned { home_epoch, .. } if home_epoch > epoch => {
                return Err(ConfirmRefused::Superseded);
            }
            HomeOwnership::Owned { home_epoch, .. } if home_epoch == epoch => {
                if let Some(held) = local.held.as_mut() {
                    held.renew_sent = held.renew_sent.max(sent);
                }
            }
            _ => {
                let gate_epoch = self.gate.acquired();
                local.held = Some(Held {
                    home_epoch: epoch,
                    gate_epoch,
                    renew_sent: sent,
                });
                local.last_epoch = Some(epoch);
            }
        }
        if written.state() != HomeState::Worker {
            local.intake_closed = Some(epoch);
        }
        Ok(self.read(local))
    }

    /// Reopens a drain its final close ended early, from a renewal written at the retired epoch
    /// while the row still drains; intake stays closed and older epochs stay retired.
    pub(crate) fn resume_drain(
        &self,
        written: &HeldHome,
        sent: Instant,
    ) -> Result<HomeOwnership, ConfirmRefused> {
        let mut local = self.locked();
        let epoch = written.epoch();
        let draining = matches!(
            written.state(),
            HomeState::Releasing | HomeState::Reclaiming
        );
        if !draining || local.retired != Some(epoch) {
            return Err(ConfirmRefused::Retired);
        }
        local.retired = Some(epoch - 1);
        let resumed = self.confirm_locked(&mut local, written, sent);
        if resumed.is_err() {
            local.retired = Some(epoch);
        }
        resumed
    }

    /// Whether the final close already retired `epoch`.
    pub(crate) fn final_closed(&self, epoch: i64) -> bool {
        let local = self.locked();
        local.withdrawn || local.retired.is_some_and(|retired| retired >= epoch)
    }

    fn withdraw(&self) {
        let mut local = self.locked();
        local.withdrawn = true;
        self.close_locked(&mut local);
    }

    pub(crate) fn withdrawn(&self) -> bool {
        self.locked().withdrawn
    }

    /// Records what the drain waits on; `None` once it is not waiting.
    pub(crate) fn note_drain(&self, blocker: Option<&'static str>) {
        self.locked().drain_blocker = blocker;
    }

    /// A renewal at `epoch` matched no row: the row no longer names this holder there.
    pub(crate) fn renewal_stale(&self, epoch: i64) {
        let mut local = self.locked();
        if local
            .held
            .as_ref()
            .is_some_and(|held| held.home_epoch == epoch)
        {
            self.drop_hold(&mut local, Instant::now());
        }
    }

    /// Stops new intake for the held epoch while the gate keeps admitting owed pieces.
    pub(crate) fn close_intake(&self) {
        let mut local = self.locked();
        if let Some(epoch) = local.held.as_ref().map(|held| held.home_epoch) {
            local.intake_closed = Some(epoch);
        }
    }

    /// The final close before a leaving write: no later renewal reopens the last epoch held,
    /// even when the hold had already lapsed.
    pub(crate) fn close(&self) {
        self.close_locked(&mut self.locked());
    }

    fn close_locked(&self, local: &mut HomeLocal) {
        if let Some(epoch) = local.last_epoch {
            local.retired = Some(local.retired.map_or(epoch, |retired| retired.max(epoch)));
        }
        self.drop_hold(local, Instant::now());
    }

    /// Whether this process may take new intake for the channel: held, with intake open.
    pub(crate) fn intake_open(&self) -> bool {
        matches!(
            self.ownership(),
            HomeOwnership::Owned {
                intake: HomeIntake::Open,
                ..
            }
        )
    }

    /// Runs `take` under the gate lock only while this home takes intake, so no close falls between
    /// that check and `take`.
    pub(crate) fn while_intake_open<T>(&self, take: impl FnOnce() -> T) -> Option<T> {
        let mut local = self.locked();
        self.expire(&mut local, Instant::now());
        let open = matches!(
            self.read(&local),
            HomeOwnership::Owned {
                intake: HomeIntake::Open,
                ..
            }
        );
        open.then(take)
    }

    /// Runs `hand_off` under the gate lock with the row epoch, only while this home is held.
    pub(crate) fn admit<T>(&self, hand_off: impl FnOnce(i64) -> T) -> Option<T> {
        let mut local = self.locked();
        self.expire(&mut local, Instant::now());
        let held = local.held.as_ref()?;
        let (home_epoch, gate_epoch) = (held.home_epoch, held.gate_epoch);
        self.gate
            .admit(|epoch| (epoch == gate_epoch).then(|| hand_off(home_epoch)))
            .flatten()
    }
}

type Homes = BTreeMap<String, Arc<HomeGate>>;

type Registry = OnceLock<Mutex<Homes>>;

/// The gates of the channels this process takes part in as holder or target; a channel without
/// one follows the gateway rules. Test builds keep the same store once per thread.
#[cfg(not(test))]
static HOMES: Registry = OnceLock::new();
#[cfg(test)]
thread_local! {
    static HOMES: Registry = const { OnceLock::new() };
}

fn with_homes<R>(use_homes: impl FnOnce(&Registry) -> R) -> R {
    #[cfg(not(test))]
    return use_homes(&HOMES);
    #[cfg(test)]
    HOMES.with(use_homes)
}

fn lock_homes(homes: &Mutex<Homes>) -> MutexGuard<'_, Homes> {
    homes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads the registered gates; while none was ever registered nothing is locked.
fn read_homes<R>(read: impl FnOnce(&Homes) -> R) -> Option<R> {
    with_homes(|homes| Some(read(&lock_homes(homes.get()?))))
}

/// Makes `home` its channel's gate in this process; intake for the channel then follows it.
/// A gate it replaces is withdrawn after the swap, so its lease ends and it never reopens.
pub(crate) fn register(home: Arc<HomeGate>) {
    let channel_id = home.channel_id.clone();
    let replaced = with_homes(|homes| {
        lock_homes(homes.get_or_init(Mutex::default)).insert(channel_id, Arc::clone(&home))
    });
    if let Some(old) = replaced.filter(|old| !Arc::ptr_eq(old, &home)) {
        old.withdraw();
    }
}

/// Returns the channel to the gateway rules here: its gate leaves the registry, then is withdrawn.
pub(crate) fn unregister(channel_id: &str) -> Option<Arc<HomeGate>> {
    let removed = with_homes(|homes| lock_homes(homes.get()?).remove(channel_id));
    if let Some(home) = &removed {
        home.withdraw();
    }
    removed
}

/// [`unregister`] only while `home` is still its channel's gate, decided under the registry
/// lock, so the cleanup of a replaced gate never removes the gate that replaced it.
pub(crate) fn unregister_if_same(home: &HomeGate) -> bool {
    let removed = with_homes(|homes| {
        let mut homes = lock_homes(homes.get()?);
        let current = homes.get(&home.channel_id)?;
        if !std::ptr::eq(Arc::as_ptr(current), home) {
            return None;
        }
        homes.remove(&home.channel_id)
    });
    if let Some(removed) = &removed {
        removed.withdraw();
    }
    removed.is_some()
}

/// The gate of a channel this process takes part in; none means the gateway rules apply.
pub(crate) fn registered(channel_id: &str) -> Option<Arc<HomeGate>> {
    read_homes(|homes| homes.get(channel_id).cloned()).flatten()
}

pub(crate) fn any_registered() -> bool {
    read_homes(|homes| !homes.is_empty()).unwrap_or(false)
}

/// The registered homes and what each drain waits on, read from memory only; `None` while
/// nothing is registered, so health stays as it was.
pub(crate) fn health() -> Option<serde_json::Value> {
    let view = |homes: &Homes| {
        let (mut listed, mut draining) = (Vec::new(), Vec::new());
        for (channel, home) in homes {
            let (state, epoch) = match home.ownership() {
                HomeOwnership::Owned {
                    home_epoch, intake, ..
                } => match intake {
                    HomeIntake::Open => ("intake_open", Some(home_epoch)),
                    HomeIntake::Closed => ("draining", Some(home_epoch)),
                },
                HomeOwnership::Lost => ("lost", None),
            };
            let entry = serde_json::json!({"channel": channel, "holder": home.holder,
                "home": state, "epoch": epoch});
            listed.push(entry);
            if let Some(blocker) = home.locked().drain_blocker {
                draining.push(serde_json::json!({"channel": channel, "blocker": blocker}));
            }
        }
        let view = serde_json::json!({"homes": listed, "home_draining": draining});
        (!homes.is_empty()).then_some(view)
    };
    read_homes(view).flatten()
}

/// Registered channels whose gate takes no new intake here.
pub(crate) fn intake_held_channels() -> Vec<String> {
    let held = |homes: &Homes| {
        let closed = homes.iter().filter(|(_, home)| !home.intake_open());
        closed.map(|(channel, _)| channel.clone()).collect()
    };
    read_homes(held).unwrap_or_default()
}

/// Why a claimed row must return to pending: a row routed at a home epoch runs only while this
/// gate holds that epoch with intake open; a gateway-rule row never runs on a registered channel.
pub(crate) fn intake_hold(channel_id: &str, home_epoch: Option<i64>) -> Option<String> {
    let home = registered(channel_id);
    let ownership = home.as_ref().map(|home| home.ownership());
    match (home_epoch, ownership) {
        (None, None) => None,
        (
            Some(routed),
            Some(HomeOwnership::Owned {
                home_epoch,
                intake: HomeIntake::Open,
                ..
            }),
        ) if routed == home_epoch => None,
        (routed, ownership) => Some(format!(
            "channel {channel_id} home does not take intake at epoch {routed:?} here: {ownership:?}"
        )),
    }
}

/// What a booting node read for one channel. Reading never opens the gate.
#[derive(Debug)]
pub(crate) enum BootHome {
    /// No row: the gateway rules apply.
    Gateway,
    Row(ChannelHome),
    /// The read failed: the channel holds and the gate stays `Lost`.
    Unreadable(HomeError),
}

impl BootHome {
    /// The epoch this node should start renewing at, when the row names it holder.
    pub(crate) fn lease_epoch(&self, holder: &str) -> Option<i64> {
        match self {
            Self::Row(home) if home.holder.as_deref() == Some(holder) => Some(home.epoch),
            _ => None,
        }
    }
}

pub(crate) async fn boot_home(pool: &PgPool, home: &HomeGate) -> BootHome {
    match o_channel_homes::read_home(pool, &home.channel_id).await {
        Ok(None) => BootHome::Gateway,
        Ok(Some(row)) => BootHome::Row(row),
        Err(error) => BootHome::Unreadable(error),
    }
}

#[derive(Debug)]
pub(crate) enum LeaseRound {
    Renewed(HomeOwnership),
    Refused(ConfirmRefused),
    /// The row no longer names this holder at the epoch; the gate is closed.
    Stale,
    /// The write failed; the hold lapses at its deadline unless a later renewal lands.
    Failed(HomeError),
    /// The hold lapsed while the write was still pending.
    Expired,
}

/// One renewal: the gate closes at its deadline even while `renewal` is still pending.
pub(crate) async fn lease_round(
    home: &HomeGate,
    epoch: i64,
    sent: Instant,
    renewal: impl Future<Output = Result<HomeWrite<HeldHome>, HomeError>>,
) -> LeaseRound {
    let deadline = home.expiry();
    let lapse = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        result = renewal => match result {
            Ok(HomeWrite::Applied(written)) => match home.confirm(&written, sent) {
                Ok(ownership) => LeaseRound::Renewed(ownership),
                Err(refused) => LeaseRound::Refused(refused),
            },
            Ok(HomeWrite::Stale) => {
                home.renewal_stale(epoch);
                LeaseRound::Stale
            }
            Err(error) => {
                home.expire_if_due(Instant::now());
                LeaseRound::Failed(error)
            }
        },
        () = lapse => {
            home.expire_if_due(Instant::now());
            LeaseRound::Expired
        }
    }
}

/// The holder's lease loop at `epoch`; ends when the row stops naming it or the gate is withdrawn.
/// Not started yet.
pub(crate) async fn run_lease(pool: PgPool, home: Arc<HomeGate>, epoch: i64) {
    while !home.withdrawn() {
        let sent = Instant::now();
        let renewal = o_channel_homes::renew(&pool, &home.channel_id, &home.holder, epoch);
        if let LeaseRound::Stale = lease_round(&home, epoch, sent, renewal).await {
            return;
        }
        let pause = tokio::time::sleep(RENEW_EVERY);
        match home.expiry() {
            Some(deadline) => {
                tokio::select! {
                    () = pause => {}
                    () = tokio::time::sleep_until(deadline) => {
                        home.expire_if_due(Instant::now());
                    }
                }
            }
            None => pause.await,
        }
    }
}

#[cfg(test)]
#[path = "channel_home_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "channel_home_claim_tests.rs"]
mod claim_tests;
