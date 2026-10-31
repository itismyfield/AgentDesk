//! The owner's cold effect boundary; the owner retains settlement and observer custody.
use crate::services::tui_input::human_frame::history::v2 as history;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

#[cfg(test)]
pub(super) struct Completion<K, O> {
    pub attempt: K,
    pub outcome: Result<O, String>,
}

#[cfg(test)]
pub(super) async fn submit<O: Send + 'static>(
    mailbox: &ChannelMailboxHandle,
    now: u64,
    effect: impl FnOnce(String) -> Result<O, String> + Send + 'static,
) -> Result<Completion<impl serde::Serialize + Send, O>, String> {
    let (attempt, permit, frame, registry, namespace) = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let key = owner.active_attempt()?;
            if !matches!(key.attempt.provider.as_str(), "claude" | "codex")
                || !matches!(key.attempt.target.host.as_str(), "tmux" | "herdr")
            {
                return Err("provider_unsupported".into());
            }
            let frame = owner
                .snapshot
                .head_injection
                .as_ref()
                .and_then(|head| head.payload.as_ref())
                .ok_or("unprepared payload")?
                .frame
                .clone();
            let registry = history::Registry::runtime()?;
            owner.prepare_history(&registry)?;
            let namespace = owner
                .snapshot
                .namespace
                .clone()
                .ok_or("missing namespace")?;
            let permit = owner.arm(&key, now)?;
            Ok((key, permit, frame, registry, namespace))
        })
        .await?;
    let fallback = attempt.clone();
    Ok(tokio::task::spawn_blocking(move || {
        let outcome = permit.consume(&attempt).and_then(|_writer| {
            let identity = serde_json::to_value(&attempt).map_err(|error| error.to_string())?;
            history::with_attempt(&registry, &namespace, &identity, || effect(frame))?
        });
        Completion { attempt, outcome }
    })
    .await
    .unwrap_or_else(|error| Completion {
        attempt: fallback,
        outcome: Err(error.to_string()),
    }))
}

#[cfg(test)]
#[path = "first_input/pg_tests.rs"]
mod pg_tests;

pub(crate) struct ColdOutcome<O> {
    pub(crate) result: Result<O, String>,
    pub(crate) not_sent: Option<crate::services::claude_tui::owner_input::NotSentProof>,
}
impl ChannelMailboxHandle {
    #[cfg(test)]
    pub(crate) async fn v2_cold_bootstrap<O: Send + 'static>(
        &self,
        now: u64,
        clock: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
        effect: impl FnOnce(String) -> Result<O, String> + Send + 'static,
    ) -> Result<(serde_json::Value, Result<O, String>), String> {
        self.v2_cold_observed(now, clock, effect)
            .await
            .map(|(key, outcome)| (key, outcome.result))
    }
    pub(crate) async fn v2_cold_observed<O: Send + 'static>(
        &self,
        now: u64,
        clock: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
        effect: impl FnOnce(String) -> Result<O, String> + Send + 'static,
    ) -> Result<(serde_json::Value, ColdOutcome<O>), String> {
        use crate::services::claude_tui::owner_input;
        use std::sync::{Arc, Mutex};
        let (key, permit, frame, registry, namespace, channel) = self
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("V2 not adopted")?;
                let key = owner.active_attempt()?;
                if !matches!(key.attempt.provider.as_str(), "claude" | "codex")
                    || !matches!(key.attempt.target.host.as_str(), "tmux" | "herdr")
                {
                    return Err("provider_unsupported".into());
                }
                let head = owner.snapshot.head_injection.as_ref().ok_or("no head")?;
                let frame = head.payload.as_ref().ok_or("unprepared")?.frame.clone();
                let channel = head.original.channel_id.ok_or("missing channel")?;
                let registry = history::Registry::runtime()?;
                owner.prepare_history(&registry)?;
                let namespace = owner
                    .snapshot
                    .namespace
                    .clone()
                    .ok_or("missing namespace")?;
                let permit = owner.arm_bootstrap(&key, now)?;
                Ok((key, permit, frame, registry, namespace, channel))
            })
            .await?;
        let identity = serde_json::to_value(&key).map_err(|e| e.to_string())?;
        let fallback = identity.clone();
        let mailbox = self.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = (|| {
                let lease = Arc::new(Mutex::new((permit.consume(&key)?, None::<String>)));
                let m = mailbox.clone();
                let k = key.clone();
                let commit: owner_input::Commit = Arc::new(move |_| {
                    let key = k.clone();
                    let now = clock();
                    tokio::runtime::Handle::current().block_on(m.v2_request(move |s| {
                        s.as_mut()
                            .ok_or("owner missing")?
                            .payload_started(&key, now)
                    }))
                });
                let m = mailbox.clone();
                let k = key.clone();
                let writer = lease.clone();
                let pin: owner_input::Pin = Arc::new(move |epoch, physical| {
                    let mut writer = writer.lock().map_err(|_| "writer poisoned")?;
                    match &writer.1 {
                        Some(old) if *old != physical => {
                            return Err("physical writer cannot transfer".into());
                        }
                        Some(_) => {}
                        None => {
                            writer.0.add_physical(physical.clone())?;
                            writer.1 = Some(physical);
                        }
                    }
                    if let Some(epoch) = epoch {
                        let key = k.clone();
                        tokio::runtime::Handle::current().block_on(m.v2_request(move |s| {
                            s.as_mut().ok_or("owner missing")?.bind_target(&key, epoch)
                        }))?;
                    }
                    Ok(())
                });
                let m = mailbox;
                let k = key.clone();
                let hold: owner_input::Hold = Arc::new(move |value| {
                    let key = k.clone();
                    tokio::runtime::Handle::current().block_on(m.v2_request(move |s| {
                        s.as_mut()
                            .ok_or("owner missing")?
                            .record_preparation_hold(&key, value)
                    }))
                });
                let mut target =
                    serde_json::to_value(&key.attempt.target).map_err(|e| e.to_string())?;
                target["_attempt"] = identity.clone();
                history::with_attempt(&registry, &namespace, &identity, || {
                    owner_input::capture_scope(
                        key.attempt.provider.clone(),
                        target,
                        channel,
                        commit,
                        pin,
                        hold,
                        || effect(frame),
                    )
                })
            })();
            let (result, not_sent) = match outcome {
                Ok(pair) => pair,
                Err(error) => (Err(error), None),
            };
            (identity, ColdOutcome { result, not_sent })
        })
        .await
        .map_err(|e| format!("cold effect {fallback}: {e}"))
    }
}
