use chrono::{DateTime, TimeZone, Utc};

use super::super::super::clock::{CatchUpClock, SystemClock};
use super::*;

struct FakeClock {
    pair: Mutex<(DateTime<Utc>, Instant)>,
    samples: AtomicUsize,
}

impl FakeClock {
    fn new(wall: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self {
            pair: Mutex::new((wall, Instant::now())),
            samples: AtomicUsize::new(0),
        })
    }

    fn fixed() -> Arc<Self> {
        let wall = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let (system_wall, _) = SystemClock.sample();
        assert!(system_wall.signed_duration_since(wall).num_days() > 2);
        Self::new(wall)
    }

    fn pair(&self) -> (DateTime<Utc>, Instant) {
        *self.pair.lock().unwrap()
    }

    fn advance(&self, secs: u64) {
        let mut pair = self.pair.lock().unwrap();
        pair.0 += chrono::Duration::seconds(i64::try_from(secs).unwrap());
        pair.1 += Duration::from_secs(secs);
    }

    fn samples(&self) -> usize {
        self.samples.load(Ordering::SeqCst)
    }
}

impl CatchUpClock for FakeClock {
    fn sample(&self) -> (DateTime<Utc>, Instant) {
        self.samples.fetch_add(1, Ordering::SeqCst);
        self.pair()
    }
}

#[derive(Default)]
struct FetchAction {
    advance_secs: u64,
    publish: Option<CatchUpRetryState>,
}

#[derive(Clone, Debug)]
struct AgeFetchRecord {
    request: CatchUpFetchRequest,
    before: HashMap<ChannelId, CatchUpRetryState>,
    after: HashMap<ChannelId, CatchUpRetryState>,
    clock: (DateTime<Utc>, Instant),
}

struct AgeApi {
    strict: StrictApi,
    clock: Arc<FakeClock>,
    phase2_action: Mutex<Option<FetchAction>>,
    fetches: Mutex<Vec<AgeFetchRecord>>,
}

impl AgeApi {
    fn new(strict: StrictApi, clock: &Arc<FakeClock>) -> Self {
        Self {
            strict,
            clock: Arc::clone(clock),
            phase2_action: Mutex::new(None),
            fetches: Mutex::new(Vec::new()),
        }
    }

    fn on_phase2(self, action: FetchAction) -> Self {
        *self.phase2_action.lock().unwrap() = Some(action);
        self
    }

    fn pending(&self) -> HashMap<ChannelId, CatchUpRetryState> {
        self.strict
            .shared
            .catch_up_retry_pending
            .iter()
            .map(|entry| (*entry.key(), *entry.value()))
            .collect()
    }

    fn phase2_fetch(&self) -> AgeFetchRecord {
        self.fetches
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.request == CatchUpFetchRequest::new(20))
            .expect("actual phase2 fetch")
            .clone()
    }

    fn assert_phase2_only(&self, checkpoint: MessageId, message_id: MessageId) {
        assert_eq!(
            self.strict.fetch_log(),
            [
                (message_channel(&self.strict), after(checkpoint), vec![]),
                (
                    message_channel(&self.strict),
                    CatchUpFetchRequest::new(20),
                    vec![message_id.get(), checkpoint.get()],
                ),
            ],
            "empty After page must leave M to the actual phase2 path"
        );
    }
}

