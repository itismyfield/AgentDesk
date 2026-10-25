//! Chooses registry state before installation; migration failures never become boot errors.

use std::collections::BTreeSet;

mod consent_holds;
pub(crate) use consent_holds::ConsentHolds;

use serde::Serialize;
use sqlx::{PgPool, Row};

use super::approval_store::{ApprovalStore, Loaded};
use super::projection::Projection;
use super::{BootSessionHosts, ChannelEndpoint, SessionHostsConfig};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Diagnostic {
    pub reason: &'static str,
    pub channels: Vec<u64>,
    pub blockers: Vec<String>,
    pub action: String,
}

pub(crate) struct HostsBootSelection {
    hosts: BootSessionHosts,
    candidate: SessionHostsConfig,
    diagnostic: Option<Diagnostic>,
    admission_stopped: bool,
}

impl HostsBootSelection {
    pub(crate) fn hosts(&self) -> &BootSessionHosts {
        &self.hosts
    }

    fn new(hosts: BootSessionHosts, candidate: &BootSessionHosts) -> Self {
        Self {
            hosts,
            candidate: candidate.config.clone(),
            diagnostic: None,
            admission_stopped: false,
        }
    }

    pub(crate) fn into_parts(self) -> (BootSessionHosts, super::BootMigration) {
        (
            self.hosts,
            super::BootMigration {
                candidate: self.candidate,
                diagnostic: self.diagnostic,
                admission_stopped: self.admission_stopped,
            },
        )
    }

    fn diagnose(
        mut self,
        reason: &'static str,
        channels: &BTreeSet<u64>,
        blockers: Vec<String>,
    ) -> Self {
        self.diagnostic = Some(Diagnostic {
            reason,
            channels: channels.iter().copied().collect(),
            blockers,
            action: String::new(),
        });
        self
    }
}

pub(crate) struct MigrationRow {
    pub id: i64,
    pub channel: u64,
    pub record: HostedRecord,
}

pub(crate) trait RowSource {
    fn available(&self) -> bool {
        true
    }
    async fn rows(&self, channels: &BTreeSet<u64>) -> Result<Vec<MigrationRow>, String>;
}

pub(crate) trait HoldSource {
    const INTRODUCED: bool;
    async fn unresolved(&self, channels: &BTreeSet<u64>) -> Result<Vec<u64>, String>;
    /// Held channels for this move; only a return reserved for `candidate` lets its input pass.
    async fn blocking(
        &self,
        channels: &BTreeSet<u64>,
        _candidate: &BootSessionHosts,
    ) -> Result<Vec<u64>, String> {
        self.unresolved(channels).await
    }
    /// Runs after `approved` is durable: the returns reserved for it are now adopted.
    async fn adopted(
        &self,
        _approved: &BootSessionHosts,
        _channels: &BTreeSet<u64>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// Names one channel's endpoint binding; a returned input is reserved for exactly this value.
#[cfg(unix)]
pub(crate) fn binding_digest(hosts: &SessionHostsConfig, channel: u64) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mapped = super::projected_channels(hosts).ok()?.remove(&channel)?;
    let mode = hosts.herdr.endpoints.get(&mapped.key)?.mode;
    let ChannelEndpoint {
        key,
        execution_node,
        socket_path,
        herdr_home,
        herdr_session,
    } = mapped;
    let bound = (
        channel,
        key,
        execution_node,
        socket_path,
        herdr_home,
        herdr_session,
        mode,
    );
    Some(Sha256::digest(serde_json::to_vec(&bound).ok()?).into())
}

pub(crate) struct NoHoldStore;
const HOLDS_NOT_INTRODUCED: &[u64] = &[];
impl HoldSource for NoHoldStore {
    const INTRODUCED: bool = false;
    async fn unresolved(&self, _: &BTreeSet<u64>) -> Result<Vec<u64>, String> {
        Ok(HOLDS_NOT_INTRODUCED.to_vec())
    }
}

pub(crate) struct PgRows<'a>(pub Option<&'a PgPool>);
impl RowSource for PgRows<'_> {
    fn available(&self) -> bool {
        self.0.is_some()
    }
    async fn rows(&self, channels: &BTreeSet<u64>) -> Result<Vec<MigrationRow>, String> {
        let pool = self.0.ok_or("postgres pool unavailable")?;
        let ids: Vec<String> = channels.iter().map(u64::to_string).collect();
        let rows = sqlx::query("SELECT id, provider, identity_kind, discord_token_hash, channel_id, hosted_execution FROM sessions WHERE channel_id = ANY($1) ORDER BY id")
            .bind(ids).fetch_all(pool).await.map_err(|e| e.to_string())?;
        rows.into_iter()
            .map(|row| {
                let channel: String = row.try_get("channel_id").map_err(|e| e.to_string())?;
                let raw: Option<serde_json::Value> =
                    row.try_get("hosted_execution").map_err(|e| e.to_string())?;
                let mut record = HostedRecord::decode(raw.as_ref());
                if let HostedRecord::Known(known) = &record {
                    let mut mismatch = false;
                    for (column, expected) in [
                        ("provider", known.owner.provider.as_str()),
                        ("identity_kind", "discord_channel"),
                        (
                            "discord_token_hash",
                            known.owner.discord_token_hash.as_str(),
                        ),
                        ("channel_id", known.owner.channel_id.as_str()),
                    ] {
                        let value: Option<String> =
                            row.try_get(column).map_err(|e| e.to_string())?;
                        if value.as_deref() != Some(expected) {
                            mismatch = true;
                            break;
                        }
                    }
                    if mismatch {
                        record = HostedRecord::Unknown(raw.clone().unwrap_or_default());
                    }
                }
                Ok(MigrationRow {
                    id: row.try_get("id").map_err(|e| e.to_string())?,
                    channel: channel.parse::<u64>().map_err(|e| e.to_string())?,
                    record,
                })
            })
            .collect()
    }
}

