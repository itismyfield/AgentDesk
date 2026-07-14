use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Preserve the first warning for a new slow hook, then keep only every Nth
/// occurrence so a persistent regression remains visible without log spam.
pub(super) const SLOW_HOOK_WARN_EVERY_N: u64 = 100;

type HookKey = (String, String);

fn occurrence_counts() -> &'static Mutex<HashMap<HookKey, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<HookKey, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn should_emit(policy_name: &str, hook_name: &str, is_slow: bool) -> bool {
    let Ok(mut counts) = occurrence_counts().lock() else {
        // A poisoned limiter must not hide a new performance regression.
        return is_slow;
    };
    let key = (policy_name.to_owned(), hook_name.to_owned());
    if !is_slow {
        counts.remove(&key);
        return false;
    }
    let count = counts.entry(key).or_insert(0);
    *count = count.saturating_add(1);
    *count == 1 || *count % SLOW_HOOK_WARN_EVERY_N == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_first_slow_hook_warning_then_only_every_nth_occurrence() {
        let calls = SLOW_HOOK_WARN_EVERY_N * 2;
        let emitted = (1..=calls)
            .filter(|_| should_emit("rate-limit-proof-policy-4250", "onTick5min", true))
            .count();

        assert_eq!(calls, 200);
        assert_eq!(
            emitted, 3,
            "expected WARNs only at occurrences 1, 100, and 200"
        );
    }

    #[test]
    fn healthy_hook_resets_the_slow_occurrence_series() {
        let policy = "recovery-reset-policy-4250";
        let hook = "onTick5min";

        assert!(should_emit(policy, hook, true));
        assert!(!should_emit(policy, hook, true));
        assert!(!should_emit(policy, hook, false));
        assert!(
            should_emit(policy, hook, true),
            "the first regression after recovery must emit immediately"
        );
    }
}
