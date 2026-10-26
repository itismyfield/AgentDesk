//! A local tmux channel's own in-flight session, found before any watcher claims it.
use super::*;

/// An admitted session and the pinned Unbound inputs that admitted it.
pub(in crate::services::discord::turn_presence::activity) struct Admission {
    pub(in crate::services::discord::turn_presence::activity) name: String,
    snapshot: Snapshot,
    namespace: Observation,
}

impl Admission {
    /// Whether the inputs that admitted the session still read the same, rechecked as the Unbound
    /// read rechecks its absence before publication.
    pub(in crate::services::discord::turn_presence::activity) async fn current(&self) -> bool {
        Box::pin(self.snapshot.revalidate(&self.namespace)).await
    }
}

/// The tmux session this channel's in-flight turn runs in once both pinned Unbound snapshots name
/// it alone, resolve its sessions row as the channel's and see it live; `None` keeps the Unbound read.
pub(in crate::services::discord::turn_presence::activity) async fn admitted(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<Admission> {
    // Only an in-flight row can admit, so a quiet channel skips the second collection.
    if !matches!(
        inflight::load_inflight_state_read_only_result(provider, channel.get()),
        Ok(Some(row)) if row.tmux_session_name.is_some()
    ) {
        return None;
    }
    let ports = Arc::new(LivePorts {
        #[cfg(test)]
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let (snapshot, namespace, next) = Snapshot::collect_pair(shared, provider, channel, ports)
        .await
        .ok()?;
    if !snapshot.same_inputs(&next) {
        return None;
    }
    let name = snapshot.inflight_session(shared, &namespace.inventory)?;
    Some(Admission {
        name,
        snapshot,
        namespace,
    })
}

impl Snapshot {
    fn inflight_session(&self, shared: &SharedData, inventory: &Inventory) -> Option<String> {
        let candidates = self.files.candidates.as_ref().ok()?;
        let name = candidates.current.as_deref()?;
        let Inventory::Sessions(live) = inventory else {
            return None;
        };
        let channel = self.channel.get();
        let owners = self.effective_owners();
        let ownership = self.ownership.as_ref()?;
        // Every candidate, the row and the sessions row name this session; any other answer holds.
        let own = candidates.inflight
            && !candidates.conflict
            && candidates.names.iter().all(|c| c.name == name)
            && matches!(&self.files.row, Ok(Some(row)) if row.tmux_session_name.as_deref() == Some(name))
            && matches!(self.exact, HostedLookup::Found(_))
            && self.exact == self.canonical
            && self.scope(shared).result == Scope::Hold("inflight_present")
            && live.iter().any(|session| session == name)
            && ownership.admits(name, channel)
            && !live
                .iter()
                .any(|session| session != name && owners.get(session) == Some(&Some(channel)));
        own.then(|| name.to_owned())
    }
}

#[cfg(test)]
#[path = "inflight_bound_tests.rs"]
mod tests;
