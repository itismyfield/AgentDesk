//! Backoff schedule for the Claude leg of `rate_limit_sync_loop`.
//!
//! The loop polls `https://api.anthropic.com/api/oauth/usage` on a fixed
//! 120 s tick. In production that endpoint answered 429 for ~31% of polls
//! (3-day sample: 1,980 ok vs 907 rate limited) because the loop kept
//! hammering it at the same cadence after every 429 and warned each time.
//!
//! This module is pure (no clock, no I/O): the caller injects `now` so the
//! schedule can be unit-tested deterministically.
//!
//! Policy:
//! * success → next attempt after the base interval (120 s), counters reset;
//! * 429 with a usable `Retry-After` → wait that long (clamped to the max);
//! * 429 without one → exponential: 120 s, 240 s, 480 s … capped at 30 min;
//! * any other error → base interval (unchanged from before);
//! * the loop keeps ticking every 120 s for the other providers and simply
//!   skips the Claude fetch while `not_before` is in the future, so an
//!   effective wait is rounded up to the next tick.

use std::time::{Duration, Instant};

pub(crate) const RATE_LIMIT_SYNC_BASE_INTERVAL: Duration = Duration::from_secs(120);
pub(crate) const RATE_LIMIT_SYNC_MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Typed error returned by the Claude usage fetchers on HTTP 429 so the loop
/// can distinguish it from other failures (via `anyhow::Error::downcast_ref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeUsageRateLimited {
    pub(crate) retry_after: Option<Duration>,
}

impl std::fmt::Display for ClaudeUsageRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after {
            Some(retry_after) => write!(
                f,
                "Claude OAuth usage API rate limited (429, retry-after {}s)",
                retry_after.as_secs()
            ),
            None => write!(f, "Claude OAuth usage API rate limited (429)"),
        }
    }
}

impl std::error::Error for ClaudeUsageRateLimited {}

