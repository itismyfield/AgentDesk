//! Contract tests for provider-independent, process-local writer authority.

#[cfg(test)]
mod red_contract {
    use super::super::ProviderDomain;

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct RuntimeNamespaceId(pub(super) u64);

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct AliasGroupId(pub(super) u64);

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct AuthoritySetId(pub(super) u64);

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct HolderInstance(pub(super) u64);

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) enum ArtifactSlot {
        RelayJsonl,
        Prompt,
    }

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) enum ConflictSegment {
        RuntimeSessions,
        Session(u64),
        Artifact(ArtifactSlot),
    }

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) enum ConflictCoverage {
        Exact,
        Subtree,
    }

    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct ConflictDomain {
        pub(super) lineage: Vec<ConflictSegment>,
        pub(super) coverage: ConflictCoverage,
    }

    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(super) struct AuthorityKey {
        pub(super) namespace: RuntimeNamespaceId,
        pub(super) domain: ConflictDomain,
        pub(super) alias: AliasGroupId,
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) struct AuthorityActor {
        pub(super) provider: ProviderDomain,
        pub(super) holder: HolderInstance,
    }

    #[derive(Clone, Debug)]
    pub(super) struct AuthorityRequest {
        pub(super) set_id: AuthoritySetId,
        pub(super) actor: AuthorityActor,
        pub(super) keys: Vec<AuthorityKey>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum AcquireError {
        Conflict,
    }

    #[derive(Debug, Default)]
    pub(super) struct AuthoritySetRegistry;

    #[derive(Debug)]
    pub(super) struct AuthoritySet;

    impl AuthoritySetRegistry {
        pub(super) fn acquire(
            &self,
            request: AuthorityRequest,
        ) -> Result<AuthoritySet, AcquireError> {
            let _ = (
                request.set_id,
                request.actor.provider,
                request.actor.holder,
                request.keys,
            );
            Ok(AuthoritySet)
        }

        pub(super) fn release(&self, _set_id: AuthoritySetId, _holder: HolderInstance) {}
    }

    pub(super) fn overlaps(_left: &AuthorityKey, _right: &AuthorityKey) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::red_contract::*;
    use crate::services::writer_protocol::ProviderDomain;

    fn key(session: u64, artifact: ArtifactSlot, coverage: ConflictCoverage) -> AuthorityKey {
        AuthorityKey {
            namespace: RuntimeNamespaceId(1),
            domain: ConflictDomain {
                lineage: vec![
                    ConflictSegment::RuntimeSessions,
                    ConflictSegment::Session(session),
                    ConflictSegment::Artifact(artifact),
                ],
                coverage,
            },
            alias: AliasGroupId(artifact as u64),
        }
    }

    fn request(
        set_id: u64,
        provider: ProviderDomain,
        holder: u64,
        keys: impl IntoIterator<Item = AuthorityKey>,
    ) -> AuthorityRequest {
        AuthorityRequest {
            set_id: AuthoritySetId(set_id),
            actor: AuthorityActor {
                provider,
                holder: HolderInstance(holder),
            },
            keys: keys.into_iter().collect(),
        }
    }

    #[test]
    fn provider_metadata_does_not_discriminate_authority() {
        let registry = AuthoritySetRegistry;
        let contested = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let _claude = registry
            .acquire(request(1, ProviderDomain::Claude, 1, [contested.clone()]))
            .unwrap();

        assert_eq!(
            registry
                .acquire(request(2, ProviderDomain::Codex, 2, [contested]))
                .unwrap_err(),
            AcquireError::Conflict
        );
    }

    #[test]
    fn subtree_ancestor_conflicts_with_descendant() {
        let registry = AuthoritySetRegistry;
        let mut ancestor = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Subtree);
        ancestor.domain.lineage.pop();
        let descendant = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
        let _ancestor = registry
            .acquire(request(1, ProviderDomain::Claude, 1, [ancestor]))
            .unwrap();

        assert_eq!(
            registry
                .acquire(request(2, ProviderDomain::Codex, 2, [descendant]))
                .unwrap_err(),
            AcquireError::Conflict
        );
    }

    #[test]
    fn sibling_exact_domains_do_not_conflict() {
        let relay = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let prompt = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);

        assert!(!overlaps(&relay, &prompt));
    }

    #[test]
    fn failed_multi_key_acquisition_rolls_back_atomically() {
        let registry = AuthoritySetRegistry;
        let available = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
        let occupied = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let _occupied = registry
            .acquire(request(1, ProviderDomain::Claude, 1, [occupied.clone()]))
            .unwrap();

        assert_eq!(
            registry
                .acquire(request(
                    2,
                    ProviderDomain::Codex,
                    2,
                    [available.clone(), occupied],
                ))
                .unwrap_err(),
            AcquireError::Conflict
        );
        assert!(
            registry
                .acquire(request(3, ProviderDomain::Qwen, 3, [available]))
                .is_ok()
        );
    }

    #[test]
    fn stale_release_cannot_clear_successor() {
        let registry = AuthoritySetRegistry;
        let contested = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let stale_set = AuthoritySetId(1);
        let stale_holder = HolderInstance(1);
        drop(
            registry
                .acquire(request(
                    stale_set.0,
                    ProviderDomain::Claude,
                    stale_holder.0,
                    [contested.clone()],
                ))
                .unwrap(),
        );
        let _successor = registry
            .acquire(request(2, ProviderDomain::Codex, 2, [contested.clone()]))
            .unwrap();

        registry.release(stale_set, stale_holder);
        assert_eq!(
            registry
                .acquire(request(3, ProviderDomain::Qwen, 3, [contested]))
                .unwrap_err(),
            AcquireError::Conflict
        );
    }
}
