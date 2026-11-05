//! A local tmux channel's own in-flight session, found before any watcher claims it.
use super::inflight_diagnostics::{self as diagnostics, Rejection};
use super::*;

pub(in crate::services::discord::turn_presence::activity) struct Admission {
    pub(in crate::services::discord::turn_presence::activity) name: String,
    snapshot: Snapshot,
    namespace: Observation,
}
impl Admission {
    pub(in crate::services::discord::turn_presence::activity) async fn current(
        &self,
    ) -> Result<(inflight_identity::Kind, Vec<&'static str>), Rejection> {
        Box::pin(self.snapshot.revalidate_inflight(&self.namespace)).await
    }
    pub(in crate::services::discord::turn_presence::activity) fn report(
        &self,
        result: Result<(inflight_identity::Kind, Vec<&'static str>), Rejection>,
    ) {
        let (result, ignored) = match result {
            Ok((kind, ignored)) => (Ok(kind), ignored),
            Err(error) => (Err(error), Vec::new()),
        };
        diagnostics::record(
            &self.snapshot.provider,
            self.snapshot.channel,
            "publication",
            "publication_fence",
            result,
            &ignored,
        );
    }
    pub(in crate::services::discord::turn_presence::activity) fn superseded(
        &self,
        field: &'static str,
    ) {
        self.report(Err(Rejection::Superseded {
            phase: "publication_fence",
            field,
        }));
    }
}

pub(in crate::services::discord::turn_presence::activity) async fn admitted(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<Admission> {
    match inflight::load_inflight_state_read_only_result(provider, channel.get()) {
        Ok(Some(row)) if row.tmux_session_name.is_some() => {}
        Ok(_) => return None,
        Err(_) => {
            diagnostics::record(
                provider,
                channel,
                "admission",
                "precheck",
                Err(Rejection::ReadFailed {
                    phase: "precheck",
                    field: "row",
                }),
                &[],
            );
            return None;
        }
    }
    for attempt in 0..2 {
        match collect(shared, provider, channel).await {
            Ok((admission, kind, ignored)) => {
                diagnostics::record(
                    provider,
                    channel,
                    "admission",
                    "admission_pair",
                    Ok(kind),
                    &ignored,
                );
                return Some(admission);
            }
            Err(error) => {
                let retry = attempt == 0 && error.retryable();
                diagnostics::record(
                    provider,
                    channel,
                    "admission",
                    "admission_pair",
                    Err(error),
                    &[],
                );
                if !retry {
                    break;
                }
            }
        }
    }
    None
}

async fn collect(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<(Admission, inflight_identity::Kind, Vec<&'static str>), Rejection> {
    let ports = Arc::new(LivePorts {
        #[cfg(test)]
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let (snapshot, namespace, next) = Snapshot::collect_pair(shared, provider, channel, ports)
        .await
        .map_err(|error| Rejection::collected("admission_collect", &error))?;
    let kind = snapshot.same_inflight_inputs(&next, "admission_pair")?;
    let name = snapshot.inflight_session_checked(shared, &namespace.inventory)?;
    let ignored = inflight_identity::ignored(&snapshot.files, &next.files);
    Ok((
        Admission {
            name,
            snapshot,
            namespace,
        },
        kind,
        ignored,
    ))
}

impl Snapshot {
    #[cfg(test)]
    fn inflight_session(&self, shared: &SharedData, inventory: &Inventory) -> Option<String> {
        self.inflight_session_checked(shared, inventory).ok()
    }
    fn inflight_session_checked(
        &self,
        shared: &SharedData,
        inventory: &Inventory,
    ) -> Result<String, Rejection> {
        let reject = |guard| Rejection::GuardRejected { guard };
        let candidates = self
            .files
            .candidates
            .as_ref()
            .map_err(|_| reject("candidate_read"))?;
        let name = candidates
            .current
            .as_deref()
            .ok_or_else(|| reject("current_missing"))?;
        let Inventory::Sessions(live) = inventory else {
            return Err(reject("inventory_unreadable"));
        };
        let ownership = self
            .ownership
            .as_ref()
            .ok_or_else(|| reject("membership"))?;
        let owners = self.effective_owners();
        macro_rules! require {
            ($condition:expr, $guard:literal) => {
                if !$condition {
                    return Err(reject($guard));
                }
            };
        }
        require!(candidates.inflight, "candidate_inflight_missing");
        require!(!candidates.conflict, "candidate_conflict");
        require!(
            candidates.names.iter().all(|c| c.name == name),
            "candidate_name_disagree"
        );
        require!(
            matches!(&self.files.row, Ok(Some(row)) if row.tmux_session_name.as_deref() == Some(name)),
            "row_name_disagree"
        );
        require!(
            matches!(self.exact, HostedLookup::Found(_)),
            "exact_not_found"
        );
        require!(self.exact == self.canonical, "exact_canonical_disagree");
        require!(
            self.scope(shared).result == Scope::Hold("inflight_present"),
            "scope_disagree"
        );
        require!(
            live.iter().any(|session| session == name),
            "target_not_live"
        );
        ownership
            .admission_guard(name, self.channel.get())
            .map_err(reject)?;
        if live.iter().any(|session| {
            session != name && owners.get(session) == Some(&Some(self.channel.get()))
        }) {
            return Err(reject("other_live_owned_session"));
        }
        Ok(name.to_owned())
    }
}

#[cfg(test)]
#[path = "inflight_bound_tests.rs"]
mod tests;
