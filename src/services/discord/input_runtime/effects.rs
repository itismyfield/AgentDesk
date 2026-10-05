//! Production effects for moving Legacy inputs into the ledger and back, run on a blocking worker.
//! Notices and actor start are only recorded; the supervisor sends and attaches them afterwards.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::fence::{self, Closing};
use crate::services::discord::input_transition::Effects;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::handover::{Composer, EnqueueOutcome, MoveEvidence};
use crate::services::tui_input::rows::Row;
use crate::services::turn_orchestrator::input_handback::{self, Destination};

pub(crate) struct Deps {
    pub provider: ProviderKind,
    pub channel: u64,
    pub tmux_session: String,
    pub token_hash: String,
    pub authorized: bool,
    pub active_sources: Vec<u64>,
    pub pool: Option<sqlx::PgPool>,
    pub closing: Arc<Closing>,
}

pub(crate) struct ProdEffects {
    root: PathBuf,
    deps: Deps,
    runtime: tokio::runtime::Handle,
    notices: Vec<(Option<u64>, &'static str)>,
    actor_requests: usize,
    #[cfg(test)]
    outbox_for_test: Option<bool>,
}

fn worker() -> io::Result<()> {
    fence::require_worker().map_err(|failure| io::Error::other(format!("input fence: {failure:?}")))
}

impl ProdEffects {
    /// Built inside the runtime that later blocks on Postgres from the blocking worker.
    pub(crate) fn new(deps: Deps) -> io::Result<Self> {
        let root =
            fence::population_root().ok_or_else(|| io::Error::other("runtime root unresolved"))?;
        if deps.closing.channel() != deps.channel || deps.closing.provider() != &deps.provider {
            return Err(io::Error::other("closing capability names another channel"));
        }
        Ok(Self {
            root,
            deps,
            runtime: tokio::runtime::Handle::try_current().map_err(io::Error::other)?,
            notices: Vec::new(),
            actor_requests: 0,
            #[cfg(test)]
            outbox_for_test: None,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn take_notices(&mut self) -> Vec<(Option<u64>, &'static str)> {
        std::mem::take(&mut self.notices)
    }

    pub(crate) fn actor_requests(&self) -> usize {
        self.actor_requests
    }
}

impl Effects for ProdEffects {
    // An unreadable outbox is not an empty one; the move then holds instead of guessing.
    fn intake_outbox_open(&mut self) -> io::Result<bool> {
        worker()?;
        #[cfg(test)]
        if let Some(open) = self.outbox_for_test {
            return Ok(open);
        }
        let pool = self
            .deps
            .pool
            .as_ref()
            .ok_or_else(|| io::Error::other("intake outbox unreadable without Postgres"))?;
        let channel = self.deps.channel.to_string();
        self.runtime
            .block_on(crate::db::intake_outbox::channel_has_open_row(
                pool, &channel,
            ))
            .map_err(io::Error::other)
    }

    // Legacy keeps no rendered prompt, so a moved row has neither an acceptance witness nor a
    // proven-empty composer: a turn row is held, a queued one stays never-pasted.
    fn evidence(&mut self, _key: u64, _payload: &Value) -> io::Result<MoveEvidence> {
        worker()?;
        Ok(MoveEvidence {
            user_record: false,
            turn_open: false,
            never_started: false,
            composer: Composer::Draft,
        })
    }

    // Only the row's own exact attempt witness proves acceptance; nothing proves the composer empty.
    fn reconcile(&mut self, _key: u64, row: &Row) -> io::Result<MoveEvidence> {
        worker()?;
        let accepted = row.attempt.as_ref().is_some_and(|attempt| {
            matches!(
                crate::services::tui_input::actor::witness::scan(attempt, false),
                Ok(Some(_))
            )
        });
        Ok(MoveEvidence {
            user_record: accepted,
            turn_open: false,
            never_started: false,
            composer: Composer::Draft,
        })
    }

    fn provider_alive(&mut self) -> io::Result<bool> {
        use crate::services::platform::tmux::{SessionPresence, session_presence};
        worker()?;
        let tmux = &self.deps.tmux_session;
        if crate::services::session_backend::process_session_pid(tmux).is_some() {
            return Ok(true);
        }
        match session_presence(tmux) {
            SessionPresence::Present => Ok(true),
            SessionPresence::Missing => Ok(false),
            SessionPresence::ProbeFailed => Err(io::Error::other("tmux presence probe failed")),
        }
    }

    fn materialize_bundle(&mut self, upload: &Value) -> io::Result<Vec<(String, Vec<u8>)>> {
        use crate::services::cluster::attachment_transfer::{store, uploads::BundleRef};
        worker()?;
        let reference: BundleRef = serde_json::from_value(upload.clone())?;
        let pool = self
            .deps
            .pool
            .as_ref()
            .ok_or_else(|| io::Error::other("attachment storage unavailable"))?;
        let bundle = self
            .runtime
            .block_on(store::load(pool, &reference))
            .map_err(io::Error::other)?;
        Ok(bundle
            .as_bundle()
            .entries
            .iter()
            .map(|entry| (entry.filename.clone(), entry.bytes.clone()))
            .collect())
    }

    // Uploads are copied before the population guard is taken, so no file IO of the copy runs
    // under it; the guard exists only while the channel is in handback.
    fn enqueue(&mut self, _key: u64, payload: &Value) -> io::Result<EnqueueOutcome> {
        worker()?;
        let clean = input_handback::without_pins(&self.root, self.deps.channel, payload)?;
        let guard = self
            .deps
            .closing
            .population(&self.root)
            .map_err(|failure| io::Error::other(format!("input fence: {failure:?}")))?;
        let destination = Destination {
            root: &self.root,
            provider: &self.deps.provider,
            token_hash: &self.deps.token_hash,
            channel: self.deps.channel,
            authorized: self.deps.authorized,
            active_sources: &self.deps.active_sources,
        };
        input_handback::enqueue_borrowed(&destination, &clean, &guard)
    }

    fn start_actor(&mut self) -> io::Result<()> {
        self.actor_requests += 1;
        Ok(())
    }

    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.notices.push((key, reason));
        Ok(())
    }
}

#[cfg(test)]
#[path = "effects_tests.rs"]
mod tests;
