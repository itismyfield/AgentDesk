use crate::config::runtime_profile::GatewayHandbackBreakerConfig;
use crate::services::discord::runtime_store::atomic_write;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    pending: Option<u64>,
    empty: Vec<u64>,
    activations: Vec<u64>,
    suppress_until: u64,
    manual: bool,
}

#[cfg(test)]
#[path = "gateway_handback_breaker_tests.rs"]
mod tests;

impl State {
    fn suppressed(&self, now: u64) -> bool {
        self.manual || now < self.suppress_until
    }
}

pub(super) struct GatewayHandbackBreaker {
    provider: String,
    owner: String,
    path: Option<PathBuf>,
    retry: Option<State>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    config: Box<dyn Fn() -> GatewayHandbackBreakerConfig + Send + Sync>,
}

impl GatewayHandbackBreaker {
    pub(super) fn for_owner(provider: &str, token_hash: &str) -> Self {
        Self::with_sources(
            provider,
            token_hash,
            crate::config::runtime_root(),
            || {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            },
            || {
                crate::config::load_graceful()
                    .cluster
                    .gateway_handback_breaker
            },
        )
    }

    pub(super) fn with_sources(
        provider: &str,
        token_hash: &str,
        root: Option<PathBuf>,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
        config: impl Fn() -> GatewayHandbackBreakerConfig + Send + Sync + 'static,
    ) -> Self {
        let token = token_hash.strip_prefix("discord_").unwrap_or(token_hash);
        let owner = format!("{provider}-{token}");
        let path = root.map(|root| {
            root.join("gateway_handback_breaker")
                .join(format!("{owner}.json"))
        });
        Self {
            provider: provider.to_owned(),
            owner,
            path,
            retry: None,
            clock: Box::new(clock),
            config: Box::new(config),
        }
    }

    fn state_error(&self, error: impl std::fmt::Display) {
        tracing::error!(owner = %self.owner, %error, "gateway_handback_breaker_state_error");
    }

    fn read(&mut self) -> Option<State> {
        let Some(path) = &self.path else {
            self.state_error("runtime root unavailable");
            return None;
        };
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(state) => Some(state),
                Err(error) => {
                    self.state_error(error);
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.retry = None;
                Some(State::default())
            }
            Err(error) => {
                self.state_error(error);
                None
            }
        }
    }

    fn write(&self, state: &State) -> bool {
        let result = (|| {
            let path = self.path.as_ref().ok_or("runtime root unavailable")?;
            let json = serde_json::to_string(state).map_err(|error| error.to_string())?;
            atomic_write(path, &json)
        })();
        if let Err(error) = result {
            self.state_error(error);
            return false;
        }
        true
    }

    pub(super) fn suppressed(&mut self) -> bool {
        if !(self.config)().enabled {
            return false;
        }
        let Some(mut state) = self.read() else {
            return true;
        };
        if let Some(retry) = self.retry.take() {
            if !self.write(&retry) {
                self.retry = Some(retry);
                return true;
            }
            state = retry;
        }
        state.suppressed((self.clock)())
    }

    pub(super) fn record_yield(&mut self) -> bool {
        if !(self.config)().enabled {
            return true;
        }
        let Some(mut state) = self.read() else {
            return false;
        };
        let now = (self.clock)();
        if self.retry.is_some() || state.pending.is_some() || state.suppressed(now) {
            return false;
        }
        state.pending = Some(now);
        self.write(&state)
    }

    pub(super) fn observe(&mut self, acquired: Result<bool, ()>) {
        let config = (self.config)();
        if !config.enabled {
            return;
        }
        let Ok(acquired) = acquired else {
            return;
        };
        let Some(mut state) = self.read() else {
            return;
        };
        if let Some(retry) = self.retry.take() {
            state = retry;
        } else {
            if state.pending.take().is_none() {
                return;
            }
            let now = (self.clock)();
            state.empty.retain(|at| now.saturating_sub(*at) < DAY_SECS);
            state
                .activations
                .retain(|at| now.saturating_sub(*at) < DAY_SECS);
            if acquired {
                state.empty.push(now);
                let dense = state
                    .empty
                    .iter()
                    .filter(|at| now.saturating_sub(**at) <= config.window_secs)
                    .count()
                    >= config.max_empty;
                if dense {
                    state.activations.push(now);
                    state.suppress_until = now.saturating_add(config.suppress_secs);
                }
                state.manual |= state.empty.len() >= 4 || state.activations.len() >= 2;
                if dense || state.manual {
                    tracing::error!(
                        provider = %self.provider, owner = %self.owner,
                        manual = state.manual, suppress_until = state.suppress_until,
                        "gateway_handback_suppressed"
                    );
                }
            }
        }
        if !self.write(&state) {
            self.retry = Some(state);
        }
    }
}
