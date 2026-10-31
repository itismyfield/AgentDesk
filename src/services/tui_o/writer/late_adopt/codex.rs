//! Codex late takeover of a fresh rollout before Legacy posted any of its output.
//! Legacy's sends are counted before the next bind, so a body sent from any rollout skips it.

use sha2::{Digest, Sha256};

use super::Wake;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{InitSource, OStore};
use crate::services::tui_o::writer::activation;
use crate::services::tui_o::writer::adoption;
use crate::services::tui_o::writer::binding::{
    BindingEvent, BindingEvents, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::host::{FencedFacts, HostIo, release, until_owned};
use crate::services::tui_o::writer::renumbered;

mod prefix;
use prefix::{Prefix, Verdict};

/// Legacy's sends, then the last binding seq: only a rollout bound after it, with no Legacy send
/// started since, may be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Baseline {
    sends: (u64, u64),
    seq: u64,
    order: u64,
    turn_mark: bool,
}

impl Baseline {
    fn detail(self) -> String {
        format!(
            "baseline={} order={} sends={:?} seq={}",
            if self.turn_mark { "turn" } else { "host" },
            self.order,
            self.sends,
            self.seq
        )
    }
}

/// Why a takeover attempt did not commit.
#[derive(Debug)]
enum Taken {
    /// The same rollout is judged again on the next wake.
    Retry(String),
    /// An eligible marked prefix hit a transient commit blocker.
    Soon(String),
    /// The rollout grew while still eligible; it is judged again at once.
    Again(String),
    /// Legacy may hold this rollout's output; only a later bind is taken.
    Skip(String),
    /// The channel stays Legacy's for this process.
    Final(String),
}

impl Taken {
    fn detail(&self) -> &str {
        match self {
            Self::Retry(detail)
            | Self::Soon(detail)
            | Self::Again(detail)
            | Self::Skip(detail)
            | Self::Final(detail) => detail,
        }
    }
}

struct Takeover<'a, I: HostIo> {
    io: &'a I,
    channel: u64,
    store: &'a OStore,
    log: &'a I::Bindings,
    candidate: &'a Candidate,
    gate: &'a OwnershipGate,
}

