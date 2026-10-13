//! The shadow worker: reads the home row with PG's clock, samples this node's gate without waiting,
//! judges with the ingress contract and records the result. It writes nothing it reads.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::Descriptor;
use super::telemetry::{Counts, Observed, Record, Sink};
use crate::db::o_channel_homes::{self, ChannelHome, HomeError, HomeProvider};
use crate::services::cluster::channel_home::HomeOwnership;
use crate::services::cluster::channel_home::ingress_snapshot::{self, GateSample};
use crate::services::cluster::channel_ingress::{
    self, HomeRead, IngressHoldReason, IngressKey, IngressObservation, ReadClock, SourceEpoch,
};

/// Pool acquire plus the row query; past it the authority is unknown, never missing.
const LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);
/// How often a close record states the counts so far.
const CLOSE_EVERY: Duration = Duration::from_secs(60);

#[cfg(test)]
pub(super) type FixedRead = Arc<
    dyn Fn(
            u64,
        ) -> futures::future::BoxFuture<
            'static,
            Result<Option<(ChannelHome, DateTime<Utc>)>, HomeError>,
        > + Send
        + Sync,
>;

/// Where home rows come from.
pub(super) enum HomeSource {
    /// A one-connection pool with the runtime pool's connect options; it only reads.
    Pg(PgPool),
    /// No database: every authority is unknown.
    Absent,
    #[cfg(test)]
    Fixed(FixedRead),
}

impl HomeSource {
    /// Opens no connection until the first lookup.
    pub(super) fn of(pool: Option<&PgPool>) -> Self {
        let Some(pool) = pool else {
            return Self::Absent;
        };
        let options = (*pool.connect_options()).clone();
        let observer = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(LOOKUP_TIMEOUT)
            .connect_lazy_with(options);
        Self::Pg(observer)
    }
}

/// Who observes, pinned in the start record and copied into each judged record.
pub(super) struct Scope {
    pub(super) provider: HomeProvider,
    pub(super) bot: String,
    pub(super) generation: u64,
    pub(super) instance_id: String,
    pub(super) allowlist: Vec<u64>,
    /// `None`: the SHA-256 of the running executable.
    pub(super) build: Option<String>,
    pub(super) sink: Sink,
}

enum Lookup {
    NoDatabase,
    TimedOut,
    Failed,
    Missing,
    Row(Box<ChannelHome>, DateTime<Utc>),
}

impl Lookup {
    fn label(&self) -> &'static str {
        match self {
            Self::NoDatabase => "no_database",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
            Self::Missing => "missing",
            Self::Row(..) => "row",
        }
    }
}

async fn lookup(source: &HomeSource, channel: u64) -> Lookup {
    let read = async {
        match source {
            HomeSource::Absent => None,
            HomeSource::Pg(pool) => {
                Some(o_channel_homes::read_home_at(pool, &channel.to_string()).await)
            }
            #[cfg(test)]
            HomeSource::Fixed(read) => Some(read(channel).await),
        }
    };
    match tokio::time::timeout(LOOKUP_TIMEOUT, read).await {
        Err(_) => Lookup::TimedOut,
        Ok(None) => Lookup::NoDatabase,
        Ok(Some(Err(_))) => Lookup::Failed,
        Ok(Some(Ok(None))) => Lookup::Missing,
        Ok(Some(Ok(Some((home, read_at))))) => Lookup::Row(Box::new(home), read_at),
    }
}

