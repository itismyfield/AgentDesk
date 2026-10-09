//! Routes O writer alarms to health reasons and the operator channel, never to the failing channel.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use sqlx::PgPool;

use super::shadow::tap::TuiOConfig;
use super::writer::{AlarmSink, WriterAlarm};

/// NotFound is expected now and then; it becomes an alarm at this many per channel per window.
pub(crate) const NOT_FOUND_THRESHOLD: usize = 3;
pub(crate) const NOT_FOUND_WINDOW: Duration = Duration::from_secs(3600);

/// Outbox source label and reason prefix for operator-channel alarm messages.
pub(crate) const ALARM_SOURCE: &str = "tui_o_alarm";
pub(crate) const ALARM_REASON_CODE: &str = "tui_o.writer_alarm";

/// Sends one alarm line to the operator channel.
pub(crate) trait AlarmNotifier: Send + Sync {
    fn notify(&self, alert_channel: u64, alarm: AlarmNotification, attempt: NotificationAttempt);
}

/// History and current health conditions are independent of operator enqueue attempts.
#[derive(Default)]
pub(crate) struct AlarmHealth {
    raised: Mutex<BTreeSet<String>>,
    notifications: Mutex<HashMap<String, NotificationState>>,
    active: Mutex<BTreeSet<String>>,
    not_found: Mutex<HashMap<u64, VecDeque<Instant>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NotificationState {
    Pending,
    Enqueued,
}

pub(crate) struct AlarmNotification {
    channel: u64,
    kind: &'static str,
    text: String,
}

/// A failed, suppressed, or cancelled attempt releases its slot for the next observation.
pub(crate) struct NotificationAttempt {
    health: Arc<AlarmHealth>,
    reason: String,
    enqueued: bool,
}

impl NotificationAttempt {
    fn begin(health: Arc<AlarmHealth>, reason: String) -> Option<Self> {
        let mut notifications = locked(&health.notifications);
        if notifications.contains_key(&reason) {
            return None;
        }
        notifications.insert(reason.clone(), NotificationState::Pending);
        drop(notifications);
        Some(Self {
            health,
            reason,
            enqueued: false,
        })
    }

