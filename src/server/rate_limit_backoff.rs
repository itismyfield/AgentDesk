//! Backoff schedule for the Claude leg of `rate_limit_sync_loop`. Pure (no
//! clock, no I/O): the caller injects `now`, so the schedule is testable. The
//! loop's fixed 120 s cadence drew 429s on ~31% of polls in production.
//!
//! Policy: success → base interval (120 s), counters reset; 429 with a usable
//! `Retry-After` → that long, clamped to the max; 429 without one → exponential
//! 120/240/480 s … capped at 30 min; any other error → base interval. Only the
//! Claude fetch is skipped while `not_before` is in the future. That 30-minute
//! ceiling is the *no-pressure* one: while the cached telemetry still defers
//! dispatch the caller drops the hold ([`ClaudeSyncBackoff::release_hold`]), so
//! the leg keeps the base cadence and that pressure stays observable.
use std::time::{Duration, Instant};

pub(crate) const RATE_LIMIT_SYNC_BASE_INTERVAL: Duration = Duration::from_secs(120);
pub(crate) const RATE_LIMIT_SYNC_MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Typed error returned by the Claude usage fetchers on HTTP 429 so the loop
/// can tell it from other failures (via `anyhow::Error::downcast_ref`).
/// `buckets` carries whatever `anthropic-ratelimit-*` telemetry the 429 itself
/// advertised, so scheduling a retry never costs that observation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClaudeUsageRateLimited {
    pub(crate) retry_after: Option<Duration>,
    pub(crate) buckets: Vec<serde_json::Value>,
}

impl std::fmt::Display for ClaudeUsageRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Claude usage fetch rate limited (429")?;
        match self.retry_after {
            Some(after) => write!(f, ", retry-after {}s)", after.as_secs()),
            None => write!(f, ")"),
        }
    }
}

impl std::error::Error for ClaudeUsageRateLimited {}

/// Parses an HTTP `Retry-After`: delta-seconds or an HTTP-date (RFC 7231
/// IMF-fixdate, which `chrono`'s RFC 2822 parser accepts). `now` is injected so
/// date forms are testable; unparseable or past values return `None`.
pub(crate) fn parse_retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let delta = at.with_timezone(&chrono::Utc) - now;
    delta.to_std().ok()
}

/// Outcome of one Claude rate-limit fetch, as classified by the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeSyncOutcome {
    Success,
    RateLimited { retry_after: Option<Duration> },
    OtherError,
}

#[derive(Debug)]
pub(crate) struct ClaudeSyncBackoff {
    base: Duration,
    max: Duration,
    /// Exponential delay to apply on the *next* 429 without `Retry-After`.
    next_exponential: Duration,
    consecutive_rate_limits: u32,
    not_before: Option<Instant>,
}