/// The running executable's SHA-256, read off the event path; `unknown` when unreadable.
async fn build_digest() -> String {
    let digest = tokio::task::spawn_blocking(|| {
        use sha2::{Digest, Sha256};
        let mut file = std::fs::File::open(std::env::current_exe()?)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok::<_, std::io::Error>(hex::encode(hasher.finalize()))
    });
    match digest.await {
        Ok(Ok(digest)) => digest,
        _ => "unknown".to_owned(),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Judges descriptors until every sender is gone, then closes the pool and states the totals.
pub(super) async fn run(
    scope: Scope,
    source: HomeSource,
    mut rx: mpsc::Receiver<Descriptor>,
    counts: Arc<Counts>,
) {
    let incarnation = uuid::Uuid::new_v4().to_string();
    let build = match scope.build.clone() {
        Some(build) => build,
        None => build_digest().await,
    };
    scope.sink.emit(Record::Start {
        incarnation: incarnation.clone(),
        instance_id: scope.instance_id.clone(),
        build,
        bot: scope.bot.clone(),
        provider: scope.provider.as_str().to_owned(),
        generation: scope.generation,
        allowlist: scope.allowlist.clone(),
        at: Utc::now(),
    });
    let mut close = tokio::time::interval(CLOSE_EVERY);
    close.tick().await;
    loop {
        tokio::select! {
            next = rx.recv() => {
                let Some(descriptor) = next else { break };
                let (observed, unknown) = judge(&scope, &incarnation, &source, descriptor).await;
                scope.sink.emit(Record::Observed(Box::new(observed)));
                if unknown {
                    counts.unknown.fetch_add(1, Ordering::AcqRel);
                }
                counts.completed.fetch_add(1, Ordering::AcqRel);
            }
            _ = close.tick() => scope.sink.emit(counts.close(&incarnation, false)),
        }
    }
    if let HomeSource::Pg(pool) = &source {
        pool.close().await;
    }
    scope.sink.emit(counts.close(&incarnation, true));
}

/// One descriptor: row and PG clock in one read timed from before the pool acquire, then this
/// node's gate keyed by the row's provider, then the contract's verdict.
pub(super) async fn judge(
    scope: &Scope,
    incarnation: &str,
    source: &HomeSource,
    descriptor: Descriptor,
) -> (Observed, bool) {
    let started = Instant::now();
    let found = lookup(source, descriptor.channel.get()).await;
    let looked_up = Instant::now();
    let sample = match &found {
        Lookup::Row(home, _) => Some(ingress_snapshot::try_sample(
            descriptor.channel.get(),
            &home.provider,
        )),
        _ => None,
    };
    let now = Instant::now();
    let home = match &found {
        Lookup::Missing => HomeRead::Missing,
        Lookup::Row(home, read_at) => HomeRead::Row(
            home,
            ReadClock {
                pg_now: Some(*read_at),
                started: Some(started),
            },
        ),
        Lookup::NoDatabase | Lookup::TimedOut | Lookup::Failed => HomeRead::Unreadable,
    };
    let key = IngressKey::new(scope.provider, descriptor.channel);
    let local = sample.as_ref().and_then(GateSample::sample);
    let verdict = channel_ingress::observe(
        &key,
        &scope.instance_id,
        SourceEpoch::UnstampedSource,
        home,
        local,
        now,
    );
    let local_epoch = match local.map(|sample| sample.ownership) {
        Some(HomeOwnership::Owned { home_epoch, .. }) => Some(home_epoch),
        _ => None,
    };
    // A verdict names the row epoch it judged; a hold keeps whatever row was read.
    let (label, row_epoch, reason, holder) = match &verdict {
        IngressObservation::LocalCandidate { home_epoch } => {
            ("local_candidate", Some(*home_epoch), None, None)
        }
        IngressObservation::IgnoreForeign { holder, home_epoch } => (
            "ignore_foreign",
            Some(*home_epoch),
            None,
            Some(holder.clone()),
        ),
        IngressObservation::Hold(why) => {
            let read = match &found {
                Lookup::Row(home, _) => Some(home.epoch),
                _ => None,
            };
            ("hold", read, Some(format!("{why:?}")), None)
        }
    };
    let sampler_unreadable = sample.as_ref().is_some_and(GateSample::unreadable);
    let unknown = matches!(
        verdict,
        IngressObservation::Hold(
            IngressHoldReason::AuthorityUnreadable
                | IngressHoldReason::TimeUnknown
                | IngressHoldReason::NoLocalIdentity
        )
    ) || (sampler_unreadable
        && verdict == IngressObservation::Hold(IngressHoldReason::NoLocalLease));
    let observed = Observed {
        incarnation: incarnation.to_owned(),
        instance_id: scope.instance_id.clone(),
        generation: scope.generation,
        bot: scope.bot.clone(),
        provider: scope.provider.as_str().to_owned(),
        sequence: descriptor.sequence,
        kind: descriptor.kind,
        channel: descriptor.channel.get(),
        source_id: descriptor.source_id,
        application_id: descriptor.application_id,
        queued_ms: millis(started.saturating_duration_since(descriptor.received)),
        lookup_ms: millis(looked_up.saturating_duration_since(started)),
        sample_ms: millis(now.saturating_duration_since(looked_up)),
        lookup: found.label().to_owned(),
        sampler: sample
            .as_ref()
            .map_or("not_sampled", GateSample::label)
            .to_owned(),
        row_epoch,
        local_epoch,
        verdict: label.to_owned(),
        reason,
        holder,
    };
    (observed, unknown)
}