fn message_channel(api: &StrictApi) -> ChannelId {
    assert_eq!(api.channels.len(), 1);
    *api.channels.keys().next().unwrap()
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for AgeApi {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        self.strict.current_user_id().await
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        self.strict
            .resolve_runtime_channel_binding_status(channel_id)
            .await
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String> {
        let before = self.pending();
        let action = if request == CatchUpFetchRequest::new(20) {
            self.phase2_action.lock().unwrap().take()
        } else {
            None
        };
        if let Some(action) = action {
            self.clock.advance(action.advance_secs);
            if let Some(state) = action.publish {
                super::super::super::arm_catch_up_retry_state(
                    &self.strict.shared,
                    channel_id,
                    state,
                );
            }
        }
        self.fetches.lock().unwrap().push(AgeFetchRecord {
            request,
            before,
            after: self.pending(),
            clock: self.clock.pair(),
        });
        self.strict.fetch_messages(channel_id, request).await
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        self.strict
            .cleanup_recovered_catch_up_hourglass(shared, channel_id, message_id)
            .await;
    }

    async fn enqueue_intervention(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        intervention: Intervention,
        observed: ClaimObservation,
    ) -> MailboxEnqueueOutcome {
        self.strict
            .enqueue_intervention(shared, provider, channel_id, intervention, observed)
            .await
    }
}

fn at(wall: DateTime<Utc>, sequence: u64, age_secs: i64) -> MessageId {
    let timestamp = (wall - chrono::Duration::seconds(age_secs)).timestamp_millis();
    let discord_ms = u64::try_from(timestamp - 1_420_070_400_000).unwrap();
    MessageId::new((discord_ms << 22) | sequence)
}

fn phase2_api(
    fx: &Fixture,
    clock: &Arc<FakeClock>,
    channel_id: ChannelId,
    checkpoint: MessageId,
    message_id: MessageId,
    defer: bool,
) -> AgeApi {
    let strict = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![own_reply(channel_id, checkpoint)])
        .arriving_at(1, human(channel_id, message_id));
    let strict = if defer {
        strict.with_hooks(message_id, &[Hook::Defer])
    } else {
        strict
    };
    AgeApi::new(strict, clock)
}

async fn sweep(fx: &Fixture, api: &AgeApi, retry_channel: Option<ChannelId>) {
    let pending = retry_channel.into_iter().collect();
    let deps = CatchUpDeps::new(api, &fx.shared, &fx.provider)
        .with_clock(api.clock.as_ref())
        .with_pending_retry_channels(&pending);
    run_catch_up_sweep(deps).await;
}

async fn fill_queue(fx: &Fixture, channel_id: ChannelId, wall: DateTime<Utc>) {
    for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
        let id = at(wall, 100 + index as u64, 10);
        let intervention = queued_intervention(id, index);
        let outcome = discord::mailbox_enqueue_intervention(
            &fx.shared,
            &fx.provider,
            channel_id,
            intervention,
        )
        .await;
        assert!(super::super::super::catch_up_enqueue_accepted(&outcome));
    }
    assert_eq!(
        discord::mailbox_snapshot(&fx.shared, channel_id)
            .await
            .intervention_queue
            .len(),
        MAX_INTERVENTIONS_PER_CHANNEL
    );
}

fn assert_too_old_warn(logs: &str, channel_id: ChannelId, message_id: MessageId) {
    let mut events = logs
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("catch_up_age_out"));
    let event = events.next().expect("actual structured phase2 age WARN");
    assert!(events.next().is_none(), "one WARN per age-out handling");
    for field in [
        "phase=\"phase2\"".to_owned(),
        format!("channel_id={}", channel_id.get()),
        format!("message_id={}", message_id.get()),
        "age_secs=601".to_owned(),
        "max_age_secs=600".to_owned(),
        "reason=\"catch_up_age_out\"".to_owned(),
    ] {
        assert!(event.contains(&field), "missing {field}: {event}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn t17_scan_age_is_fixed_before_phase2_fetch_crosses_600() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let (wall, instant) = clock.pair();
    let channel_id = ChannelId::new(4_604_317);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 599));
    fx.seed_checkpoint(channel_id, b);
    let api = phase2_api(&fx, &clock, channel_id, b, m, false).on_phase2(FetchAction {
        advance_secs: 2,
        ..FetchAction::default()
    });

    sweep(&fx, &api, None).await;

    api.assert_phase2_only(b, m);
    assert_eq!(api.strict.enqueue_log(), [(m.get(), true, None)]);
    assert_eq!(fx.surfaces(channel_id), (Some(m.get()), Some(m.get())));
    assert_eq!(clock.samples(), 1, "with_clock must sample the scan once");
    assert_eq!(api.phase2_fetch().clock.1, instant + Duration::from_secs(2));
    assert_eq!(
        clock
            .pair()
            .0
            .signed_duration_since(*m.created_at())
            .num_seconds(),
        601
    );
}

