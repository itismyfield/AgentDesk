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

/// Outbox source label and reason code for operator-channel alarm messages.
pub(crate) const ALARM_SOURCE: &str = "tui_o_alarm";
pub(crate) const ALARM_REASON_CODE: &str = "tui_o.writer_alarm";

/// Sends one alarm line to the operator channel.
pub(crate) trait AlarmNotifier: Send + Sync {
    fn notify(&self, alert_channel: u64, text: String);
}

/// Degraded reasons raised by alarms, kept until the process restarts.
#[derive(Default)]
pub(crate) struct AlarmHealth {
    reasons: Mutex<BTreeSet<String>>,
}

impl AlarmHealth {
    fn insert(&self, reason: String) -> bool {
        self.reasons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(reason)
    }

    pub(crate) fn reasons(&self) -> Vec<String> {
        self.reasons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

static PROCESS_HEALTH: LazyLock<Arc<AlarmHealth>> = LazyLock::new(Arc::default);

/// Alarm reasons for this process's health snapshot, as `tui_o:<kind>:<channel>`.
pub(crate) fn health_reasons() -> Vec<String> {
    PROCESS_HEALTH.reasons()
}

/// Reason slug for an alarm; NotFound has none because only its frequency is an alarm.
fn alarm_kind(alarm: &WriterAlarm) -> Option<&'static str> {
    Some(match alarm {
        WriterAlarm::Blocked { .. } => "blocked",
        WriterAlarm::PausedNoGateway => "paused_no_gateway",
        WriterAlarm::SchemaBlocked { .. } => "schema_blocked",
        WriterAlarm::LedgerViolation { .. } => "ledger_violation",
        WriterAlarm::Halted { .. } => "halted",
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
        WriterAlarm::NotFound { .. } => return None,
    })
}

const NOT_FOUND_FREQUENT: &str = "not_found_frequent";

/// The writer's alarm sink: each (channel, kind) alarms once, NotFound only past its frequency.
pub(crate) struct AlarmRouter {
    alert_channel: Option<u64>,
    notifier: Option<Arc<dyn AlarmNotifier>>,
    health: Arc<AlarmHealth>,
    not_found: Mutex<HashMap<u64, VecDeque<Instant>>>,
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
            not_found: Mutex::default(),
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
            Some(kind) => kind,
            None if self.not_found_crossed(channel, now) => NOT_FOUND_FREQUENT,
            None => return,
        };
        if !self.health.insert(format!("tui_o:{kind}:{channel}")) {
            return;
        }
        tracing::warn!(channel, kind, ?alarm, "[tui_o] writer alarm");
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
        notifier.notify(
            alert_channel,
            format!("[tui_o] {kind} on channel {channel}: {alarm:?}"),
        );
    }

    /// Records one NotFound and reports whether the channel reached the threshold within the window.
    fn not_found_crossed(&self, channel: u64, now: Instant) -> bool {
        let mut not_found = self
            .not_found
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let hits = not_found.entry(channel).or_default();
        while hits
            .front()
            .is_some_and(|hit| now.saturating_duration_since(*hit) >= NOT_FOUND_WINDOW)
        {
            hits.pop_front();
        }
        hits.push_back(now);
        hits.len() >= NOT_FOUND_THRESHOLD
    }
}

impl AlarmSink for AlarmRouter {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.raise_at(channel, &alarm, Instant::now());
    }
}

impl AlarmSink for Arc<AlarmRouter> {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.as_ref().raise(channel, alarm);
    }
}

/// Queues the alarm for the notify bot through the message outbox.
struct OutboxNotifier {
    pool: PgPool,
}

impl AlarmNotifier for OutboxNotifier {
    fn notify(&self, alert_channel: u64, text: String) {
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
            let message = crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &text,
                bot: crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
                source: ALARM_SOURCE,
                reason_code: Some(ALARM_REASON_CODE),
                session_key: None,
            };
            if let Err(error) =
                crate::services::message_outbox::enqueue_outbox_best_effort(Some(&pool), message)
                    .await
            {
                tracing::warn!(alert_channel, %error, "[tui_o] alarm message not queued");
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
        fn notify(&self, alert_channel: u64, text: String) {
            self.0.lock().unwrap().push((alert_channel, text));
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
    fn first_event_raises_health_and_one_operator_message() {
        let (router, recorder, health) = router(Some(ALERT));
        let now = Instant::now();
        let halted = WriterAlarm::Halted {
            detail: "io".into(),
        };
        router.raise_at(FAILING, &halted, now);
        router.raise_at(FAILING, &halted, now);
        router.raise_at(7, &halted, now);
        assert_eq!(health.reasons(), ["tui_o:halted:42", "tui_o:halted:7"]);
        let sent = sent(&recorder);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent.iter().all(|(channel, _)| *channel == ALERT));
        assert!(sent[0].1.contains("halted on channel 42"), "{sent:?}");
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
        assert!(health.reasons().is_empty(), "{:?}", health.reasons());
        assert!(sent(&recorder).is_empty());

        let third = start + NOT_FOUND_WINDOW + Duration::from_secs(59);
        router.raise_at(FAILING, &not_found, third);
        assert_eq!(health.reasons(), ["tui_o:not_found_frequent:42"]);
        router.raise_at(FAILING, &not_found, third);
        assert_eq!(sent(&recorder).len(), 1);
    }

    #[test]
    fn an_alarm_is_never_sent_to_its_own_channel() {
        let (router, recorder, health) = router(Some(FAILING));
        router.raise_at(FAILING, &WriterAlarm::SpoolFull, Instant::now());
        assert_eq!(health.reasons(), ["tui_o:spool_full:42"]);
        assert!(sent(&recorder).is_empty());
    }

    #[test]
    fn without_an_alert_channel_or_notifier_alarms_stay_in_health() {
        let (router, recorder, health) = router(None);
        router.raise_at(FAILING, &WriterAlarm::PausedNoGateway, Instant::now());
        assert_eq!(health.reasons(), ["tui_o:paused_no_gateway:42"]);
        assert!(sent(&recorder).is_empty());

        let health = Arc::new(AlarmHealth::default());
        let router = AlarmRouter::new(Some(ALERT), None, health.clone());
        router.raise_at(
            FAILING,
            &WriterAlarm::Blocked { status: 403 },
            Instant::now(),
        );
        assert_eq!(health.reasons(), ["tui_o:blocked:42"]);
    }
}
