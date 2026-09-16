//! Same-provider account routing. Unknown usage is eligible; only known exhaustion
//! or a classified execution failure temporarily removes an account from selection.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_PROFILE_ID, ProviderAuthProfileDef, extra_account_login_supported, intern_provider,
};
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackPolicy {
    pub enabled: bool,
    pub fallback_profile: Option<String>,
    pub priority: Vec<String>,
    pub include_remaining: bool,
    pub cooldown_secs: u32,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_profile: None,
            priority: Vec::new(),
            include_remaining: true,
            cooldown_secs: 300,
        }
    }
}

impl FallbackPolicy {
    pub fn validate(
        &self,
        provider: &ProviderKind,
        catalog: &HashMap<String, ProviderAuthProfileDef>,
    ) -> Result<(), String> {
        if !(1..=86400).contains(&self.cooldown_secs) {
            return Err("profile fallback cooldown_secs must be between 1 and 86400".into());
        }
        if !extra_account_login_supported(provider) {
            return Err(format!(
                "{} does not support isolated account fallback",
                provider.as_str()
            ));
        }
        for id in self.fallback_profile.iter().chain(self.priority.iter()) {
            if id == DEFAULT_PROFILE_ID {
                continue;
            }
            let def = catalog
                .get(id)
                .ok_or_else(|| format!("unknown fallback auth_profile '{id}'"))?;
            if intern_provider(&def.provider).map_err(|e| e.to_string())? != *provider {
                return Err(format!(
                    "fallback auth_profile '{id}' belongs to a different provider"
                ));
            }
        }
        Ok(())
    }

    /// Current, explicit fallback, priority, sorted remainder; global home is opt-in.
    pub fn candidates(
        &self,
        provider: &ProviderKind,
        primary: &str,
        catalog: &HashMap<String, ProviderAuthProfileDef>,
    ) -> Vec<String> {
        let mut result = vec![primary.to_string()];
        if !self.enabled || !extra_account_login_supported(provider) {
            return result;
        }
        let mut append = |id: &str| {
            if !result.iter().any(|existing| existing == id) {
                result.push(id.to_string());
            }
        };
        for id in self.fallback_profile.iter().chain(self.priority.iter()) {
            if id == DEFAULT_PROFILE_ID
                || catalog.get(id).is_some_and(|def| {
                    intern_provider(&def.provider).ok().as_ref() == Some(provider)
                })
            {
                append(id);
            }
        }
        if self.include_remaining {
            let remaining: BTreeSet<_> = catalog
                .iter()
                .filter_map(|(id, def)| {
                    (intern_provider(&def.provider).ok().as_ref() == Some(provider)).then_some(id)
                })
                .collect();
            for id in remaining {
                append(id);
            }
        }
        result
    }
}

#[derive(Default)]
struct Route {
    candidates: Vec<String>,
    selected: String,
    request: u64,
    attempted: BTreeSet<String>,
    touched_at: i64,
}

#[derive(Default)]
struct Router {
    routes: HashMap<(String, u64), Route>,
    cooldowns: HashMap<(String, String), i64>,
}

impl Router {
    fn prune(&mut self, now: i64) {
        self.cooldowns.retain(|_, until| *until > now);
        self.routes
            .retain(|_, route| now.saturating_sub(route.touched_at) < 86400);
    }

    fn select(
        &mut self,
        provider: &str,
        channel: u64,
        candidates: &[String],
        now: i64,
        available: impl Fn(&str) -> bool,
    ) -> Option<String> {
        self.prune(now);
        let route = self
            .routes
            .entry((provider.to_string(), channel))
            .or_default();
        if route.candidates != candidates {
            *route = Route {
                candidates: candidates.to_vec(),
                ..Route::default()
            };
        }
        let eligible = |id: &str| {
            !self
                .cooldowns
                .contains_key(&(provider.to_string(), id.to_string()))
                && available(id)
        };
        // Keep a healthy selected account to preserve its warm session.
        let selected = if candidates.contains(&route.selected) && eligible(&route.selected) {
            route.selected.clone()
        } else {
            candidates.iter().find(|id| eligible(id)).cloned()?
        };
        route.selected = selected.clone();
        route.touched_at = now;
        Some(selected)
    }

    fn fail(
        &mut self,
        provider: &str,
        channel: u64,
        request: u64,
        cooldown_secs: u32,
        now: i64,
        available: impl Fn(&str) -> bool,
    ) -> Option<(String, String)> {
        self.prune(now);
        let route = self.routes.get_mut(&(provider.to_string(), channel))?;
        if request == 0 || route.candidates.len() < 2 {
            return None;
        }
        if route.request != request {
            route.request = request;
            route.attempted.clear();
        }
        let failed = route.selected.clone();
        route.attempted.insert(failed.clone());
        route.touched_at = now;
        self.cooldowns.insert(
            (provider.to_string(), failed.clone()),
            now + i64::from(cooldown_secs),
        );
        let next = route
            .candidates
            .iter()
            .find(|id| {
                !route.attempted.contains(*id)
                    && !self
                        .cooldowns
                        .contains_key(&(provider.to_string(), (*id).clone()))
                    && available(id)
            })?
            .clone();
        route.selected = next.clone();
        Some((failed, next))
    }
}

fn router() -> &'static Mutex<Router> {
    static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
    ROUTER.get_or_init(|| Mutex::new(Router::default()))
}

pub(crate) fn select(
    provider: &ProviderKind,
    channel: u64,
    candidates: &[String],
    available: impl Fn(&str) -> bool,
) -> Option<String> {
    let now = chrono::Utc::now().timestamp();
    let mut router = router().lock().unwrap_or_else(|p| p.into_inner());
    router.select(provider.as_str(), channel, candidates, now, available)
}

pub(crate) fn fail(
    provider: &ProviderKind,
    channel: u64,
    request: u64,
    policy: &FallbackPolicy,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
    available: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    if !policy.enabled {
        return None;
    }
    let mut router = router().lock().unwrap_or_else(|p| p.into_inner());
    let id = provider.as_str();
    let route = router.routes.get_mut(&(id.to_string(), channel))?;
    let primary = route.candidates.first()?.clone();
    route.candidates = policy.candidates(provider, &primary, catalog);
    let now = chrono::Utc::now().timestamp();
    let cooldown = policy.cooldown_secs;
    router.fail(id, channel, request, cooldown, now, available)
}

#[cfg(test)]
mod tests;
