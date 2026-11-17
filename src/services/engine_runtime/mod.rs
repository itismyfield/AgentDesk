//! Engine-owned runtime state shared by messenger adapters.

pub(crate) mod legacy_state;
pub(crate) mod relay_state;
pub(crate) mod session;

use std::{ops::Deref, sync::Arc};

use self::legacy_state::LegacyState;
use crate::domain::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

pub(crate) struct PolicyRuntime {
    pub(in crate::services) engine: Option<crate::engine::PolicyEngine>,
}

pub(crate) struct EngineRuntime {
    legacy: LegacyState,
    pub(in crate::services) mailboxes: ChannelMailboxRegistry,
    pub(in crate::services) watcher_registry: Arc<relay_state::RegistryState>,
    pub(in crate::services) skills_cache: tokio::sync::RwLock<Vec<(String, String)>>,
    pub(in crate::services) last_turn_at: std::sync::Mutex<Option<String>>,
    pub(in crate::services) provider: ProviderKind,
    pub(in crate::services) api_port: u16,
    pub(in crate::services) pg_pool: Option<sqlx::PgPool>,
    pub(in crate::services) policy: PolicyRuntime,
}

impl EngineRuntime {
    pub(in crate::services) fn new(
        provider: ProviderKind,
        api_port: u16,
        pg_pool: Option<sqlx::PgPool>,
        policy: PolicyRuntime,
        initial_skills: Vec<(String, String)>,
        session_storage: impl FnOnce() -> (
            tokio::sync::Mutex<session::CoreState>,
            ChannelMailboxRegistry,
            Arc<relay_state::RegistryState>,
        ),
    ) -> Arc<Self> {
        let session_transition_locks = Default::default();
        let turn_start_times = Default::default();
        let skills_cache = tokio::sync::RwLock::new(initial_skills);
        let last_turn_at = std::sync::Mutex::new(None);
        let (core, mailboxes, watcher_registry) = session_storage();
        let runtime = Arc::new(Self {
            legacy: LegacyState {
                session_transition_locks,
                turn_start_times,
                core,
            },
            mailboxes,
            watcher_registry,
            skills_cache,
            last_turn_at,
            provider,
            api_port,
            pg_pool,
            policy,
        });
        #[cfg(test)]
        ALLOCATIONS.with(|slot| {
            if let Some(records) = slot.borrow().as_ref() {
                records.borrow_mut().push(Arc::downgrade(&runtime));
            }
        });
        runtime
    }

    #[cfg(test)]
    pub(in crate::services) fn record_allocations_for_tests() -> AllocationRecorder {
        let records: AllocationLog = Default::default();
        ALLOCATIONS.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "allocation recording cannot nest");
            *slot = Some(std::rc::Rc::clone(&records));
        });
        AllocationRecorder { records }
    }
}

impl Deref for EngineRuntime {
    type Target = LegacyState;

    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}

#[cfg(test)]
type AllocationLog = std::rc::Rc<std::cell::RefCell<Vec<std::sync::Weak<EngineRuntime>>>>;

#[cfg(test)]
std::thread_local! {
    static ALLOCATIONS: std::cell::RefCell<Option<AllocationLog>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::services) struct AllocationRecorder {
    records: AllocationLog,
}

#[cfg(test)]
impl AllocationRecorder {
    pub(in crate::services) fn finish(self) -> Vec<std::sync::Weak<EngineRuntime>> {
        self.records.take()
    }
}

#[cfg(test)]
impl Drop for AllocationRecorder {
    fn drop(&mut self) {
        ALLOCATIONS.with(|slot| drop(slot.take()));
    }
}

pub(in crate::services) mod lease {
    //! Private lease state and its existing atomic transitions.

    use poise::serenity_prelude::ChannelId;
    use std::sync::{Arc, atomic::AtomicBool};

