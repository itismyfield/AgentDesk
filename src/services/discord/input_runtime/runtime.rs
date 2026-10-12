//! Explicit runtime adapters; boot wiring owns the later call that drives them.

use poise::serenity_prelude::Http;
use std::io;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use super::command::SupervisorCmd;
use super::mapping::Probe;
use super::reconcile::HoldCause;
use super::reserve::Reserved;
use super::{Config, Ports, REGISTRY, Refused, Registry, Supervisor, reconcile};
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

#[path = "runtime/drive.rs"]
pub(crate) mod drive;
#[path = "runtime/notice.rs"]
pub(crate) mod notice;
#[path = "runtime/ports.rs"]
pub(crate) mod ports;
pub(in crate::services::discord) use ports::source_policy_facts;

pub(crate) fn registry() -> &'static Registry {
    &REGISTRY
}

pub(crate) struct Runtime {
    supervisor: Supervisor<ports::RuntimePorts>,
    drive: drive::RuntimeDrive,
    expected: Vec<ProviderKind>,
}

impl Runtime {
    /// Binds the sender whose receiver is passed to run; this starts no supervisor.
    pub(in crate::services::discord) fn bind_intake(
        &mut self,
        sender: mpsc::Sender<SupervisorCmd>,
    ) -> Result<Arc<super::intake::Client>, Refused> {
        self.supervisor.bind_intake(sender)
    }

    pub(crate) async fn new(
        reserved: Reserved,
        config: Config,
        shared: Arc<SharedData>,
        http: Arc<Http>,
        expected: Vec<ProviderKind>,
    ) -> io::Result<Self> {
        if config.provider != shared.provider {
            return Err(io::Error::other(
                "runtime provider does not match reservation",
            ));
        }
        let ports = ports::RuntimePorts::new(
            shared,
            http.clone(),
            config.root.clone(),
            config.channel,
            Probe::default(),
        )
        .await?;
        let drive = drive::RuntimeDrive::new(
            config.provider.clone(),
            ports.session().to_owned(),
            http,
            config.channel,
            crate::services::tui_o::ownership::gate(config.provider.as_str()),
        );
        let mut supervisor =
            Supervisor::start(reserved, config, ports).map_err(|refused: Refused| {
                io::Error::other(format!("runtime reservation: {refused:?}"))
            })?;
        supervisor.prepare_handoff();
        Ok(Self {
            supervisor,
            drive,
            expected,
        })
    }

    /// The final view handoff is distinct from the command's task-ownership acknowledgement.
    pub(crate) async fn run(
        mut self,
        views: oneshot::Receiver<Vec<Arc<SharedData>>>,
        commands: mpsc::Receiver<SupervisorCmd>,
    ) {
        if self
            .supervisor
            .serve(self.drive, &self.expected, views, commands)
            .await
        {
            self.supervisor.release();
        }
    }
}
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "runtime/dormant_tests.rs"]
mod dormant_tests;

impl<P: Ports> Supervisor<P> {
    pub(crate) fn prepare_handoff(&mut self) {
        self.registration
            .report(&HoldCause::RuntimeViewPending, true);
    }

    pub(super) fn adopt_task(&mut self, channel: u64) -> Result<(), Refused> {
        if channel != self.config.channel
            || self.registration.key != (self.config.provider.as_str().to_owned(), channel)
        {
            return Err(Refused::Mismatch);
        }
        if self.adopted {
            return Err(Refused::Duplicate);
        }
        let entries = self
            .registration
            .registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if entries
            .get(&self.registration.key)
            .is_none_or(|entry| entry.poisoned)
        {
            return Err(Refused::Poisoned);
        }
        self.adopted = true;
        Ok(())
    }

    /// One Notice per cause and key in a supervisor's life; a failed send stays unsent.
    pub(super) async fn notify(&mut self, key: Option<u64>, reason: &'static str) {
        let episode = (key, reconcile::topic(reason));
        if self.sent.contains(&episode) {
            return;
        }
        let reason = *self.pending_notices.entry(episode).or_insert(reason);
        if self.ports.notice(reconcile::notice(key, reason)).await {
            self.sent.insert(episode);
            self.pending_notices.remove(&episode);
        }
    }

    pub(super) async fn retry_notices(&mut self) {
        let pending: Vec<_> = self
            .pending_notices
            .iter()
            .map(|(&(key, _), &reason)| (key, reason))
            .collect();
        for (key, reason) in pending {
            self.notify(key, reason).await;
        }
    }
}