#[tokio::test(flavor = "current_thread")]
async fn t18_consumed_deferred_origin_recovers_at_raw_age_650() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let (wall, instant) = clock.pair();
    let channel_id = ChannelId::new(4_604_318);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 250));
    fx.seed_checkpoint(channel_id, b);
    let first = phase2_api(&fx, &clock, channel_id, b, m, true);
    sweep(&fx, &first, None).await;
    first.assert_phase2_only(b, m);
    let origin = fx.pending(channel_id).expect("actual Deferred arms S");
    assert_eq!(origin.armed_at, instant);
    assert_eq!(origin.deferred_rearms, 1);
    assert_eq!(clock.samples(), 2, "scan and actual first arm sample");
    clock.advance(400);
    let second = phase2_api(&fx, &clock, channel_id, b, m, false);

    sweep(&fx, &second, Some(channel_id)).await;

    second.assert_phase2_only(b, m);
    assert!(
        second.phase2_fetch().before.is_empty(),
        "S was actually consumed"
    );
    assert_eq!(second.strict.enqueue_log(), [(m.get(), true, None)]);
    assert_eq!(fx.pending(channel_id), None);
    assert_eq!(fx.surfaces(channel_id), (Some(m.get()), Some(m.get())));
    assert_eq!(clock.samples(), 3);
    assert_eq!(
        clock
            .pair()
            .0
            .signed_duration_since(*m.created_at())
            .num_seconds(),
        650
    );
}