/// Waits for a rollout bound after the baseline that holds no output yet and commits it; false
/// once the channel left Deferred or stays Legacy's.
pub(super) async fn activate<I: HostIo>(
    io: &I,
    channel: u64,
    store: &OStore,
    log: &I::Bindings,
    candidate: &Candidate,
    gate: &OwnershipGate,
) -> bool {
    let alarms = io.alarms();
    let mut wake = Wake::new(log, channel);
    let takeover = Takeover {
        io,
        channel,
        store,
        log,
        candidate,
        gate,
    };
    let (mut baseline, mut logged) = (None, None);
    let (mut retry_order, mut short_retries) = (0, 0);
    loop {
        if candidate.peek() != Adoption::Deferred {
            return false;
        }
        if io.late_adopt_enabled() {
            until_owned(gate).await;
            let sampled = baseline.map_or_else(|| takeover.baseline(), Ok);
            let mut used = None;
            let outcome = match sampled {
                Ok(at) => {
                    baseline = Some(at);
                    let at = takeover.newest(at);
                    used = Some(at);
                    if retry_order != at.order {
                        (retry_order, short_retries) = (at.order, 0);
                    }
                    takeover.attempt(at).await
                }
                Err(taken) => Err(taken),
            };
            match outcome {
                Ok(()) => {
                    let detail = used.expect("successful attempt has a baseline").detail();
                    tracing::info!(channel, detail, "[tui_o] Codex takeover committed");
                    return true;
                }
                Err(Taken::Final(detail)) => {
                    release(candidate, &alarms, channel, &detail);
                    return false;
                }
                Err(Taken::Again(_)) => continue,
                Err(Taken::Skip(detail)) => {
                    let detail = format!("{detail}; {}", used.expect("attempt baseline").detail());
                    tracing::info!(channel, detail, "[tui_o] Codex takeover skips this rollout");
                    baseline = None;
                    #[cfg(test)]
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(Taken::Soon(detail)) => {
                    if logged.as_ref() != Some(&detail) {
                        tracing::info!(channel, detail, "[tui_o] Codex takeover waits");
                        logged = Some(detail);
                    }
                    if short_retries < 8 {
                        short_retries += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                }
                Err(Taken::Retry(detail)) if logged.as_ref() != Some(&detail) => {
                    tracing::info!(channel, detail, "[tui_o] Codex takeover waits");
                    logged = Some(detail);
                }
                Err(Taken::Retry(_)) => {}
            }
        }
        wake.wait().await;
    }
}

impl<I: HostIo> Takeover<'_, I> {
    fn events(&self) -> Result<Vec<BindingEvent>, Taken> {
        let events = self.log.binding_events_since(self.channel, 0);
        events.map(renumbered::first_named).map_err(Taken::Final)
    }

    /// Sends are read before the log, so a rollout bound in between counts as already there.
    fn baseline(&self) -> Result<Baseline, Taken> {
        let (sends, order) = {
            let _adoption = self.candidate.lock();
            self.candidate.baseline_sends()
        };
        let seq = self.events()?.last().map_or(0, |event| event.seq);
        Ok(Baseline {
            sends,
            seq,
            order,
            turn_mark: false,
        })
    }

    fn newest(&self, at: Baseline) -> Baseline {
        match self.candidate.turn_mark() {
            Some(mark) if mark.order > at.order => Baseline {
                sends: mark.sends,
                seq: mark.seq,
                order: mark.order,
                turn_mark: true,
            },
            _ => at,
        }
    }

    async fn attempt(&self, at: Baseline) -> Result<(), Taken> {
        let events = self.events()?;
        let Some(bound) = bound_after(&events, at.seq) else {
            return Err(Taken::Retry("no rollout bound since the baseline".into()));
        };
        match prefix::judge(&bound.0) {
            // Turn mode is confirmed after the taken turn, so a rollout with no prompt is not taken yet.
            Verdict::Eligible(prefix) if prefix.prompt_end.is_none() => Err(Taken::Retry(
                "the takeover rollout has no first user prompt yet".into(),
            )),
            Verdict::Eligible(prefix) => {
                self.commit(at, &bound, &prefix)
                    .await
                    .map_err(|taken| match taken {
                        Taken::Retry(detail)
                            if at.turn_mark
                                && self
                                    .candidate
                                    .turn_mark()
                                    .is_some_and(|mark| mark.order == at.order) =>
                        {
                            Taken::Soon(detail)
                        }
                        other => other,
                    })
            }
            Verdict::Wait(detail) => Err(Taken::Retry(detail)),
            Verdict::Refuse(detail) => Err(Taken::Skip(detail)),
        }
    }

    /// The intake fence is held from before the final checks until the init is published.
    async fn commit(
        &self,
        at: Baseline,
        bound: &(SourceId, u64),
        prefix: &Prefix,
    ) -> Result<(), Taken> {
        let (io, channel) = (self.io, self.channel);
        let Some(expected) = self.gate.current().epoch() else {
            return Err(Taken::Retry(
                "gateway ownership changed before O activation".into(),
            ));
        };
        #[cfg(test)]
        activation::test_hook::run(channel, activation::test_hook::Step::BeforeFence)
            .map_err(Taken::Retry)?;
        let fence_sends = {
            let _adoption = self.candidate.lock();
            self.candidate.sends()
        };
        if fence_sends.0 != at.sends.0 {
            return Err(Taken::Skip(
                "a Legacy body send started since the baseline".into(),
            ));
        }
        let FencedFacts {
            hold,
            facts,
            queued_bodies,
        } = (io.intake_fence(channel, ShadowProvider::Codex).await).map_err(Taken::Retry)?;
        let taken = (|| {
            #[cfg(test)]
            activation::test_hook::run(channel, activation::test_hook::Step::AfterFence)
                .map_err(Taken::Retry)?;
            if let Some(detail) = facts.final_blocker() {
                return Err(Taken::Final(detail));
            }
            let mut why = None;
            let sources = || {
                self.recheck(at, fence_sends, bound, prefix)
                    .map_err(|taken| {
                        let detail = taken.detail().to_owned();
                        why = Some(taken);
                        detail
                    })
            };
            // Custody reads are not atomic; the fence, the sends and the relay are judged too.
            let local = || {
                let custody = io.local_custody(channel, ShadowProvider::Codex)?;
                Ok(custody.blocks_takeover() || io.relaying(channel))
            };
            let result = self.gate.admit(|epoch| {
                (epoch == expected).then(|| {
                    activation::activate_codex_takeover(
                        self.store,
                        channel,
                        facts,
                        queued_bodies,
                        local,
                        self.candidate,
                        sources,
                    )
                })
            });
            match result.flatten() {
                Some(Ok(())) => Ok(()),
                Some(Err(detail)) => Err(why.unwrap_or(Taken::Retry(detail))),
                None => Err(Taken::Retry(
                    "gateway ownership changed before O activation".into(),
                )),
            }
        })();
        if let Err(error) = hold.release().await {
            tracing::warn!(channel, %error, "[tui_o] intake fence rollback failed");
        }
        taken
    }

    /// Under the adoption lock: no Legacy send since the baseline or the fence, the same bind,
    /// and the same eligible bytes. The init reads the rollout from its first byte.
    fn recheck(
        &self,
        at: Baseline,
        fence_sends: (u64, u64),
        (source, seq): &(SourceId, u64),
        prefix: &Prefix,
    ) -> Result<Vec<InitSource>, Taken> {
        // A send started since the baseline may be this rollout's; one only finished is older.
        let sends = self.candidate.sends();
        if sends.0 != at.sends.0 || sends.0 != fence_sends.0 {
            return Err(Taken::Skip(
                "a Legacy body send started while O fenced the takeover".into(),
            ));
        }
        if sends != fence_sends {
            return Err(Taken::Retry(
                "a Legacy body send finished while O fenced the takeover".into(),
            ));
        }
        let now = bound_after(&self.events()?, at.seq);
        if now.as_ref().is_none_or(|(s, q)| s != source || q != seq) {
            return Err(Taken::Retry(
                "the binding moved while O fenced the takeover".into(),
            ));
        }
        match prefix::judge(source) {
            Verdict::Eligible(now) if now == *prefix => {}
            Verdict::Eligible(_) => {
                return Err(Taken::Again(
                    "the rollout grew while O fenced the takeover".into(),
                ));
            }
            Verdict::Wait(detail) => return Err(Taken::Retry(detail)),
            Verdict::Refuse(detail) => return Err(Taken::Skip(detail)),
        }
        Ok(vec![InitSource {
            source_id: source.clone(),
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
        }])
    }
}

/// The source the log binds now with the seq that bound it, when that seq is above `after`.
fn bound_after(events: &[BindingEvent], after: u64) -> Option<(SourceId, u64)> {
    let (source, _) = adoption::current(events)?;
    let seq = events.iter().rev().find_map(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(bound),
            ..
        }
        | BindingRecord::Resolved { source: bound, .. }
            if *bound == source =>
        {
            Some(event.seq)
        }
        _ => None,
    })?;
    (seq > after).then_some((source, seq))
}