    fn commit(mut self) {
        locked(&self.health.notifications).insert(self.reason.clone(), NotificationState::Enqueued);
        self.enqueued = true;
    }
}

impl Drop for NotificationAttempt {
    fn drop(&mut self) {
        if !self.enqueued {
            locked(&self.health.notifications).remove(&self.reason);
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn within_window(hit: Instant, now: Instant) -> bool {
    now.saturating_duration_since(hit) < NOT_FOUND_WINDOW
}

impl AlarmHealth {
    /// True only for the first occurrence of `reason` in this process.
    fn latch(&self, reason: &str) -> bool {
        locked(&self.raised).insert(reason.to_string())
    }

    fn activate(&self, reason: &str) {
        locked(&self.active).insert(reason.to_string());
    }

    /// Records one NotFound and reports whether the channel reached the threshold within the window.
    fn record_not_found(&self, channel: u64, now: Instant) -> bool {
        let mut not_found = locked(&self.not_found);
        let hits = not_found.entry(channel).or_default();
        while hits.front().is_some_and(|hit| !within_window(*hit, now)) {
            hits.pop_front();
        }
        hits.push_back(now);
        hits.len() >= NOT_FOUND_THRESHOLD
    }

    /// Pause and reader count end on their own evidence; other sticky conditions wait for an operator.
    fn resume_gateway(&self, channel: u64) {
        let reason = format!("tui_o:{}:{channel}", PAUSED_NO_GATEWAY);
        locked(&self.active).remove(&reason);
    }

    fn set_resume_pending(&self, channel: u64, pending: bool) {
        let reason = format!("tui_o:{RESUME_PENDING}:{channel}");
        let mut active = locked(&self.active);
        if pending {
            active.insert(reason);
        } else {
            active.remove(&reason);
        }
    }

    /// A writer recovered in process replaces the halted one, so its halt is no longer in force.
    fn clear_halted(&self, channel: u64) {
        locked(&self.active).remove(&format!("tui_o:halted:{channel}"));
    }

    /// Conditions in force at `now`; NotFound frequency is re-counted against the window here.
    pub(crate) fn current_at(&self, now: Instant) -> Vec<String> {
        let mut current = locked(&self.active).clone();
        locked(&self.not_found).retain(|channel, hits| {
            hits.retain(|hit| within_window(*hit, now));
            if hits.len() >= NOT_FOUND_THRESHOLD {
                current.insert(format!("tui_o:{NOT_FOUND_FREQUENT}:{channel}"));
            }
            !hits.is_empty()
        });
        current.into_iter().collect()
    }

    /// Every reason raised in this process, resolved or not.
    #[cfg(test)]
    fn history(&self) -> Vec<String> {
        locked(&self.raised).iter().cloned().collect()
    }
}

static PROCESS_HEALTH: LazyLock<Arc<AlarmHealth>> = LazyLock::new(Arc::default);

/// Alarm conditions in force for this process's health snapshot, as `tui_o:<kind>:<channel>`.
pub(crate) fn health_reasons() -> Vec<String> {
    PROCESS_HEALTH.current_at(Instant::now())
}

/// The channel's writer posted again under an owned gateway, so its pause is over.
pub(crate) fn gateway_resumed(channel: u64) {
    PROCESS_HEALTH.resume_gateway(channel);
}

/// Reason slug for an alarm; NotFound has none because only its frequency is an alarm.
fn alarm_kind(alarm: &WriterAlarm) -> Option<&'static str> {
    Some(match alarm {
        WriterAlarm::Blocked { .. } => "blocked",
        WriterAlarm::PausedNoGateway => PAUSED_NO_GATEWAY,
        WriterAlarm::SchemaBlocked { .. } => "schema_blocked",
        WriterAlarm::LedgerViolation { .. } => "ledger_violation",
        WriterAlarm::Halted { .. } => "halted",
        WriterAlarm::Released { .. } => "released",
        WriterAlarm::Abandoned { .. } => "abandoned",
        WriterAlarm::ContentTransform { .. } => "content_transform",
        WriterAlarm::Ambiguous { .. } => "ambiguous",
        WriterAlarm::Unresolved { .. } => "unresolved",
        WriterAlarm::SpoolFull => "spool_full",
        WriterAlarm::BindingGap { .. } => "binding_gap",
        WriterAlarm::BindingPending { .. } => "binding_pending",
        WriterAlarm::BoundaryPending { .. } => "boundary_pending",
        WriterAlarm::SourceStillGrowing { .. } => "source_still_growing",
        WriterAlarm::TooManyReaders { .. } => "too_many_readers",
        WriterAlarm::RetiredSourceGrew { .. } => "retired_source_grew",
        WriterAlarm::BindingLogUnavailable { .. } => "binding_log_unavailable",
        WriterAlarm::RotationStalled { .. } => "rotation_stalled",
        WriterAlarm::SelectionMissing => "selection_missing",
        WriterAlarm::NotFound { .. } => return None,
    })
}

const NOT_FOUND_FREQUENT: &str = "not_found_frequent";
const RESUME_PENDING: &str = "resume_pending";
const PAUSED_NO_GATEWAY: &str = "paused_no_gateway";

/// The writer's alarm sink: each (channel, kind) alarms once, NotFound only past its frequency.
pub(crate) struct AlarmRouter {
    alert_channel: Option<u64>,
    notifier: Option<Arc<dyn AlarmNotifier>>,
    health: Arc<AlarmHealth>,
}

impl AlarmRouter {
    pub(crate) fn new(
        alert_channel: Option<u64>,
        notifier: Option<Arc<dyn AlarmNotifier>>,
        health: Arc<AlarmHealth>,
    ) -> Self {
        Self {
            alert_channel,
            notifier,
            health,
        }
    }

    /// Router for this process: `tui_o.alert_channel_id` via the notify bot's outbox when a pool exists.
    pub(crate) fn for_process(config: Option<&TuiOConfig>, pool: Option<PgPool>) -> Self {
        let notifier = pool.map(|pool| Arc::new(OutboxNotifier { pool }) as Arc<dyn AlarmNotifier>);
        let alert_channel = config.and_then(|config| config.alert_channel_id);
        Self::new(alert_channel, notifier, PROCESS_HEALTH.clone())
    }

    pub(crate) fn raise_at(&self, channel: u64, alarm: &WriterAlarm, now: Instant) {
        let kind = match alarm_kind(alarm) {
            Some(kind) => {
                self.health.activate(&format!("tui_o:{kind}:{channel}"));
                kind
            }
            None if self.health.record_not_found(channel, now) => NOT_FOUND_FREQUENT,
            None => return,
        };
        let reason = format!("tui_o:{kind}:{channel}");
        if self.health.latch(&reason) {
            tracing::warn!(channel, kind, ?alarm, "[tui_o] writer alarm");
        }
        let (Some(alert_channel), Some(notifier)) = (self.alert_channel, &self.notifier) else {
            return;
        };
        if alert_channel == channel {
            tracing::warn!(
                channel,
                kind,
                "[tui_o] alert channel is the failing channel; not sent"
            );
            return;
        }
        let Some(attempt) = NotificationAttempt::begin(self.health.clone(), reason) else {
            return;
        };
        notifier.notify(
            alert_channel,
            AlarmNotification {
                channel,
                kind,
                text: format!("[tui_o] {kind} on channel {channel}: {alarm:?}"),
            },
            attempt,
        );
    }
}

impl AlarmSink for AlarmRouter {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.raise_at(channel, &alarm, Instant::now());
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        if count <= super::writer::rotation::MAX_READERS {
            locked(&self.health.active).remove(&format!("tui_o:too_many_readers:{channel}"));
        }
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.health.set_resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.health.clear_halted(channel);
    }
}

impl AlarmSink for Arc<AlarmRouter> {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.as_ref().raise(channel, alarm);
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.as_ref().reconcile_reader_count(channel, count);
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.as_ref().resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.as_ref().halt_cleared(channel);
    }
}

/// Queues the alarm for the notify bot through the message outbox.
struct OutboxNotifier {
    pool: PgPool,
}

impl AlarmNotifier for OutboxNotifier {
    fn notify(&self, alert_channel: u64, alarm: AlarmNotification, attempt: NotificationAttempt) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                alert_channel,
                "[tui_o] no runtime to queue the alarm message"
            );
            return;
        };
        let pool = self.pool.clone();
        runtime.spawn(async move {
            let target = format!("channel:{alert_channel}");
            let reason_code = format!("{ALARM_REASON_CODE}.{}", alarm.kind);
            let session_key = format!("tui_o:{}", alarm.channel);
            let message = crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &alarm.text,
                bot: crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
                source: ALARM_SOURCE,
                reason_code: Some(&reason_code),
                session_key: Some(&session_key),
            };
            match crate::services::message_outbox::enqueue_outbox_best_effort(Some(&pool), message)
                .await
            {
                Ok(true) => attempt.commit(),
                Ok(false) => tracing::warn!(
                    alert_channel,
                    "[tui_o] alarm enqueue suppressed; will retry"
                ),
                Err(error) => {
                    tracing::warn!(alert_channel, %error, "[tui_o] alarm message not queued")
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALERT: u64 = 900;
    const FAILING: u64 = 42;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, String)>>);

    impl AlarmNotifier for Recorder {
        fn notify(
            &self,
            alert_channel: u64,
            alarm: AlarmNotification,
            attempt: NotificationAttempt,
        ) {
            locked(&self.0).push((alert_channel, alarm.text));
            attempt.commit();
        }
    }

    fn router(alert_channel: Option<u64>) -> (AlarmRouter, Arc<Recorder>, Arc<AlarmHealth>) {
        let recorder = Arc::new(Recorder::default());
        let health = Arc::new(AlarmHealth::default());
        let notifier = recorder.clone() as Arc<dyn AlarmNotifier>;
        let router = AlarmRouter::new(alert_channel, Some(notifier), health.clone());
        (router, recorder, health)
    }

    fn sent(recorder: &Recorder) -> Vec<(u64, String)> {
        recorder.0.lock().unwrap().clone()
    }

    #[test]
    fn p2_2_reader_recovery_preserves_history_other_reasons_and_first_notification_latch() {
        let (router, recorder, health) = router(Some(ALERT));
        router.raise(FAILING, WriterAlarm::TooManyReaders { count: 4 });
        router.raise(FAILING + 1, WriterAlarm::TooManyReaders { count: 4 });
        router.raise(FAILING, WriterAlarm::BindingPending { seq: 2 });
        let history = health.history();
        let messages = sent(&recorder);
        router.reconcile_reader_count(FAILING, 3);
        assert_eq!(
            health.current_at(Instant::now()),
            [
                format!("tui_o:binding_pending:{FAILING}"),
                format!("tui_o:too_many_readers:{}", FAILING + 1),
            ]
        );
        assert_eq!(health.history(), history);
        router.raise(FAILING, WriterAlarm::TooManyReaders { count: 4 });
        assert!(
            health
                .current_at(Instant::now())
                .contains(&format!("tui_o:too_many_readers:{FAILING}"))
        );
        assert_eq!(sent(&recorder), messages);
        assert_eq!(health.history(), history);
    }

    #[test]
    fn first_event_raises_health_and_one_operator_message() {
        let (router, recorder, health) = router(Some(ALERT));
        let now = Instant::now();
        let halted = WriterAlarm::Halted {
            detail: "io".into(),
        };
        router.raise_at(FAILING, &halted, now);
        router.raise_at(FAILING, &halted, now);
        router.raise_at(7, &halted, now);
        assert_eq!(health.history(), ["tui_o:halted:42", "tui_o:halted:7"]);
        let sent = sent(&recorder);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent.iter().all(|(channel, _)| *channel == ALERT));
        assert!(sent[0].1.contains("halted on channel 42"), "{sent:?}");
    }

    #[test]
    fn a_released_channel_has_its_own_reason_apart_from_halted() {
        let (router, recorder, health) = router(Some(ALERT));
        let released = WriterAlarm::Released {
            detail: "adoption held: no source is bound".into(),
        };
        router.raise_at(FAILING, &released, Instant::now());
        router.raise_at(FAILING, &released, Instant::now());
        assert_eq!(health.current_at(Instant::now()), ["tui_o:released:42"]);
        let sent = sent(&recorder);
        assert!(
            matches!(sent.as_slice(), [(ALERT, text)] if text.contains("released on channel 42")),
            "{sent:?}"
        );
    }

    #[test]
    fn not_found_alarms_only_at_three_within_an_hour() {
        let (router, recorder, health) = router(Some(ALERT));
        let start = Instant::now();
        let not_found = WriterAlarm::NotFound { serial: 1 };
        router.raise_at(FAILING, &not_found, start);
        router.raise_at(FAILING, &not_found, start + Duration::from_secs(60));
        // The first hit is exactly one window old, so it no longer counts.
        router.raise_at(FAILING, &not_found, start + NOT_FOUND_WINDOW);
        router.raise_at(7, &not_found, start + NOT_FOUND_WINDOW);
        assert!(health.history().is_empty(), "{:?}", health.history());
        assert!(sent(&recorder).is_empty());

        let third = start + NOT_FOUND_WINDOW + Duration::from_secs(59);
        router.raise_at(FAILING, &not_found, third);
        assert_eq!(health.history(), ["tui_o:not_found_frequent:42"]);
        router.raise_at(FAILING, &not_found, third);
        assert_eq!(sent(&recorder).len(), 1);
    }

    #[test]
    fn an_alarm_is_never_sent_to_its_own_channel() {
        let (router, recorder, health) = router(Some(FAILING));
        router.raise_at(FAILING, &WriterAlarm::SpoolFull, Instant::now());
        assert_eq!(health.history(), ["tui_o:spool_full:42"]);
        assert!(sent(&recorder).is_empty());
    }

    #[test]
    fn without_an_alert_channel_or_notifier_alarms_stay_in_health() {
        let (router, recorder, health) = router(None);
        router.raise_at(FAILING, &WriterAlarm::PausedNoGateway, Instant::now());
        assert_eq!(health.history(), ["tui_o:paused_no_gateway:42"]);
        assert!(sent(&recorder).is_empty());

        let health = Arc::new(AlarmHealth::default());
        let router = AlarmRouter::new(Some(ALERT), None, health.clone());
        router.raise_at(
            FAILING,
            &WriterAlarm::Blocked { status: 403 },
            Instant::now(),
        );
        assert_eq!(health.history(), ["tui_o:blocked:42"]);
    }

    #[test]
    fn a_pause_ends_on_gateway_return_while_blocked_and_history_stay() {
        let (router, recorder, health) = router(Some(ALERT));
        let start = Instant::now();
        router.raise_at(FAILING, &WriterAlarm::PausedNoGateway, start);
        router.raise_at(FAILING, &WriterAlarm::Blocked { status: 403 }, start);
        health.resume_gateway(FAILING);
        let later = start + NOT_FOUND_WINDOW * 3;
        assert_eq!(health.current_at(later), ["tui_o:blocked:42"]);

        router.raise_at(FAILING, &WriterAlarm::PausedNoGateway, later);
        assert_eq!(
            health.current_at(later),
            ["tui_o:blocked:42", "tui_o:paused_no_gateway:42"]
        );
        assert_eq!(
            health.history(),
            ["tui_o:blocked:42", "tui_o:paused_no_gateway:42"]
        );
        assert_eq!(
            sent(&recorder).len(),
            2,
            "the latch still sends each kind once"
        );
    }

    #[test]
    fn not_found_frequency_clears_when_its_window_passes_without_new_events() {
        let (router, recorder, health) = router(Some(ALERT));
        let start = Instant::now();
        let not_found = WriterAlarm::NotFound { serial: 1 };
        for offset in 0..3 {
            router.raise_at(FAILING, &not_found, start + Duration::from_secs(offset));
        }
        let frequent = ["tui_o:not_found_frequent:42"];
        assert_eq!(health.current_at(start + Duration::from_secs(2)), frequent);
        let last_moment = start + NOT_FOUND_WINDOW - Duration::from_secs(1);
        assert_eq!(health.current_at(last_moment), frequent);
        // The first hit leaves the window here and no event arrives to re-count it.
        assert!(health.current_at(start + NOT_FOUND_WINDOW).is_empty());
        assert_eq!(health.history(), frequent);

        let again = start + NOT_FOUND_WINDOW * 2;
        for offset in 0..3 {
            router.raise_at(FAILING, &not_found, again + Duration::from_secs(offset));
        }
        assert_eq!(health.current_at(again + Duration::from_secs(2)), frequent);
        assert_eq!(sent(&recorder).len(), 1);
    }

    #[test]
    fn a_pending_resume_shows_until_settled_and_a_cleared_halt_leaves_every_other_reason() {
        let (router, recorder, health) = router(Some(ALERT));
        // The gateway host holds its router in an `Arc`, so the reports must reach it through one.
        let router = Arc::new(router);
        let halted = || WriterAlarm::Halted {
            detail: "spool append".into(),
        };
        router.raise(FAILING, halted());
        router.raise(FAILING + 1, halted());
        router.raise(FAILING, WriterAlarm::SpoolFull);
        let messages = sent(&recorder);
        router.resume_pending(FAILING, true);
        let reasons = |names: &[&str]| {
            names
                .iter()
                .map(|name| format!("tui_o:{name}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            health.current_at(Instant::now()),
            reasons(&[
                "halted:42",
                "halted:43",
                "resume_pending:42",
                "spool_full:42"
            ])
        );
        router.halt_cleared(FAILING);
        router.resume_pending(FAILING, false);
        assert_eq!(
            health.current_at(Instant::now()),
            reasons(&["halted:43", "spool_full:42"])
        );
        assert_eq!(sent(&recorder), messages, "resume reports send nothing");
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::*;

    async fn row_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(ALARM_SOURCE)
            .fetch_one(pool)
            .await
            .expect("count actual alarm outbox rows")
    }

    #[tokio::test]
    async fn distinct_channels_and_kinds_enqueue_once_pg() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_o_alarm_identity",
            "O alarm outbox identity",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let health = Arc::new(AlarmHealth::default());
        let router = Arc::new(AlarmRouter::new(
            Some(900),
            Some(Arc::new(OutboxNotifier { pool: pool.clone() })),
            health.clone(),
        ));
        let mut tasks = Vec::new();
        for (channel, alarm) in [
            (42, WriterAlarm::Blocked { status: 403 }),
            (7, WriterAlarm::Blocked { status: 403 }),
            (
                42,
                WriterAlarm::Halted {
                    detail: "io".into(),
                },
            ),
            (42, WriterAlarm::Blocked { status: 403 }),
        ] {
            let router = router.clone();
            tasks.push(tokio::spawn(async move { router.raise(channel, alarm) }));
        }
        for task in tasks {
            task.await.expect("concurrent alarm producer");
        }
        settled(&health).await;
        let destinations: Vec<(String, String)> =
            sqlx::query_as("SELECT target, bot FROM message_outbox WHERE source = $1")
                .bind(ALARM_SOURCE)
                .fetch_all(&pool)
                .await
                .expect("read actual alarm destinations");
        assert!(
            destinations
                .iter()
                .all(|(target, bot)| target == "channel:900" && bot == "notify")
        );
        let actual = row_count(&pool).await;
        pool.close().await;
        db.drop().await;
        assert_eq!(
            actual, 3,
            "different channel/kind identities each need a PG row"
        );
        assert_eq!(health.history().len(), 3);
    }

    async fn settled(health: &AlarmHealth) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !locked(&health.notifications)
                    .values()
                    .any(|state| *state == NotificationState::Pending)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual outbox enqueue attempt finished");
    }

    fn pg_router(pool: &PgPool, health: Arc<AlarmHealth>) -> Arc<AlarmRouter> {
        Arc::new(AlarmRouter::new(
            Some(900),
            Some(Arc::new(OutboxNotifier { pool: pool.clone() })),
            health,
        ))
    }

    #[tokio::test]
    async fn suppressed_failed_and_runtime_missing_enqueues_retry_pg() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_o_alarm_retry",
            "O alarm outbox retry",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        assert!(
            crate::services::message_outbox::enqueue_outbox_best_effort(
                Some(&pool),
                crate::services::message_outbox::OutboxMessage {
                    target: "channel:900",
                    content: "old alarm row",
                    bot: "notify",
                    source: ALARM_SOURCE,
                    reason_code: Some(ALARM_REASON_CODE),
                    session_key: None,
                },
            )
            .await
            .expect("seed the previous alarm identity")
        );

        let health = Arc::new(AlarmHealth::default());
        let router = pg_router(&pool, health.clone());
        let no_runtime = router.clone();
        std::thread::spawn(move || no_runtime.raise(42, WriterAlarm::PausedNoGateway))
            .join()
            .expect("alarm observation outside a runtime");
        assert_eq!(row_count(&pool).await, 1);
        router.raise(42, WriterAlarm::PausedNoGateway);
        settled(&health).await;
        assert_eq!(
            row_count(&pool).await,
            2,
            "old identity does not suppress the new alarm"
        );

        let restarted_health = Arc::new(AlarmHealth::default());
        let restarted = pg_router(&pool, restarted_health.clone());
        restarted.raise(42, WriterAlarm::PausedNoGateway);
        settled(&restarted_health).await;
        assert_eq!(
            row_count(&pool).await,
            2,
            "PG suppressed a replay after restart"
        );
        sqlx::query("UPDATE message_outbox SET status = 'failed' WHERE id = (SELECT MAX(id) FROM message_outbox)")
            .execute(&pool).await.expect("remove the fixture duplicate from the active set");
        restarted.raise(42, WriterAlarm::PausedNoGateway);
        settled(&restarted_health).await;
        assert_eq!(
            row_count(&pool).await,
            3,
            "NoRow did not consume the notification latch"
        );
        restarted.raise(42, WriterAlarm::PausedNoGateway);
        settled(&restarted_health).await;
        assert_eq!(
            row_count(&pool).await,
            3,
            "successful enqueue latches the same event"
        );

        sqlx::query("ALTER TABLE message_outbox ADD CONSTRAINT alarm_fixture_failure CHECK (content NOT LIKE '%channel 77:%')")
            .execute(&pool).await.expect("inject a real PG insert failure");
        restarted.raise(77, WriterAlarm::SpoolFull);
        settled(&restarted_health).await;
        assert_eq!(row_count(&pool).await, 3);
        assert_eq!(
            restarted_health.current_at(Instant::now()),
            ["tui_o:paused_no_gateway:42", "tui_o:spool_full:77"]
        );
        sqlx::query("ALTER TABLE message_outbox DROP CONSTRAINT alarm_fixture_failure")
            .execute(&pool)
            .await
            .expect("repair the isolated PG fixture");
        restarted.raise(77, WriterAlarm::SpoolFull);
        settled(&restarted_health).await;
        assert_eq!(
            row_count(&pool).await,
            4,
            "PG error did not consume the notification latch"
        );
        pool.close().await;
        db.drop().await;
    }
}