impl ClaudeSyncBackoff {
    pub(crate) fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max: max.max(base),
            next_exponential: base,
            consecutive_rate_limits: 0,
            not_before: None,
        }
    }

    /// Whether the loop should attempt the Claude fetch on this tick.
    pub(crate) fn should_attempt(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|not_before| now >= not_before)
    }

    /// Remaining hold-off from `now`, for log lines. Zero when not backing off.
    pub(crate) fn remaining(&self, now: Instant) -> Duration {
        self.not_before.map_or(Duration::ZERO, |not_before| {
            not_before.saturating_duration_since(now)
        })
    }

    pub(crate) fn consecutive_rate_limits(&self) -> u32 {
        self.consecutive_rate_limits
    }

    /// Drops an in-progress hold so this tick attempts: never creates one, and
    /// leaves the 429 streak and the ladder alone. The caller does this while
    /// the cached telemetry still defers, since below the base cadence that
    /// pressure cannot be re-observed at all (#5727).
    pub(crate) fn release_hold(&mut self) {
        self.not_before = None;
    }

    /// Records a fetch outcome and returns the delay applied before the next
    /// Claude attempt (the base interval on success / other errors).
    pub(crate) fn record(&mut self, outcome: ClaudeSyncOutcome, now: Instant) -> Duration {
        match outcome {
            ClaudeSyncOutcome::Success => {
                self.next_exponential = self.base;
                self.consecutive_rate_limits = 0;
                self.not_before = None;
                self.base
            }
            ClaudeSyncOutcome::OtherError => {
                // Not a rate limit: keep the cadence, but do not reset an
                // in-progress 429 streak either.
                self.not_before = None;
                self.base
            }
            ClaudeSyncOutcome::RateLimited { retry_after } => {
                self.consecutive_rate_limits = self.consecutive_rate_limits.saturating_add(1);
                let delay = match retry_after {
                    Some(retry_after) => retry_after.clamp(self.base, self.max),
                    None => {
                        let delay = self.next_exponential.min(self.max);
                        self.next_exponential = delay.saturating_mul(2).min(self.max);
                        delay
                    }
                };
                self.not_before = Some(now + delay);
                delay
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff() -> ClaudeSyncBackoff {
        ClaudeSyncBackoff::new(RATE_LIMIT_SYNC_BASE_INTERVAL, RATE_LIMIT_SYNC_MAX_BACKOFF)
    }

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn limited(retry_after: Option<u64>) -> ClaudeSyncOutcome {
        ClaudeSyncOutcome::RateLimited {
            retry_after: retry_after.map(secs),
        }
    }

    #[test]
    fn exponential_backoff_doubles_and_caps_at_thirty_minutes() {
        let mut backoff = backoff();
        let mut at = Instant::now();
        for expected in [120_u64, 240, 480, 960, 1800, 1800] {
            assert_eq!(backoff.record(limited(None), at), secs(expected));
            assert!(!backoff.should_attempt(at + secs(expected - 1)));
            assert!(backoff.should_attempt(at + secs(expected)));
            at += secs(expected);
        }
        assert_eq!(backoff.consecutive_rate_limits(), 6);
        assert_eq!(backoff.remaining(at - secs(1800)), secs(1800));
        assert_eq!(backoff.remaining(at - secs(800)), secs(800));
    }

    #[test]
    fn success_after_backoff_returns_to_base_interval() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        // A fresh schedule is ready immediately and stays at the base interval.
        assert!(backoff.should_attempt(t0));
        assert_eq!(backoff.record(ClaudeSyncOutcome::Success, t0), secs(120));
        backoff.record(limited(None), t0);
        // An unrelated error keeps the cadence and clears the hold, but must
        // not restart the 429 ladder.
        assert_eq!(
            backoff.record(ClaudeSyncOutcome::OtherError, t0 + secs(120)),
            secs(120)
        );
        assert!(backoff.should_attempt(t0 + secs(120)));
        assert_eq!(backoff.consecutive_rate_limits(), 1);
        assert_eq!(backoff.record(limited(None), t0 + secs(120)), secs(240));
        backoff.record(limited(None), t0 + secs(360));
        assert_eq!(backoff.consecutive_rate_limits(), 3);

        // Success mid-hold (not_before is t0+840) releases it immediately.
        let t_ok = t0 + secs(600);
        assert_eq!(backoff.record(ClaudeSyncOutcome::Success, t_ok), secs(120));
        assert!(backoff.should_attempt(t_ok));
        assert_eq!(backoff.consecutive_rate_limits(), 0);
        // The exponential ladder restarts from the base after a success.
        assert_eq!(backoff.record(limited(None), t_ok), secs(120));
        assert_eq!(backoff.consecutive_rate_limits(), 1);
    }

    #[test]
    fn retry_after_is_clamped_and_leaves_the_ladder_alone() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        // Honoured as-is above the base, rounded up below it, clamped at max.
        assert_eq!(backoff.record(limited(Some(300)), t0), secs(300));
        assert!(!backoff.should_attempt(t0 + secs(299)));
        assert!(backoff.should_attempt(t0 + secs(300)));
        assert_eq!(backoff.record(limited(Some(5)), t0), secs(120));
        assert_eq!(backoff.record(limited(Some(86_400)), t0), secs(1800));
        assert_eq!(backoff.consecutive_rate_limits(), 3);
        // A header-driven hold never advances the exponential ladder: the next
        // header-less 429 starts at the base, not at 240 s.
        assert_eq!(backoff.record(limited(None), t0 + secs(1800)), secs(120));
    }

    /// The parser, plus the typed 429's anyhow round-trip and Display.
    #[test]
    fn parses_retry_after_seconds_and_http_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-05T06:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // Unparseable values and dates already past yield no usable delay.
        for (value, expected) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("Sat, 05 Sep 2026 06:05:00 GMT", Some(300)),
            ("Sat, 05 Sep 2026 05:59:00 GMT", None),
            ("", None),
            ("soon", None),
            ("-5", None),
        ] {
            assert_eq!(
                parse_retry_after(value, now),
                expected.map(secs),
                "{value:?}"
            );
        }
        let buckets = Vec::new();
        let retry_after = Some(secs(42));
        let error = anyhow::Error::new(ClaudeUsageRateLimited {
            retry_after,
            buckets,
        });
        let typed = error.downcast_ref::<ClaudeUsageRateLimited>();
        assert_eq!(typed.and_then(|typed| typed.retry_after), retry_after);
        assert!(error.to_string().contains("429, retry-after 42s"));
    }
}