fn affected(old: &BootSessionHosts, new: &BootSessionHosts) -> BTreeSet<u64> {
    old.channels
        .keys()
        .chain(new.channels.keys())
        .copied()
        .filter(|id| {
            old.channels.get(id) != new.channels.get(id)
                || old
                    .channels
                    .get(id)
                    .and_then(|ep| old.config.herdr.endpoints.get(&ep.key))
                    != new
                        .channels
                        .get(id)
                        .and_then(|ep| new.config.herdr.endpoints.get(&ep.key))
                || old.local_node != new.local_node
        })
        .collect()
}

fn blocks(row: &MigrationRow, old: &BootSessionHosts, new: &BootSessionHosts) -> bool {
    match &row.record {
        HostedRecord::Legacy => false,
        HostedRecord::Unknown(_) => true,
        HostedRecord::Known(record) if record.state == HostedState::Retired => false,
        HostedRecord::Known(record) => {
            let Some(location) = &record.location else {
                return true;
            };
            let Some(endpoint) = new.channels.get(&row.channel) else {
                return true;
            };
            let locator_matches = |ep: &ChannelEndpoint| {
                location.execution_node == ep.execution_node
                    && location.endpoint_config_key == ep.key
                    && location.socket_addr == ep.socket_path.to_string_lossy()
                    && location.named_session == ep.herdr_session
            };
            let source = |boot: &BootSessionHosts| {
                boot.channels
                    .get(&row.channel)
                    .and_then(|ep| boot.config.herdr.endpoints.get(&ep.key))
                    .map(|ep| (ep.mode, ep.herdr_home.clone()))
            };
            old.local_node != new.local_node
                || !locator_matches(endpoint)
                || source(old) != source(new)
        }
    }
}

pub(crate) async fn select<S: ApprovalStore, R: RowSource, H: HoldSource>(
    candidate: BootSessionHosts,
    store: &S,
    rows: &R,
    holds: &H,
    read_only: bool,
) -> HostsBootSelection {
    let mut selected = select_projection(candidate, store, rows, holds, read_only).await;
    if let Some(diagnostic) = &mut selected.diagnostic {
        diagnostic.action = store.recovery_action(diagnostic.reason, selected.admission_stopped);
    }
    selected
}