#[tokio::test(flavor = "current_thread")]
async fn t19a_capacity_keeps_origin_and_deferred_budget_between_real_consumes() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let (wall, instant) = clock.pair();
    let channel_id = ChannelId::new(4_604_319);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 250));
    fx.seed_checkpoint(channel_id, b);
    let first = phase2_api(&fx, &clock, channel_id, b, m, true);
    sweep(&fx, &first, None).await;
    first.assert_phase2_only(b, m);
    let first_state = fx.pending(channel_id).expect("Deferred state");
    assert_eq!(
        (first_state.armed_at, first_state.deferred_rearms),
        (instant, 1)
    );
    fill_queue(&fx, channel_id, wall).await;
    clock.advance(400);
    let capped = phase2_api(&fx, &clock, channel_id, b, m, false);
    sweep(&fx, &capped, Some(channel_id)).await;
    capped.assert_phase2_only(b, m);
    assert!(capped.phase2_fetch().before.is_empty());
    assert!(capped.strict.enqueue_log().is_empty(), "real capacity gate");
    assert_eq!(
        fx.pending(channel_id),
        Some(first_state),
        "capacity inherits all S fields"
    );
    let cleared = discord::mailbox_clear_channel(&fx.shared, &fx.provider, channel_id).await;
    assert!(cleared.persistence_error.is_none());
    assert_eq!(
        cleared
            .discarded_message_ids
            .iter()
            .map(|id| id.get())
            .collect::<HashSet<_>>()
            .len(),
        MAX_INTERVENTIONS_PER_CHANNEL
    );
    assert!(
        discord::mailbox_snapshot(&fx.shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
    let deferred = phase2_api(&fx, &clock, channel_id, b, m, true);

    sweep(&fx, &deferred, Some(channel_id)).await;

    deferred.assert_phase2_only(b, m);
    assert!(deferred.phase2_fetch().before.is_empty());
    let final_state = fx.pending(channel_id).expect("second Deferred rearm");
    assert_eq!(final_state.armed_at, instant);
    assert_eq!(final_state.fetch_failures, first_state.fetch_failures);
    assert_eq!(
        final_state.deferred_rearms, 2,
        "Deferred count is 1 -> 1 -> 2"
    );
    assert_eq!(final_state.checkpoint, b.get());
    assert_eq!(
        clock.samples(),
        4,
        "existing origin must not resample an arm"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn t19b_phase1_capacity_publishes_inherited_state_before_phase2_fetch() {
    let fx = Fixture::new().await;
    let (wall, _) = SystemClock.sample();
    let clock = FakeClock::new(wall);
    let instant = clock.pair().1;
    let channel_id = ChannelId::new(4_604_329);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 30));
    fx.seed_checkpoint(channel_id, b);
    let consumed = CatchUpRetryState {
        checkpoint: b.get(),
        fetch_failures: 2,
        deferred_rearms: 3,
        armed_at: instant - Duration::from_secs(120),
    };
    fx.shared
        .catch_up_retry_pending
        .insert(channel_id, consumed);
    fill_queue(&fx, channel_id, wall).await;
    let strict = StrictApi::new(&fx.shared).with_history(
        channel_id,
        vec![own_reply(channel_id, b), human(channel_id, m)],
    );
    let api = AgeApi::new(strict, &clock);

    sweep(&fx, &api, Some(channel_id)).await;

    assert_phase1_read(&api.strict, after(b), &[m]);
    assert_eq!(
        api.phase2_fetch().before.get(&channel_id),
        Some(&consumed),
        "phase1 publish before phase2 can mask it"
    );
    assert_eq!(fx.pending(channel_id), Some(consumed));
    assert!(api.strict.enqueue_log().is_empty());
    assert_eq!(
        clock.samples(),
        1,
        "capacity with prior must preserve its arm"
    );
}

async fn assert_consumed_and_published_prior(early_consumed: bool) {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let (wall, instant) = clock.pair();
    let channel_id = ChannelId::new(if early_consumed { 4_604_321 } else { 4_604_320 });
    let (a, b, m) = (at(wall, 1, 1000), at(wall, 2, 900), at(wall, 3, 650));
    fx.seed_checkpoint(channel_id, b);
    let early = instant - Duration::from_secs(100);
    let late = instant - Duration::from_secs(40);
    let consumed = CatchUpRetryState {
        checkpoint: b.get(),
        fetch_failures: 4,
        deferred_rearms: 2,
        armed_at: if early_consumed { early } else { late },
    };
    let published = CatchUpRetryState {
        checkpoint: a.get(),
        fetch_failures: 1,
        deferred_rearms: 5,
        armed_at: if early_consumed { late } else { early },
    };
    fx.shared
        .catch_up_retry_pending
        .insert(channel_id, consumed);
    let api = phase2_api(&fx, &clock, channel_id, b, m, true).on_phase2(FetchAction {
        publish: Some(published),
        ..FetchAction::default()
    });

    sweep(&fx, &api, Some(channel_id)).await;

    api.assert_phase2_only(b, m);
    let fetch = api.phase2_fetch();
    assert!(fetch.before.is_empty(), "C was consumed before P publish");
    assert_eq!(fetch.after.get(&channel_id), Some(&published));
    assert_eq!(
        api.strict.enqueue_log(),
        [(m.get(), false, Some(EnqueueRefusalReason::ActorUnreachable))]
    );
    assert_eq!(
        fx.pending(channel_id),
        Some(CatchUpRetryState {
            checkpoint: a.get(),
            fetch_failures: 4,
            deferred_rearms: 6,
            armed_at: early,
        }),
        "min checkpoint/origin and max separate budgets"
    );
    assert_eq!(clock.samples(), 1);
    assert_eq!(
        wall.signed_duration_since(*m.created_at()).num_seconds(),
        650
    );
    assert_eq!(
        (wall - chrono::Duration::seconds(40))
            .signed_duration_since(*m.created_at())
            .num_seconds(),
        610
    );
    assert_eq!(
        (wall - chrono::Duration::seconds(100))
            .signed_duration_since(*m.created_at())
            .num_seconds(),
        550
    );
}

#[tokio::test(flavor = "current_thread")]
async fn t20a_published_earlier_origin_merges_with_consumed_budgets() {
    assert_consumed_and_published_prior(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t20b_consumed_earlier_origin_merges_with_published_budgets() {
    assert_consumed_and_published_prior(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t21a_600_seconds_without_retry_is_accepted() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let wall = clock.pair().0;
    let channel_id = ChannelId::new(4_604_331);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 600));
    fx.seed_checkpoint(channel_id, b);
    let api = phase2_api(&fx, &clock, channel_id, b, m, false);

    sweep(&fx, &api, None).await;

    api.assert_phase2_only(b, m);
    assert_eq!(api.strict.enqueue_log(), [(m.get(), true, None)]);
    assert_eq!(fx.surfaces(channel_id), (Some(m.get()), Some(m.get())));
    assert_eq!(clock.samples(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn t21b_601_seconds_without_retry_keeps_checkpoint_and_warns() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let wall = clock.pair().0;
    let channel_id = ChannelId::new(4_604_332);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 601));
    fx.seed_checkpoint(channel_id, b);
    let api = phase2_api(&fx, &clock, channel_id, b, m, false);
    let logs = LogWriter::capture();

    sweep(&fx, &api, None).await;

    let logs = logs.finish();
    api.assert_phase2_only(b, m);
    assert!(api.strict.enqueue_log().is_empty());
    assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
    assert_eq!(fx.pending(channel_id), None);
    assert_eq!(clock.samples(), 1);
    assert_too_old_warn(&logs, channel_id, m);
}

#[tokio::test(flavor = "current_thread")]
async fn t22_first_deferred_arm_uses_actual_post_fetch_instant() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let (wall, instant) = clock.pair();
    let channel_id = ChannelId::new(4_604_322);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 599));
    fx.seed_checkpoint(channel_id, b);
    let first = phase2_api(&fx, &clock, channel_id, b, m, true).on_phase2(FetchAction {
        advance_secs: 2,
        ..FetchAction::default()
    });
    sweep(&fx, &first, None).await;
    first.assert_phase2_only(b, m);
    let armed = fx.pending(channel_id).expect("fetch-crossing Deferred arm");
    assert_eq!(armed.armed_at, instant + Duration::from_secs(2));
    assert_eq!(armed.armed_at, first.phase2_fetch().clock.1);
    assert_eq!(armed.deferred_rearms, 1);
    assert_eq!(clock.samples(), 2);
    clock.advance(400);
    let second = phase2_api(&fx, &clock, channel_id, b, m, false);
    let logs = LogWriter::capture();

    sweep(&fx, &second, Some(channel_id)).await;

    let logs = logs.finish();
    second.assert_phase2_only(b, m);
    assert!(second.phase2_fetch().before.is_empty());
    assert!(
        second.strict.enqueue_log().is_empty(),
        "age at actual arm was already 601"
    );
    assert_eq!(fx.pending(channel_id), None);
    assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
    assert_eq!(clock.samples(), 3);
    assert_too_old_warn(&logs, channel_id, m);
}

#[tokio::test(flavor = "current_thread")]
async fn t23_phase2_age_warn_has_structured_identity_without_message_body() {
    let fx = Fixture::new().await;
    let clock = FakeClock::fixed();
    let wall = clock.pair().0;
    let channel_id = ChannelId::new(4_604_323);
    let (b, m) = (at(wall, 1, 900), at(wall, 2, 601));
    fx.seed_checkpoint(channel_id, b);
    let sentinel = "AGE_WARN_BODY_SENTINEL_6043";
    let strict = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![own_reply(channel_id, b)])
        .arriving_at(1, discord_message(channel_id, m, HUMAN_ID, false, sentinel));
    let api = AgeApi::new(strict, &clock);
    let logs = LogWriter::capture();

    sweep(&fx, &api, None).await;

    let logs = logs.finish();
    api.assert_phase2_only(b, m);
    assert_too_old_warn(&logs, channel_id, m);
    assert!(
        !logs.contains(sentinel),
        "WARN must exclude message body: {logs}"
    );
    assert!(api.strict.enqueue_log().is_empty());
    assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
    assert_eq!(clock.samples(), 1);
}
