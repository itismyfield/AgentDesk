//! The existing supervisor mailbox and ordering overflow, without a second input authority.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{mpsc, oneshot};

use super::{Classification, PreparedSource};
use crate::services::discord::input_runtime::fence;
use crate::services::discord::input_runtime::supervisor::{
    Ports, Refused, Registry, Supervisor, command, ordering, receipt, source,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};

pub(in crate::services::discord) enum Access {
    Legacy,
    Ready(Arc<Client>),
    Deferred(receipt::Deferred),
}

pub(in crate::services::discord) fn lookup(
    registry: &'static Registry,
    provider: &ProviderKind,
    channel: u64,
) -> Access {
    let Some(gate) = fence::channel_gate(channel) else {
        #[cfg(test)]
        if super::super::mutant("c2_off_lookup_registers") {
            let _ = registry.register(provider, channel, std::path::Path::new("target/c2-off"));
        }
        return Access::Legacy;
    };
    if gate.provider() != provider {
        let _ = registry.intake_client(gate.provider(), channel);
        return Access::Deferred(receipt::Deferred::Conflict);
    }
    match registry.intake_client(provider, channel) {
        Ok(client) => Access::Ready(client),
        Err(reason) => Access::Deferred(reason),
    }
}

pub(in crate::services::discord) struct Client {
    sender: mpsc::Sender<command::SupervisorCmd>,
    overflow: ordering::PendingOverflow,
    ready: AtomicBool,
}

struct ReplyGuard<'a> {
    overflow: &'a ordering::PendingOverflow,
    armed: bool,
}

impl Drop for ReplyGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        if super::super::mutant("c2_drop_client_reply_obligation") {
            return;
        }
        if self.armed {
            self.overflow.mark_dirty();
        }
    }
}

impl Client {
    pub(in crate::services::discord::input_runtime::supervisor) fn new(
        sender: mpsc::Sender<command::SupervisorCmd>,
        overflow: ordering::PendingOverflow,
    ) -> Self {
        Self {
            sender,
            overflow,
            ready: AtomicBool::new(false),
        }
    }

    pub(in crate::services::discord::input_runtime::supervisor) fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub(in crate::services::discord::input_runtime::supervisor) fn publish(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub(in crate::services::discord) async fn pending(
        &self,
        sources: Vec<u64>,
    ) -> Result<ordering::PendingSource, receipt::Deferred> {
        if !self.ready() {
            self.overflow.mark_dirty();
            return Err(receipt::Deferred::Closed);
        }
        command::pending_source(&self.sender, &self.overflow, sources).await
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> command::SupervisorCmd,
    ) -> Result<T, receipt::Deferred> {
        let mut obligation = ReplyGuard {
            overflow: &self.overflow,
            armed: true,
        };
        if !self.ready() {
            return Err(receipt::Deferred::Closed);
        }
        let result = command::request(&self.sender, make).await;
        obligation.armed = result.is_err();
        result
    }

    pub(in crate::services::discord) async fn lookup_responsibility(
        &self,
        identity: ReceiptIdentity,
    ) -> Result<Responsibility, receipt::Deferred> {
        self.call(|reply| command::SupervisorCmd::LookupResponsibility { identity, reply })
            .await
    }

    pub(in crate::services::discord) async fn fetch_ticket(
        &self,
    ) -> Result<ordering::FetchTicket, receipt::Deferred> {
        self.call(|reply| command::SupervisorCmd::FetchTicket { reply })
            .await?
            .map_err(|_| receipt::Deferred::Order)
    }

    pub(in crate::services::discord) async fn begin_scan(
        &self,
        ticket: ordering::FetchTicket,
        sources: Vec<u64>,
        horizon: u64,
        complete_fetch: bool,
    ) -> Result<ordering::ScanCapability, receipt::Deferred> {
        self.call(|reply| command::SupervisorCmd::BeginScan {
            ticket,
            sources,
            horizon,
            complete_fetch,
            reply,
        })
        .await?
        .map_err(|_| receipt::Deferred::Order)
    }

    pub(in crate::services::discord) async fn complete_scan(
        &self,
        capability: ordering::ScanCapability,
    ) -> Result<ordering::ValidatedOrderCapability, receipt::Deferred> {
        self.call(|reply| command::SupervisorCmd::CompleteScan { capability, reply })
            .await?
            .map_err(|_| receipt::Deferred::Order)
    }

    /// This certifies admission closure and flush at the ACK; a later Clear can still write.
    pub(in crate::services::discord) async fn close(
        &self,
    ) -> Result<command::ClosedAndFlushed, receipt::Deferred> {
        self.call(|ack| command::SupervisorCmd::Close { ack })
            .await?
    }
}

pub(in crate::services::discord) enum Applied {
    Receipt(command::ScanCommit),
    Settled(ordering::ScanCapability),
    Unsettled {
        classification: Classification,
        capability: ordering::ScanCapability,
    },
}

pub(in crate::services::discord) async fn apply_from_scan(
    client: &Client,
    classification: Classification,
    prepared: PreparedSource,
    capability: ordering::ScanCapability,
) -> Result<Applied, receipt::Deferred> {
    match classification {
        Classification::Target => {
            let source =
                source::Source::from_prepared(prepared).map_err(|_| receipt::Deferred::Conflict)?;
            let committed = client
                .call(|reply| command::SupervisorCmd::CommitFromScan {
                    source: Box::new(source),
                    capability,
                    reply,
                })
                .await?;
            #[cfg(test)]
            let committed = if super::super::mutant("c2_receipt_means_settled") {
                command::ScanCommit {
                    settlement: Ok(()),
                    ..committed
                }
            } else {
                committed
            };
            Ok(Applied::Receipt(committed))
        }
        Classification::NonTarget => {
            let source = prepared.key();
            client
                .call(|reply| command::SupervisorCmd::SettleFromScan {
                    source,
                    disposition: command::FinalDisposition::NonTarget,
                    capability,
                    reply,
                })
                .await?
                .map(Applied::Settled)
                .map_err(|_| receipt::Deferred::Order)
        }
        classification => Ok(Applied::Unsettled {
            classification,
            capability,
        }),
    }
}

impl Registry {
    pub(super) fn intake_client(
        &self,
        provider: &ProviderKind,
        channel: u64,
    ) -> Result<Arc<Client>, receipt::Deferred> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries
            .get(&(provider.as_str().to_owned(), channel))
            .ok_or(receipt::Deferred::Unknown)?;
        let client = entry.client.as_ref().and_then(Weak::upgrade);
        if !entry.poisoned && client.as_ref().is_some_and(|client| client.ready()) {
            return Ok(client.unwrap());
        }
        #[cfg(test)]
        let retain = !super::super::mutant("c2_drop_prebind_dirty");
        #[cfg(not(test))]
        let retain = true;
        if retain && let Some(overflow) = &entry.overflow {
            overflow.mark_dirty();
        }
        Err(
            if entry.poisoned || (entry.client.is_some() && client.is_none()) {
                receipt::Deferred::SupervisorLost
            } else {
                receipt::Deferred::Closed
            },
        )
    }
}