    #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    pub(in crate::services) struct DeliveryLeaseKey {
        pub(in crate::services) channel_id: ChannelId,
        pub(in crate::services) generation: u64,
        pub(in crate::services) user_msg_id: u64,
        turn_started_at: Option<String>,
        turn_start_offset: Option<u64>,
    }

    impl DeliveryLeaseKey {
        #[track_caller]
        pub(in crate::services) fn new(
            channel_id: ChannelId,
            generation: u64,
            user_msg_id: u64,
            turn_started_at: Option<&str>,
            turn_start_offset: Option<u64>,
        ) -> Self {
            Self::new_for_site(
                channel_id,
                generation,
                user_msg_id,
                turn_started_at,
                turn_start_offset,
                "delivery_lease_key",
            )
        }

        #[track_caller]
        pub(in crate::services) fn new_for_site(
            channel_id: ChannelId,
            generation: u64,
            user_msg_id: u64,
            turn_started_at: Option<&str>,
            turn_start_offset: Option<u64>,
            site: &'static str,
        ) -> Self {
            Self::new_for_site_with_fallback_offset(
                channel_id,
                generation,
                user_msg_id,
                turn_started_at,
                turn_start_offset,
                None,
                site,
            )
        }

        #[track_caller]
        pub(in crate::services) fn new_for_site_with_fallback_offset(
            channel_id: ChannelId,
            generation: u64,
            user_msg_id: u64,
            turn_started_at: Option<&str>,
            turn_start_offset: Option<u64>,
            fallback_turn_start_offset: Option<u64>,
            site: &'static str,
        ) -> Self {
            if user_msg_id == 0 {
                let started_at = turn_started_at
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let (Some(started_at), Some(start_offset)) = (started_at, turn_start_offset) {
                    return Self {
                        channel_id,
                        generation,
                        user_msg_id,
                        turn_started_at: Some(started_at.to_string()),
                        turn_start_offset: Some(start_offset),
                    };
                }
                if started_at.is_none()
                    && turn_start_offset.is_none()
                    && let Some(start_offset) = fallback_turn_start_offset
                {
                    return Self {
                        channel_id,
                        generation,
                        user_msg_id,
                        turn_started_at: None,
                        turn_start_offset: Some(start_offset),
                    };
                }

                let caller = std::panic::Location::caller();
                tracing::warn!(
                    channel_id = channel_id.get(),
                    generation,
                    delivery_lease_site = site,
                    caller_file = caller.file(),
                    caller_line = caller.line(),
                    "delivery lease id-0 turn missing disambiguators; using degenerate legacy key"
                );

                return Self {
                    channel_id,
                    generation,
                    user_msg_id,
                    turn_started_at: None,
                    turn_start_offset: None,
                };
            }

            Self {
                channel_id,
                generation,
                user_msg_id,
                turn_started_at: None,
                turn_start_offset: None,
            }
        }

        pub(in crate::services) fn channel_id(&self) -> ChannelId {
            self.channel_id
        }

        #[cfg(test)]
        pub(in crate::services) fn is_degenerate_legacy(&self) -> bool {
            self.user_msg_id == 0
                && self.turn_started_at.is_none()
                && self.turn_start_offset.is_none()
        }
    }

    const TAG_UNLEASED: u8 = 0;
    const TAG_LEASED: u8 = 1;
    const TAG_COMMITTED: u8 = 2;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(in crate::services) enum LeaseHolder {
        Watcher { instance_id: u64 },

        Sink,

        Bridge { attempt_id: u64 },

        OWriter { serial: u64 },
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(in crate::services) enum LeaseOutcome {
        Delivered,

        NotDelivered,

        Unknown,
    }

    #[derive(Clone, Debug)]
    enum LeaseState {
        Unleased,

        Leased {
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            deadline_ms: u64,
            start: u64,
            end: u64,
            #[cfg(test)]
            exact_token: Option<LeaseToken>,
        },

        Committed {
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
            outcome: LeaseOutcome,
            #[cfg(test)]
            exact_token: Option<LeaseToken>,
        },
    }

    pub(in crate::services) struct DeliveryLeaseCell {
        channel_id: ChannelId,

        state_tag: std::sync::atomic::AtomicU8,

        payload: std::sync::Mutex<DeliveryLeasePayload>,
    }

    #[derive(Clone, Debug)]
    struct WatcherReservation {
        cancel: Arc<AtomicBool>,
    }

    #[derive(Debug)]
    struct DeliveryLeasePayload {
        lease: LeaseState,
        watcher_reservation: Option<WatcherReservation>,
    }

    struct PayloadLockMarker;

    #[derive(Debug)]
    pub(in crate::services) enum LeaseSnapshot {
        Unleased,
        Leased {
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            deadline_ms: u64,
            start: u64,
            end: u64,
        },
        Committed {
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
            outcome: LeaseOutcome,
        },
    }

    impl Clone for LeaseSnapshot {
        fn clone(&self) -> Self {
            match self {
                Self::Unleased => Self::Unleased,
                Self::Leased {
                    holder,
                    key,
                    deadline_ms,
                    start,
                    end,
                } => Self::Leased {
                    holder: *holder,
                    key: key.clone(),
                    deadline_ms: *deadline_ms,
                    start: *start,
                    end: *end,
                },
                Self::Committed {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                } => Self::Committed {
                    holder: *holder,
                    key: key.clone(),
                    start: *start,
                    end: *end,
                    outcome: *outcome,
                },
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::services) struct IdentityMatchedLease {
        pub(in crate::services) end: u64,

        pub(in crate::services) deadline_ms: Option<u64>,
    }

    impl PayloadLockMarker {
        fn enter() -> Self {
            PAYLOAD_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
            #[cfg(test)]
            PAYLOAD_LOCK_ENTRIES.with(|entries| entries.set(entries.get() + 1));
            Self
        }
    }

    impl Drop for PayloadLockMarker {
        fn drop(&mut self) {
            PAYLOAD_LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }

    impl LeaseSnapshot {
        pub(in crate::services) fn identity_matched(
            &self,
            expected_key: &DeliveryLeaseKey,
        ) -> Option<IdentityMatchedLease> {
            match self {
                Self::Leased {
                    key,
                    deadline_ms,
                    end,
                    ..
                } if key == expected_key => Some(IdentityMatchedLease {
                    end: *end,
                    deadline_ms: Some(*deadline_ms),
                }),
                Self::Committed { key, end, .. } if key == expected_key => {
                    Some(IdentityMatchedLease {
                        end: *end,
                        deadline_ms: None,
                    })
                }
                Self::Unleased | Self::Leased { .. } | Self::Committed { .. } => None,
            }
        }
    }

    impl DeliveryLeaseCell {
        pub(in crate::services) fn new(channel_id: ChannelId) -> Self {
            Self {
                channel_id,
                state_tag: std::sync::atomic::AtomicU8::new(TAG_UNLEASED),
                payload: std::sync::Mutex::new(DeliveryLeasePayload {
                    lease: LeaseState::Unleased,
                    watcher_reservation: None,
                }),
            }
        }

        pub(in crate::services) fn channel_id(&self) -> ChannelId {
            self.channel_id
        }

        pub(in crate::services) fn read(&self) -> LeaseSnapshot {
            self.with_state_locked(|snapshot| snapshot)
        }

        pub(in crate::services) fn with_state_locked<T>(
            &self,
            act: impl FnOnce(LeaseSnapshot) -> T,
        ) -> T {
            let guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            let snapshot = match &guard.lease {
                LeaseState::Unleased => LeaseSnapshot::Unleased,
                LeaseState::Leased {
                    holder,
                    key,
                    deadline_ms,
                    start,
                    end,
                    ..
                } => LeaseSnapshot::Leased {
                    holder: *holder,
                    key: key.clone(),
                    deadline_ms: *deadline_ms,
                    start: *start,
                    end: *end,
                },
                LeaseState::Committed {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                    ..
                } => LeaseSnapshot::Committed {
                    holder: *holder,
                    key: key.clone(),
                    start: *start,
                    end: *end,
                    outcome: *outcome,
                },
            };
            let acted = act(snapshot);
            drop(guard);
            acted
        }

        pub(in crate::services) fn install_watcher_reservation(
            &self,
            _registry_guard: &std::sync::MutexGuard<'static, ()>,
            cancel: Arc<AtomicBool>,
        ) {
            assert_payload_not_held();
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            guard.watcher_reservation = Some(WatcherReservation { cancel });
        }

        pub(in crate::services) fn clear_watcher_reservation(
            &self,
            _registry_guard: &std::sync::MutexGuard<'static, ()>,
            expected_cancel: &Arc<AtomicBool>,
        ) -> bool {
            assert_payload_not_held();
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            let matches = guard
                .watcher_reservation
                .as_ref()
                .is_some_and(|reservation| Arc::ptr_eq(&reservation.cancel, expected_cancel));
            if matches {
                guard.watcher_reservation = None;
            }
            matches
        }

        pub(in crate::services) fn watcher_reservation_matches(
            &self,
            expected_cancel: &Arc<AtomicBool>,
        ) -> bool {
            assert_payload_not_held();
            let guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            guard
                .watcher_reservation
                .as_ref()
                .is_some_and(|reservation| Arc::ptr_eq(&reservation.cancel, expected_cancel))
        }

        pub(in crate::services) fn try_acquire(
            &self,
            key: DeliveryLeaseKey,
            holder: LeaseHolder,
            start: u64,
            end: u64,
            deadline_ms: u64,
        ) -> bool {
            use std::sync::atomic::Ordering;
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();

            if self
                .state_tag
                .compare_exchange(
                    TAG_UNLEASED,
                    TAG_LEASED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return false;
            }
            guard.lease = LeaseState::Leased {
                holder,
                key,
                deadline_ms,
                start,
                end,
                #[cfg(test)]
                exact_token: None,
            };
            true
        }

        pub(in crate::services) fn commit(
            &self,
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
            outcome: LeaseOutcome,
        ) -> bool {
            use std::sync::atomic::Ordering;
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            match &guard.lease {
                LeaseState::Leased {
                    holder: cur_holder,
                    key: cur_key,
                    start: cur_start,
                    end: cur_end,
                    #[cfg(test)]
                        exact_token: None,
                    ..
                } if *cur_holder == holder
                    && cur_key == &key
                    && *cur_start == start
                    && *cur_end == end =>
                {
                    guard.lease = LeaseState::Committed {
                        holder,
                        key,
                        start,
                        end,
                        outcome,
                        #[cfg(test)]
                        exact_token: None,
                    };
                    self.state_tag.store(TAG_COMMITTED, Ordering::Release);
                    true
                }

                _ => false,
            }
        }

        pub(in crate::services) fn release(
            &self,
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
        ) -> bool {
            use std::sync::atomic::Ordering;
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            let matches = match &guard.lease {
                LeaseState::Leased {
                    holder: cur,
                    key: cur_key,
                    start: cur_start,
                    end: cur_end,
                    #[cfg(test)]
                        exact_token: None,
                    ..
                }
                | LeaseState::Committed {
                    holder: cur,
                    key: cur_key,
                    start: cur_start,
                    end: cur_end,
                    #[cfg(test)]
                        exact_token: None,
                    ..
                } => *cur == holder && cur_key == &key && *cur_start == start && *cur_end == end,
                LeaseState::Unleased => false,
                #[cfg(test)]
                LeaseState::Leased {
                    exact_token: Some(_),
                    ..
                }
                | LeaseState::Committed {
                    exact_token: Some(_),
                    ..
                } => false,
            };
            if !matches {
                return false;
            }
            guard.lease = LeaseState::Unleased;
            self.state_tag.store(TAG_UNLEASED, Ordering::Release);
            true
        }

        pub(in crate::services) fn renew(
            &self,
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            new_deadline_ms: u64,
        ) -> bool {
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            if let LeaseState::Leased {
                holder: cur_holder,
                key: cur_key,
                deadline_ms,
                #[cfg(test)]
                    exact_token: None,
                ..
            } = &mut guard.lease
            {
                if *cur_holder == holder && cur_key == &key {
                    *deadline_ms = new_deadline_ms;
                    return true;
                }
            }
            false
        }

        pub(in crate::services) fn reclaim_if_expired(&self, now_ms: u64) -> bool {
            use std::sync::atomic::Ordering;
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _payload_lock_marker = PayloadLockMarker::enter();
            if let LeaseState::Leased {
                deadline_ms,
                #[cfg(test)]
                    exact_token: None,
                ..
            } = &guard.lease
                && now_ms >= *deadline_ms
            {
                guard.lease = LeaseState::Unleased;
                self.state_tag.store(TAG_UNLEASED, Ordering::Release);
                return true;
            }
            false
        }
    }

    pub(in crate::services) fn lease_now_ms() -> u64 {
        use std::sync::OnceLock;
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    pub(in crate::services) fn assert_payload_not_held() {
        debug_assert!(
            PAYLOAD_LOCK_DEPTH.with(|depth| depth.get() == 0),
            "tmux relay coord accessed while DeliveryLeaseCell payload is held"
        );
    }

    #[cfg(test)]
    pub(in crate::services) fn payload_lock_entries_for_test() -> usize {
        PAYLOAD_LOCK_ENTRIES.with(std::cell::Cell::get)
    }

    thread_local! {
        static PAYLOAD_LOCK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        #[cfg(test)]
        static PAYLOAD_LOCK_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[cfg(test)]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::services) struct LeaseToken(NonZeroU64);

    #[cfg(test)]
    impl LeaseToken {
        pub(in crate::services) fn next() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let value = NEXT
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1)
                })
                .unwrap_or_else(|_| panic!("exact delivery lease token space exhausted"));
            Self(NonZeroU64::new(value).expect("token sequence starts nonzero"))
        }
    }

    #[cfg(test)]
    use std::marker::PhantomData;
    #[cfg(test)]
    use std::num::NonZeroU64;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[cfg(test)]
    pub(in crate::services) struct ConsumedCoordinate(NonZeroU64);

    #[cfg(test)]
    impl ConsumedCoordinate {
        pub(in crate::services) fn new(value: u64) -> Option<Self> {
            NonZeroU64::new(value).map(Self)
        }

        fn get(self) -> u64 {
            self.0.get()
        }
    }

    #[cfg(test)]
    pub(in crate::services) struct Acquired;
    #[cfg(test)]
    pub(in crate::services) struct Committed;
    #[cfg(test)]
    pub(in crate::services) struct Released;

    #[cfg(test)]
    pub(in crate::services) trait GuardState {
        const RELEASE_ON_DROP: bool;
    }
    #[cfg(test)]
    impl GuardState for Acquired {
        const RELEASE_ON_DROP: bool = true;
    }
    #[cfg(test)]
    impl GuardState for Committed {
        const RELEASE_ON_DROP: bool = true;
    }
    #[cfg(test)]
    impl GuardState for Released {
        const RELEASE_ON_DROP: bool = false;
    }

    #[cfg(test)]
    pub(in crate::services) struct PinnedLeaseCell<S: GuardState> {
        cell: Arc<DeliveryLeaseCell>,
        key: DeliveryLeaseKey,
        coordinate: ConsumedCoordinate,
        token: LeaseToken,
        armed: bool,
        state: PhantomData<S>,
    }

    #[cfg(test)]
    impl PinnedLeaseCell<Acquired> {
        pub(in crate::services) fn acquire(
            cell: Arc<DeliveryLeaseCell>,
            key: DeliveryLeaseKey,
            coordinate: ConsumedCoordinate,
            deadline_ms: u64,
        ) -> Option<Self> {
            if cell.channel_id != key.channel_id {
                return None;
            }
            let token = LeaseToken::next();
            cell.try_acquire_exact(key.clone(), token, coordinate, deadline_ms)
                .then_some(Self {
                    cell,
                    key,
                    coordinate,
                    token,
                    armed: true,
                    state: PhantomData,
                })
        }

        pub(in crate::services) fn renew(&self, deadline_ms: u64) -> bool {
            self.cell.renew_exact(
                &self.cell,
                &self.key,
                self.token,
                self.coordinate,
                deadline_ms,
            )
        }

        pub(in crate::services) fn matches(&self) -> bool {
            self.cell
                .matches_exact(&self.cell, &self.key, self.token, self.coordinate)
        }

        pub(in crate::services) fn commit(
            mut self,
            outcome: LeaseOutcome,
        ) -> Result<PinnedLeaseCell<Committed>, Self> {
            let key = self.key.clone();
            let coordinate = self.coordinate;
            let token = self.token;
            if !self
                .cell
                .commit_exact(&self.cell, &key, token, coordinate, outcome)
            {
                return Err(self);
            }
            let successor = PinnedLeaseCell {
                cell: Arc::clone(&self.cell),
                key: self.key.clone(),
                coordinate,
                token,
                armed: true,
                state: PhantomData,
            };
            self.armed = false;
            Ok(successor)
        }

        pub(in crate::services) fn reclaim_if_expired(&self, now_ms: u64) -> bool {
            self.cell.reclaim_exact_if_expired(
                &self.cell,
                &self.key,
                self.token,
                self.coordinate,
                now_ms,
            )
        }

        pub(in crate::services) fn release(mut self) -> Result<PinnedLeaseCell<Released>, Self> {
            if !self.release_owned_state() {
                return Err(self);
            }
            let successor = self.released_successor();
            self.armed = false;
            Ok(successor)
        }
    }

    #[cfg(test)]
    impl PinnedLeaseCell<Committed> {
        pub(in crate::services) fn release(mut self) -> Result<PinnedLeaseCell<Released>, Self> {
            if !self.release_owned_state() {
                return Err(self);
            }
            let successor = self.released_successor();
            self.armed = false;
            Ok(successor)
        }
    }

    #[cfg(test)]
    impl<S: GuardState> PinnedLeaseCell<S> {
        pub(in crate::services) fn key_for_tests(&self) -> &DeliveryLeaseKey {
            &self.key
        }
        pub(in crate::services) fn token_for_tests(&self) -> LeaseToken {
            self.token
        }

        pub(in crate::services) fn release_owned_state(&self) -> bool {
            self.cell
                .release_exact(&self.cell, &self.key, self.token, self.coordinate, |_| true)
        }

        fn released_successor(&self) -> PinnedLeaseCell<Released> {
            PinnedLeaseCell {
                cell: Arc::clone(&self.cell),
                key: self.key.clone(),
                coordinate: self.coordinate,
                token: self.token,
                armed: false,
                state: PhantomData,
            }
        }
    }

    #[cfg(test)]
    impl<S: GuardState> Drop for PinnedLeaseCell<S> {
        fn drop(&mut self) {
            if self.armed && S::RELEASE_ON_DROP {
                self.release_owned_state();
            }
        }
    }

    #[cfg(test)]
    impl DeliveryLeaseCell {
        pub(in crate::services) fn try_acquire_exact(
            &self,
            key: DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            deadline_ms: u64,
        ) -> bool {
            use std::sync::atomic::Ordering;
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _marker = PayloadLockMarker::enter();
            if self
                .state_tag
                .compare_exchange(
                    TAG_UNLEASED,
                    TAG_LEASED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return false;
            }
            guard.lease = LeaseState::Leased {
                holder: LeaseHolder::Sink,
                key,
                deadline_ms,
                start: coordinate.get(),
                end: coordinate.get(),
                exact_token: Some(token),
            };
            true
        }

        pub(in crate::services) fn commit_exact(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            outcome: LeaseOutcome,
        ) -> bool {
            self.exact_mutate(
                pinned,
                key,
                token,
                coordinate,
                Some(TAG_COMMITTED),
                |state| match state {
                    LeaseState::Leased { .. } => {
                        *state = LeaseState::Committed {
                            holder: LeaseHolder::Sink,
                            key: key.clone(),
                            start: coordinate.get(),
                            end: coordinate.get(),
                            outcome,
                            exact_token: Some(token),
                        };
                        true
                    }
                    _ => false,
                },
            )
        }

        pub(in crate::services) fn matches_exact(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
        ) -> bool {
            self.exact_mutate(pinned, key, token, coordinate, None, |_| true)
        }

        pub(in crate::services) fn renew_exact(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            deadline_ms: u64,
        ) -> bool {
            self.exact_mutate(pinned, key, token, coordinate, None, |state| match state {
                LeaseState::Leased { deadline_ms: d, .. } => {
                    *d = deadline_ms;
                    true
                }
                _ => false,
            })
        }

        fn exact_mutate(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            success_tag: Option<u8>,
            mutate: impl FnOnce(&mut LeaseState) -> bool,
        ) -> bool {
            if !std::ptr::eq(self, Arc::as_ptr(pinned)) {
                return false;
            }
            let mut guard = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _marker = PayloadLockMarker::enter();
            if !exact_matches(&guard.lease, key, token, coordinate) {
                return false;
            }
            if !mutate(&mut guard.lease) {
                return false;
            }
            if let Some(tag) = success_tag {
                self.state_tag
                    .store(tag, std::sync::atomic::Ordering::Release);
            }
            true
        }

        pub(in crate::services) fn reclaim_exact_if_expired(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            now_ms: u64,
        ) -> bool {
            self.release_exact(pinned, key, token, coordinate, |state| {
            matches!(state, LeaseState::Leased { deadline_ms, .. } if now_ms >= *deadline_ms)
        })
        }

        pub(in crate::services) fn release_exact_for_tests(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
        ) -> bool {
            self.release_exact(pinned, key, token, coordinate, |_| true)
        }

        fn release_exact(
            &self,
            pinned: &Arc<Self>,
            key: &DeliveryLeaseKey,
            token: LeaseToken,
            coordinate: ConsumedCoordinate,
            predicate: impl FnOnce(&LeaseState) -> bool,
        ) -> bool {
            self.exact_mutate(
                pinned,
                key,
                token,
                coordinate,
                Some(TAG_UNLEASED),
                |state| {
                    if !predicate(state) {
                        return false;
                    }
                    *state = LeaseState::Unleased;
                    true
                },
            )
        }
    }

    #[cfg(test)]
    fn exact_matches(
        state: &LeaseState,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        match state {
            LeaseState::Leased {
                holder,
                key: current_key,
                start,
                end,
                exact_token,
                ..
            }
            | LeaseState::Committed {
                holder,
                key: current_key,
                start,
                end,
                exact_token,
                ..
            } => {
                *holder == LeaseHolder::Sink
                    && current_key == key
                    && *exact_token == Some(token)
                    && *start == coordinate.get()
                    && *end == coordinate.get()
            }
            LeaseState::Unleased => false,
        }
    }
}