/// Parses an HTTP `Retry-After` header value: either delta-seconds or an
/// HTTP-date (RFC 7231 IMF-fixdate, which `chrono`'s RFC 2822 parser accepts).
/// `now` is injected so date-form values are testable. Returns `None` for
/// unparseable values or dates already in the past.
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
                // Not a rate limit: keep the regular cadence but do not
                // reset an in-progress 429 streak either (a transient
                // network error between 429s should not restart at 120 s).
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

    #[test]
    fn starts_ready_and_success_keeps_base_interval() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        assert!(backoff.should_attempt(t0));
        assert_eq!(backoff.record(ClaudeSyncOutcome::Success, t0), secs(120));
        assert!(backoff.should_attempt(t0));
        assert_eq!(backoff.consecutive_rate_limits(), 0);
    }

    #[test]
    fn exponential_backoff_doubles_and_caps_at_thirty_minutes() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        let rate_limited = || ClaudeSyncOutcome::RateLimited { retry_after: None };

        assert_eq!(backoff.record(rate_limited(), t0), secs(120));
        assert!(!backoff.should_attempt(t0));
        assert!(!backoff.should_attempt(t0 + secs(119)));
        assert!(backoff.should_attempt(t0 + secs(120)));

        let t1 = t0 + secs(120);
        assert_eq!(backoff.record(rate_limited(), t1), secs(240));
        let t2 = t1 + secs(240);
        assert_eq!(backoff.record(rate_limited(), t2), secs(480));
        let t3 = t2 + secs(480);
        assert_eq!(backoff.record(rate_limited(), t3), secs(960));
        let t4 = t3 + secs(960);
        assert_eq!(backoff.record(rate_limited(), t4), secs(1800));
        let t5 = t4 + secs(1800);
        assert_eq!(backoff.record(rate_limited(), t5), secs(1800));
        assert_eq!(backoff.consecutive_rate_limits(), 6);
        assert_eq!(backoff.remaining(t5), secs(1800));
        assert_eq!(backoff.remaining(t5 + secs(1000)), secs(800));
    }

    #[test]
    fn success_after_backoff_returns_to_base_interval() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        let rate_limited = || ClaudeSyncOutcome::RateLimited { retry_after: None };
        backoff.record(rate_limited(), t0);
        backoff.record(rate_limited(), t0 + secs(120));
        backoff.record(rate_limited(), t0 + secs(360));
        assert_eq!(backoff.consecutive_rate_limits(), 3);

        let t_ok = t0 + secs(840);
        assert_eq!(backoff.record(ClaudeSyncOutcome::Success, t_ok), secs(120));
        assert!(backoff.should_attempt(t_ok));
        assert_eq!(backoff.consecutive_rate_limits(), 0);
        // The exponential ladder restarts from the base after a success.
        assert_eq!(backoff.record(rate_limited(), t_ok), secs(120));
        assert_eq!(backoff.consecutive_rate_limits(), 1);
    }

    #[test]
    fn retry_after_header_is_honoured_within_bounds() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        // Longer than the base: honoured as-is.
        assert_eq!(
            backoff.record(
                ClaudeSyncOutcome::RateLimited {
                    retry_after: Some(secs(300))
                },
                t0
            ),
            secs(300)
        );
        assert!(!backoff.should_attempt(t0 + secs(299)));
        assert!(backoff.should_attempt(t0 + secs(300)));
        // Shorter than the base tick: rounded up to the base.
        assert_eq!(
            backoff.record(
                ClaudeSyncOutcome::RateLimited {
                    retry_after: Some(secs(5))
                },
                t0
            ),
            secs(120)
        );
        // Absurdly long: clamped to the 30-minute ceiling.
        assert_eq!(
            backoff.record(
                ClaudeSyncOutcome::RateLimited {
                    retry_after: Some(secs(86_400))
                },
                t0
            ),
            secs(1800)
        );
        assert_eq!(backoff.consecutive_rate_limits(), 3);
    }

    #[test]
    fn retry_after_does_not_advance_the_exponential_ladder() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        backoff.record(
            ClaudeSyncOutcome::RateLimited {
                retry_after: Some(secs(600)),
            },
            t0,
        );
        // Next header-less 429 starts the ladder at the base, not 240 s.
        assert_eq!(
            backoff.record(
                ClaudeSyncOutcome::RateLimited { retry_after: None },
                t0 + secs(600)
            ),
            secs(120)
        );
    }

    #[test]
    fn other_errors_keep_base_cadence_without_resetting_streak() {
        let mut backoff = backoff();
        let t0 = Instant::now();
        backoff.record(ClaudeSyncOutcome::RateLimited { retry_after: None }, t0);
        assert_eq!(
            backoff.record(ClaudeSyncOutcome::OtherError, t0 + secs(120)),
            secs(120)
        );
        assert!(backoff.should_attempt(t0 + secs(120)));
        assert_eq!(backoff.consecutive_rate_limits(), 1);
        // The ladder position survives the unrelated error.
        assert_eq!(
            backoff.record(
                ClaudeSyncOutcome::RateLimited { retry_after: None },
                t0 + secs(240)
            ),
            secs(240)
        );
    }

    #[test]
    fn parses_retry_after_seconds_and_http_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-05T06:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(parse_retry_after("120", now), Some(secs(120)));
        assert_eq!(parse_retry_after(" 7 ", now), Some(secs(7)));
        assert_eq!(
            parse_retry_after("Sat, 05 Sep 2026 06:05:00 GMT", now),
            Some(secs(300))
        );
        // Past date → no usable delay.
        assert_eq!(
            parse_retry_after("Sat, 05 Sep 2026 05:59:00 GMT", now),
            None
        );
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }

    #[test]
    fn rate_limited_error_round_trips_through_anyhow() {
        let error = anyhow::Error::new(ClaudeUsageRateLimited {
            retry_after: Some(secs(42)),
        });
        let typed = error
            .downcast_ref::<ClaudeUsageRateLimited>()
            .expect("typed 429 error survives anyhow");
        assert_eq!(typed.retry_after, Some(secs(42)));
        assert!(error.to_string().contains("429"));
        assert!(error.to_string().contains("42s"));
    }
}
