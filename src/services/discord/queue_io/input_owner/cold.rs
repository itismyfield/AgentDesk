//! Runs a reserved cold attempt under the owner's writer and input scope.
use crate::services::tui_input::human_frame::history::v2 as history;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

pub(crate) struct ColdOutcome<O> {
    pub(crate) result: Result<O, String>,
    pub(crate) not_sent: Option<crate::services::claude_tui::owner_input::NotSentProof>,
}

pub(crate) async fn v2_cold_observed<O: Send + 'static>(
    mailbox: &ChannelMailboxHandle,
    now: u64,
    clock: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
    effect: impl FnOnce(String) -> Result<O, String> + Send + 'static,
) -> Result<(serde_json::Value, ColdOutcome<O>), String> {
    use crate::services::claude_tui::owner_input;
    use std::sync::{Arc, Mutex};
    let (key, permit, frame, registry, namespace, channel, retry_due) = mailbox
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
            let retry_due = owner
                .attempt_readiness_hold(&key)
                .map(|hold| (hold.started_at_ms, hold.due_at_ms));
            let permit = owner.arm_bootstrap(&key, now)?;
            Ok((key, permit, frame, registry, namespace, channel, retry_due))
        })
        .await?;
    let identity = serde_json::to_value(&key).map_err(|e| e.to_string())?;
    let fallback = identity.clone();
    let mailbox = mailbox.clone();
    let timing = crate::logging::intake_timing::carry();
    tokio::task::spawn_blocking(move || {
        let _timing = timing.entered();
        let outcome = (|| {
            let lease = Arc::new(Mutex::new((permit.consume(&key)?, None::<String>)));
            let (m, k, c) = (mailbox.clone(), key.clone(), clock.clone());
            let approve: owner_input::Approve = Arc::new(move || {
                let (key, clock) = (k.clone(), c.clone());
                tokio::runtime::Handle::current().block_on(m.v2_request(move |s| {
                    let owner = s.as_mut().ok_or("owner missing")?;
                    owner.approve_first_key(&key, &*clock)
                }))
            });
            let readiness_deadline = retry_due.map(|(started, due)| {
                let at = clock();
                let remaining = if at < started {
                    0
                } else {
                    due.saturating_sub(at)
                };
                std::time::Instant::now() + std::time::Duration::from_millis(remaining)
            });
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
                owner_input::capture_owner_scope(
                    key.attempt.provider.clone(),
                    target,
                    channel,
                    (commit, approve, readiness_deadline),
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