async fn select_projection<S: ApprovalStore, R: RowSource, H: HoldSource>(
    candidate: BootSessionHosts,
    store: &S,
    rows: &R,
    holds: &H,
    read_only: bool,
) -> HostsBootSelection {
    let loaded = store.load();
    let (approved, warning) =
        match loaded {
            Loaded::Missing => {
                let selection = HostsBootSelection::new(candidate.clone(), &candidate);
                if !read_only {
                    if let Err(error) = store.save(&candidate) {
                        return selection.diagnose(
                            "approval_snapshot_not_durable",
                            &BTreeSet::new(),
                            vec![error],
                        );
                    }
                }
                return if rows.available() {
                    selection
                } else {
                    selection.diagnose(
                        "session_hosts_migration_unverified",
                        &BTreeSet::new(),
                        vec!["postgres pool unavailable".into()],
                    )
                };
            }
            Loaded::Unavailable(reason) => {
                let mut selection = HostsBootSelection::new(candidate.clone(), &candidate)
                    .diagnose(reason, &BTreeSet::new(), vec![]);
                selection.admission_stopped = true;
                return selection;
            }
            Loaded::Known(approved, warning) => (approved, warning),
        };
    let unchanged = Projection::of(&approved) == Projection::of(&candidate);
    let changes = if unchanged {
        BTreeSet::new()
    } else {
        affected(&approved, &candidate)
    };
    let applied = if unchanged {
        candidate.clone()
    } else {
        approved.clone()
    };
    let selection = HostsBootSelection::new(applied, &candidate);
    let recovering = warning == Some("approval_snapshot_not_durable");
    if let Some(reason) = warning.filter(|_| !recovering) {
        return selection.diagnose(reason, &changes, vec![]);
    }
    if unchanged && !recovering {
        return selection;
    }
    if read_only {
        return selection.diagnose(
            warning.unwrap_or("session_hosts_migration_pending"),
            &changes,
            vec![],
        );
    }
    let hold_changes = consent_holds::affected_user_default(&approved, &candidate, &changes);
    let consult = H::INTRODUCED && !hold_changes.is_empty();
    if !changes.is_empty() {
        if !rows.available() {
            return selection.diagnose(
                "session_hosts_migration_unverified",
                &changes,
                vec!["postgres pool unavailable".into()],
            );
        }
        let live = match rows.rows(&changes).await {
            Ok(live) => live,
            Err(error) => {
                return selection.diagnose(
                    "session_hosts_migration_unverified",
                    &changes,
                    vec![error],
                );
            }
        };
        let held = if consult {
            holds.blocking(&hold_changes, &candidate).await
        } else {
            Ok(HOLDS_NOT_INTRODUCED.to_vec())
        };
        let held = match held {
            Ok(held) => held,
            Err(error) => {
                return selection.diagnose(
                    "session_hosts_migration_unverified",
                    &changes,
                    vec![error],
                );
            }
        };
        let mut blockers: Vec<String> = live
            .iter()
            .filter(|row| changes.contains(&row.channel) && blocks(row, &approved, &candidate))
            .map(|row| format!("channel {} row {}", row.channel, row.id))
            .collect();
        blockers.extend(
            held.into_iter()
                .filter(|id| changes.contains(id))
                .map(|id| format!("channel {id} unresolved hold")),
        );
        if !blockers.is_empty() {
            return selection.diagnose("session_hosts_migration_refused", &changes, blockers);
        }
    }
    if let Err(error) = store.save(&candidate) {
        return selection.diagnose("approval_snapshot_not_durable", &changes, vec![error]);
    }
    // A missed record stays provable while this approval stands; the return command re-checks it.
    if consult && let Err(error) = holds.adopted(&candidate, &hold_changes).await {
        tracing::warn!(%error, "returned consent inputs keep their reservation after approval");
    }
    HostsBootSelection::new(candidate.clone(), &candidate)
}

#[cfg(all(test, unix))]
#[path = "migration_tests.rs"]
mod tests;
