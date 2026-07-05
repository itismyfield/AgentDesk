//! tmux availability probe cache (#4113).
//!
//! `tmux -V` is probed at most once per TTL; spawn errors other than
//! NotFound keep the last known state and only demote availability after
//! consecutive failures, so a transient fork failure cannot demote a turn
//! to ProcessBackend and double-resume the session.

use super::tmux_command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const TMUX_AVAILABILITY_CACHE_TTL: Duration = Duration::from_secs(45);
const TMUX_AVAILABILITY_FAILURE_THRESHOLD: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxAvailabilityProbe {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Debug, Default)]
struct TmuxAvailabilityCache {
    cached: Option<bool>,
    checked_at: Option<Instant>,
    consecutive_failures: u8,
}

static TMUX_AVAILABILITY_CACHE: LazyLock<Mutex<TmuxAvailabilityCache>> =
    LazyLock::new(|| Mutex::new(TmuxAvailabilityCache::default()));

fn probe_tmux_availability() -> TmuxAvailabilityProbe {
    match tmux_command().arg("-V").output() {
        Ok(output) if output.status.success() => TmuxAvailabilityProbe::Available,
        Ok(_) => TmuxAvailabilityProbe::Unavailable,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            TmuxAvailabilityProbe::Unavailable
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "tmux availability probe failed to spawn; preserving cached availability"
            );
            TmuxAvailabilityProbe::Unknown
        }
    }
}

fn resolve_tmux_availability(
    cache: &mut TmuxAvailabilityCache,
    now: Instant,
    probe: impl FnOnce() -> TmuxAvailabilityProbe,
) -> bool {
    if let (Some(cached), Some(checked_at)) = (cache.cached, cache.checked_at)
        && now.duration_since(checked_at) < TMUX_AVAILABILITY_CACHE_TTL
    {
        return cached;
    }

    cache.checked_at = Some(now);
    match probe() {
        TmuxAvailabilityProbe::Available => {
            cache.cached = Some(true);
            cache.consecutive_failures = 0;
            true
        }
        TmuxAvailabilityProbe::Unavailable => {
            cache.consecutive_failures = cache.consecutive_failures.saturating_add(1);
            if cache.consecutive_failures >= TMUX_AVAILABILITY_FAILURE_THRESHOLD {
                cache.cached = Some(false);
            }
            cache.cached.unwrap_or(false)
        }
        TmuxAvailabilityProbe::Unknown => {
            cache.consecutive_failures = cache.consecutive_failures.saturating_add(1);
            if cache.consecutive_failures >= TMUX_AVAILABILITY_FAILURE_THRESHOLD {
                cache.cached = Some(false);
            }
            cache.cached.unwrap_or(true)
        }
    }
}

/// Check if tmux is available on the system.
pub fn is_available() -> bool {
    let mut cache = TMUX_AVAILABILITY_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    resolve_tmux_availability(&mut cache, Instant::now(), probe_tmux_availability)
}

#[cfg(test)]
mod availability_cache_tests {
    use super::*;

    #[test]
    fn probe_error_preserves_previous_available_state() {
        let mut cache = TmuxAvailabilityCache::default();
        let now = Instant::now();
        assert!(resolve_tmux_availability(&mut cache, now, || {
            TmuxAvailabilityProbe::Available
        }));

        let after_ttl = now + TMUX_AVAILABILITY_CACHE_TTL + Duration::from_millis(1);
        assert!(resolve_tmux_availability(&mut cache, after_ttl, || {
            TmuxAvailabilityProbe::Unknown
        }));

        assert_eq!(cache.cached, Some(true));
        assert_eq!(cache.consecutive_failures, 1);
    }

    #[test]
    fn consecutive_probe_failures_demote_after_threshold() {
        let mut cache = TmuxAvailabilityCache::default();
        let mut now = Instant::now();
        let step = TMUX_AVAILABILITY_CACHE_TTL + Duration::from_millis(1);
        assert!(resolve_tmux_availability(&mut cache, now, || {
            TmuxAvailabilityProbe::Available
        }));

        for _ in 1..TMUX_AVAILABILITY_FAILURE_THRESHOLD {
            now = now + step;
            assert!(resolve_tmux_availability(&mut cache, now, || {
                TmuxAvailabilityProbe::Unknown
            }));
        }

        now = now + step;
        assert!(!resolve_tmux_availability(&mut cache, now, || {
            TmuxAvailabilityProbe::Unknown
        }));
        assert_eq!(cache.cached, Some(false));
    }

    #[test]
    fn unknown_spawn_error_without_prior_state_stays_optimistic_until_threshold() {
        let mut cache = TmuxAvailabilityCache::default();
        let mut now = Instant::now();
        let step = TMUX_AVAILABILITY_CACHE_TTL + Duration::from_millis(1);

        for _ in 1..TMUX_AVAILABILITY_FAILURE_THRESHOLD {
            assert!(resolve_tmux_availability(&mut cache, now, || {
                TmuxAvailabilityProbe::Unknown
            }));
            now = now + step;
        }

        assert!(!resolve_tmux_availability(&mut cache, now, || {
            TmuxAvailabilityProbe::Unknown
        }));
    }

    #[test]
    fn ttl_hit_reuses_cached_state_without_probe() {
        let mut cache = TmuxAvailabilityCache::default();
        let now = Instant::now();
        assert!(resolve_tmux_availability(&mut cache, now, || {
            TmuxAvailabilityProbe::Available
        }));

        let cached = resolve_tmux_availability(&mut cache, now + Duration::from_secs(1), || {
            panic!("tmux availability probe should be cached")
        });

        assert!(cached);
        assert_eq!(cache.consecutive_failures, 0);
    }
}