impl<P: Ports> Supervisor<P> {
    /// Binds the existing mailbox without retaining its sender in the registry or this task.
    pub(crate) fn bind_intake(
        &mut self,
        sender: tokio::sync::mpsc::Sender<command::SupervisorCmd>,
    ) -> Result<Arc<Client>, Refused> {
        let mut entries = self
            .registration
            .registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = entries
            .get_mut(&self.registration.key)
            .ok_or(Refused::Poisoned)?;
        if entry.poisoned || entry.client.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(if entry.poisoned {
                Refused::Poisoned
            } else {
                Refused::Duplicate
            });
        }
        let overflow = self.order.pending_overflow();
        #[cfg(test)]
        let overflow = if super::super::mutant("c2_discard_dirty_on_bind") {
            ordering::AdmissionOrder::new(self.config.provider.clone(), self.config.channel, 1, 0)
                .unwrap()
                .pending_overflow()
        } else {
            overflow
        };
        let client = Arc::new(Client::new(sender, overflow));
        entry.client = Some(Arc::downgrade(&client));
        Ok(client)
    }

    pub(in crate::services::discord) fn publish_intake(&self) {
        if !self.adopted {
            return;
        }
        let entries = self
            .registration
            .registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(client) = entries
            .get(&self.registration.key)
            .filter(|entry| !entry.poisoned)
            .and_then(|entry| entry.client.as_ref())
            .and_then(Weak::upgrade)
        {
            client.publish();
        }
    }
}

impl super::super::Entry {
    pub(in crate::services::discord::input_runtime::supervisor) fn with_overflow(
        overflow: ordering::PendingOverflow,
    ) -> Self {
        Self {
            overflow: Some(overflow),
            ..Self::default()
        }
    }
}

impl<P: Ports> Supervisor<P> {
    pub(crate) fn pending_overflow(&self) -> ordering::PendingOverflow {
        self.order.pending_overflow()
    }

    pub(crate) fn pending_snapshot(&self) -> ordering::PendingSource {
        self.order.pending_snapshot()
    }

    pub(in crate::services::discord::input_runtime::supervisor) async fn permits_source(
        &mut self,
        source: &source::Source,
    ) -> bool {
        let policy = self.ports.source_policy(source).await;
        #[cfg(test)]
        if super::super::mutant("c2_skip_receipt_policy") {
            return true;
        }
        policy == Classification::Target
    }
}
