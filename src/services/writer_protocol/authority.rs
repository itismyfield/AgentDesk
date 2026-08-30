//! Provider-independent, process-local writer authority.
//!
//! This module models semantic conflicts only. Namespace construction,
//! cross-process coordination, writer census, and production activation belong
//! to later W0b slices.

use super::ProviderDomain;
use std::sync::{Mutex, MutexGuard};

macro_rules! semantic_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name(u64);

        impl $name {
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            pub(crate) const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

semantic_id!(SessionKey);
semantic_id!(HookQueueKey);
semantic_id!(RestartAttemptKey);
semantic_id!(AuthoritySetId);
semantic_id!(HolderInstance);

/// Opaque namespace identity issued by the trusted layout catalog in W0b-a2.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuntimeNamespaceId(u64);

impl RuntimeNamespaceId {
    pub(crate) const fn from_catalog(value: u64) -> Self {
        Self(value)
    }
}

/// Opaque alias identity issued by the semantic artifact catalog.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AliasGroupId(u64);

impl AliasGroupId {
    pub(crate) const fn from_catalog(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum ArtifactSlot {
    RelayJsonl,
    NativeTranscript,
    NativeRollout,
    Prompt,
    InputFifo,
    OwnerMarker,
    WrapperScript,
    RuntimeMarker,
}

impl ArtifactSlot {
    const fn canonical_tag_v1(self) -> u8 {
        match self {
            Self::RelayJsonl => 0,
            Self::NativeTranscript => 1,
            Self::NativeRollout => 2,
            Self::Prompt => 3,
            Self::InputFifo => 4,
            Self::OwnerMarker => 5,
            Self::WrapperScript => 6,
            Self::RuntimeMarker => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ConfigSlot {
    AgentDesk,
    RuntimeOverride,
}

impl ConfigSlot {
    const fn canonical_tag_v1(self) -> u8 {
        match self {
            Self::AgentDesk => 0,
            Self::RuntimeOverride => 1,
        }
    }
}

/// Closed semantic coordinates. Raw filesystem components never enter overlap.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ConflictSegment {
    RuntimeSessions,
    Session(SessionKey),
    HookQueues,
    HookQueue(HookQueueKey),
    RestartRoot,
    RestartAttempt(RestartAttemptKey),
    ConfigRoot,
    ConfigArtifact(ConfigSlot),
    Artifact(ArtifactSlot),
}

impl ConflictSegment {
    fn encode_v1(self, encoded: &mut Vec<u8>) {
        match self {
            Self::RuntimeSessions => encoded.push(0),
            Self::Session(key) => {
                encoded.push(1);
                encoded.extend_from_slice(&key.get().to_be_bytes());
            }
            Self::HookQueues => encoded.push(2),
            Self::HookQueue(key) => {
                encoded.push(3);
                encoded.extend_from_slice(&key.get().to_be_bytes());
            }
            Self::RestartRoot => encoded.push(4),
            Self::RestartAttempt(key) => {
                encoded.push(5);
                encoded.extend_from_slice(&key.get().to_be_bytes());
            }
            Self::ConfigRoot => encoded.push(6),
            Self::ConfigArtifact(slot) => {
                encoded.extend_from_slice(&[7, slot.canonical_tag_v1()]);
            }
            Self::Artifact(slot) => {
                encoded.extend_from_slice(&[8, slot.canonical_tag_v1()]);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ConflictCoverage {
    Exact,
    Subtree,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConflictDomain {
    lineage: Vec<ConflictSegment>,
    coverage: ConflictCoverage,
}

impl ConflictDomain {
    pub(crate) fn try_new(
        lineage: impl IntoIterator<Item = ConflictSegment>,
        coverage: ConflictCoverage,
    ) -> Result<Self, EmptyConflictLineage> {
        let lineage = lineage.into_iter().collect::<Vec<_>>();
        if lineage.is_empty() {
            return Err(EmptyConflictLineage);
        }
        Ok(Self { lineage, coverage })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EmptyConflictLineage;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AuthorityKey {
    runtime_namespace: RuntimeNamespaceId,
    conflict_domain: ConflictDomain,
    alias_group: AliasGroupId,
}

impl AuthorityKey {
    pub(crate) fn new(
        runtime_namespace: RuntimeNamespaceId,
        conflict_domain: ConflictDomain,
        alias_group: AliasGroupId,
    ) -> Self {
        Self {
            runtime_namespace,
            conflict_domain,
            alias_group,
        }
    }

    pub(crate) fn canonical_bytes_v1(&self) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(64);
        encoded.extend_from_slice(b"agentdesk.writer-authority-key.v1\0");
        encoded.extend_from_slice(&self.runtime_namespace.0.to_be_bytes());
        encoded.extend_from_slice(&(self.conflict_domain.lineage.len() as u64).to_be_bytes());
        for segment in &self.conflict_domain.lineage {
            segment.encode_v1(&mut encoded);
        }
        encoded.push(match self.conflict_domain.coverage {
            ConflictCoverage::Exact => 0,
            ConflictCoverage::Subtree => 1,
        });
        encoded.extend_from_slice(&self.alias_group.0.to_be_bytes());
        encoded
    }

    pub(crate) fn canonical_digest_v1(&self) -> blake3::Hash {
        blake3::hash(&self.canonical_bytes_v1())
    }
}

pub(crate) fn overlaps(left: &AuthorityKey, right: &AuthorityKey) -> bool {
    if left.runtime_namespace != right.runtime_namespace {
        return false;
    }

    let left_domain = &left.conflict_domain;
    let right_domain = &right.conflict_domain;
    if left_domain.lineage == right_domain.lineage {
        return left.alias_group == right.alias_group
            || left_domain.coverage == ConflictCoverage::Subtree
            || right_domain.coverage == ConflictCoverage::Subtree;
    }

    (left_domain.coverage == ConflictCoverage::Subtree
        && right_domain.lineage.starts_with(&left_domain.lineage))
        || (right_domain.coverage == ConflictCoverage::Subtree
            && left_domain.lineage.starts_with(&right_domain.lineage))
}

/// Actor metadata for an in-process holder. Process and coordination identity
/// are added by W0b-a2; provider never participates in AuthorityKey equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthorityActor {
    provider: Option<ProviderDomain>,
    holder_instance: HolderInstance,
}

impl AuthorityActor {
    pub(crate) const fn new(
        provider: Option<ProviderDomain>,
        holder_instance: HolderInstance,
    ) -> Self {
        Self {
            provider,
            holder_instance,
        }
    }

    pub(crate) const fn provider(self) -> Option<ProviderDomain> {
        self.provider
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthorityRequest {
    set_id: AuthoritySetId,
    actor: AuthorityActor,
    keys: Vec<AuthorityKey>,
}

impl AuthorityRequest {
    pub(crate) fn new(
        set_id: AuthoritySetId,
        actor: AuthorityActor,
        keys: impl IntoIterator<Item = AuthorityKey>,
    ) -> Result<Self, AuthorityRequestError> {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        if keys.is_empty() {
            return Err(AuthorityRequestError::EmptyKeySet);
        }
        keys.sort_by_cached_key(AuthorityKey::canonical_bytes_v1);
        if keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AuthorityRequestError::DuplicateKey);
        }
        Ok(Self {
            set_id,
            actor,
            keys,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorityRequestError {
    EmptyKeySet,
    DuplicateKey,
}

#[derive(Debug)]
struct ActiveAuthoritySet {
    set_id: AuthoritySetId,
    actor: AuthorityActor,
    keys: Vec<AuthorityKey>,
}

/// One-mutex, process-local authority registry. It makes no cross-process,
/// host-lock, database-fence, or cross-host coordination claim.
#[derive(Debug, Default)]
pub(crate) struct AuthoritySetRegistry {
    active: Mutex<Vec<ActiveAuthoritySet>>,
}

impl AuthoritySetRegistry {
    pub(crate) fn new() -> Self {
        Self {
            active: Mutex::new(Vec::new()),
        }
    }

    fn active(&self) -> MutexGuard<'_, Vec<ActiveAuthoritySet>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn acquire(
        &self,
        request: AuthorityRequest,
    ) -> Result<AuthoritySet<'_>, AcquireError> {
        let mut active = self.active();
        if active.iter().any(|held| {
            held.set_id == request.set_id
                && held.actor.holder_instance == request.actor.holder_instance
        }) {
            return Err(AcquireError::IdentityAlreadyActive);
        }
        if active.iter().any(|held| {
            held.keys.iter().any(|held_key| {
                request
                    .keys
                    .iter()
                    .any(|requested_key| overlaps(held_key, requested_key))
            })
        }) {
            return Err(AcquireError::Conflict);
        }

        let set_id = request.set_id;
        let holder_instance = request.actor.holder_instance;
        active.push(ActiveAuthoritySet {
            set_id,
            actor: request.actor,
            keys: request.keys,
        });
        drop(active);
        Ok(AuthoritySet {
            registry: self,
            set_id,
            holder_instance,
            released: false,
        })
    }

    fn release(&self, set_id: AuthoritySetId, holder_instance: HolderInstance) {
        self.active()
            .retain(|held| held.set_id != set_id || held.actor.holder_instance != holder_instance);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcquireError {
    Conflict,
    IdentityAlreadyActive,
}

#[derive(Debug)]
pub(crate) struct AuthoritySet<'a> {
    registry: &'a AuthoritySetRegistry,
    set_id: AuthoritySetId,
    holder_instance: HolderInstance,
    released: bool,
}

impl AuthoritySet<'_> {
    pub(crate) fn release(mut self) {
        self.registry.release(self.set_id, self.holder_instance);
        self.released = true;
    }
}

impl Drop for AuthoritySet<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release(self.set_id, self.holder_instance);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::writer_protocol::ProviderDomain;

    fn key(session: u64, artifact: ArtifactSlot, coverage: ConflictCoverage) -> AuthorityKey {
        AuthorityKey::new(
            RuntimeNamespaceId::from_catalog(1),
            ConflictDomain::try_new(
                [
                    ConflictSegment::RuntimeSessions,
                    ConflictSegment::Session(SessionKey::new(session)),
                    ConflictSegment::Artifact(artifact),
                ],
                coverage,
            )
            .unwrap(),
            AliasGroupId::from_catalog(artifact as u64),
        )
    }

    fn session_boundary_key(session: u64, coverage: ConflictCoverage) -> AuthorityKey {
        AuthorityKey::new(
            RuntimeNamespaceId::from_catalog(1),
            ConflictDomain::try_new(
                [
                    ConflictSegment::RuntimeSessions,
                    ConflictSegment::Session(SessionKey::new(session)),
                ],
                coverage,
            )
            .unwrap(),
            AliasGroupId::from_catalog(90),
        )
    }

    fn request(
        set_id: u64,
        provider: ProviderDomain,
        holder: u64,
        keys: impl IntoIterator<Item = AuthorityKey>,
    ) -> AuthorityRequest {
        AuthorityRequest::new(
            AuthoritySetId::new(set_id),
            AuthorityActor::new(Some(provider), HolderInstance::new(holder)),
            keys,
        )
        .unwrap()
    }

    #[test]
    fn provider_metadata_does_not_discriminate_authority() {
        let registry = AuthoritySetRegistry::new();
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
        let registry = AuthoritySetRegistry::new();
        let ancestor = session_boundary_key(7, ConflictCoverage::Subtree);
        let _ancestor = registry
            .acquire(request(1, ProviderDomain::Claude, 1, [ancestor.clone()]))
            .unwrap();
        for artifact in [
            ArtifactSlot::RelayJsonl,
            ArtifactSlot::NativeTranscript,
            ArtifactSlot::NativeRollout,
            ArtifactSlot::Prompt,
            ArtifactSlot::InputFifo,
            ArtifactSlot::OwnerMarker,
            ArtifactSlot::WrapperScript,
            ArtifactSlot::RuntimeMarker,
        ] {
            let descendant = key(7, artifact, ConflictCoverage::Exact);
            assert!(overlaps(&ancestor, &descendant));
            assert!(overlaps(&descendant, &ancestor));
            assert_eq!(
                registry
                    .acquire(request(2, ProviderDomain::Codex, 2, [descendant]))
                    .unwrap_err(),
                AcquireError::Conflict
            );
        }
    }

    #[test]
    fn sibling_exact_domains_do_not_conflict() {
        let relay = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let prompt = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);

        assert!(!overlaps(&relay, &prompt));
    }

    #[test]
    fn exact_parent_does_not_overlap_descendant() {
        let exact_parent = session_boundary_key(7, ConflictCoverage::Exact);
        let descendant = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);

        assert!(!overlaps(&exact_parent, &descendant));
        assert!(!overlaps(&descendant, &exact_parent));
    }

    #[test]
    fn subtree_session_does_not_overlap_sibling_session() {
        let session_seven = session_boundary_key(7, ConflictCoverage::Subtree);
        let session_eight = key(8, ArtifactSlot::Prompt, ConflictCoverage::Exact);

        assert!(!overlaps(&session_seven, &session_eight));
        assert!(!overlaps(&session_eight, &session_seven));
    }

    #[test]
    fn canonical_v1_digests_and_request_order_ignore_provider_and_permutation() {
        let relay = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let prompt = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
        let forward = request(
            1,
            ProviderDomain::Claude,
            1,
            [relay.clone(), prompt.clone()],
        );
        let reverse = request(2, ProviderDomain::Codex, 2, [prompt, relay]);
        let canonical = |request: &AuthorityRequest| {
            request
                .keys
                .iter()
                .map(|key| (key.canonical_bytes_v1(), key.canonical_digest_v1()))
                .collect::<Vec<_>>()
        };
        let forward_canonical = canonical(&forward);
        assert_eq!(forward_canonical, canonical(&reverse));
        assert!(forward_canonical.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            forward_canonical
                .iter()
                .all(|(encoded, _)| encoded.starts_with(b"agentdesk.writer-authority-key.v1\0"))
        );

        let encodings = [
            ConflictSegment::RuntimeSessions,
            ConflictSegment::Session(SessionKey::new(7)),
            ConflictSegment::HookQueues,
            ConflictSegment::HookQueue(HookQueueKey::new(7)),
            ConflictSegment::RestartRoot,
            ConflictSegment::RestartAttempt(RestartAttemptKey::new(7)),
            ConflictSegment::ConfigRoot,
            ConflictSegment::ConfigArtifact(ConfigSlot::AgentDesk),
            ConflictSegment::ConfigArtifact(ConfigSlot::RuntimeOverride),
            ConflictSegment::Artifact(ArtifactSlot::RelayJsonl),
            ConflictSegment::Artifact(ArtifactSlot::NativeTranscript),
            ConflictSegment::Artifact(ArtifactSlot::NativeRollout),
            ConflictSegment::Artifact(ArtifactSlot::Prompt),
            ConflictSegment::Artifact(ArtifactSlot::InputFifo),
            ConflictSegment::Artifact(ArtifactSlot::OwnerMarker),
            ConflictSegment::Artifact(ArtifactSlot::WrapperScript),
            ConflictSegment::Artifact(ArtifactSlot::RuntimeMarker),
        ]
        .into_iter()
        .map(|segment| {
            let mut encoded = Vec::new();
            segment.encode_v1(&mut encoded);
            encoded
        })
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(encodings.len(), 17);
    }

    #[test]
    fn namespace_and_alias_dimensions_are_independent() {
        let original = key(7, ArtifactSlot::RuntimeMarker, ConflictCoverage::Exact);
        let mut other_namespace = original.clone();
        other_namespace.runtime_namespace = RuntimeNamespaceId::from_catalog(2);
        let mut other_alias = original.clone();
        other_alias.alias_group = AliasGroupId::from_catalog(999);

        assert!(!overlaps(&original, &other_namespace));
        assert!(!overlaps(&original, &other_alias));
        other_alias.conflict_domain.coverage = ConflictCoverage::Subtree;
        assert!(overlaps(&original, &other_alias));
        assert!(overlaps(&other_alias, &original));
    }

    #[test]
    fn invalid_raw_requests_do_not_pollute_registry() {
        let registry = AuthoritySetRegistry::new();
        let artifact = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let actor = AuthorityActor::new(Some(ProviderDomain::Claude), HolderInstance::new(1));
        assert_eq!(
            AuthorityRequest::new(
                AuthoritySetId::new(1),
                actor,
                std::iter::empty::<AuthorityKey>()
            )
            .unwrap_err(),
            AuthorityRequestError::EmptyKeySet
        );
        assert_eq!(
            AuthorityRequest::new(
                AuthoritySetId::new(2),
                actor,
                [artifact.clone(), artifact.clone()]
            )
            .unwrap_err(),
            AuthorityRequestError::DuplicateKey
        );
        assert!(
            registry
                .acquire(request(3, ProviderDomain::Codex, 2, [artifact]))
                .is_ok()
        );
    }

    #[test]
    fn multi_key_permutations_contend_identically() {
        for reverse in [false, true] {
            let registry = AuthoritySetRegistry::new();
            let relay = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
            let prompt = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
            let first = if reverse {
                [prompt.clone(), relay.clone()]
            } else {
                [relay.clone(), prompt.clone()]
            };
            let second = if reverse {
                [relay, prompt]
            } else {
                [prompt, relay]
            };
            let _held = registry
                .acquire(request(1, ProviderDomain::Claude, 1, first))
                .unwrap();
            let disjoint = key(8, ArtifactSlot::RuntimeMarker, ConflictCoverage::Exact);
            assert_eq!(
                registry
                    .acquire(request(1, ProviderDomain::Qwen, 1, [disjoint]))
                    .unwrap_err(),
                AcquireError::IdentityAlreadyActive
            );
            assert_eq!(
                registry
                    .acquire(request(2, ProviderDomain::Codex, 2, second))
                    .unwrap_err(),
                AcquireError::Conflict
            );
        }
    }

    #[test]
    fn panic_unwind_releases_entire_set() {
        let registry = AuthoritySetRegistry::new();
        let relay = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let prompt = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = registry
                .acquire(request(
                    1,
                    ProviderDomain::Claude,
                    1,
                    [relay.clone(), prompt.clone()],
                ))
                .unwrap();
            panic!("exercise AuthoritySet::drop");
        }));
        assert!(unwind.is_err());
        assert!(
            registry
                .acquire(request(2, ProviderDomain::Codex, 2, [relay, prompt]))
                .is_ok()
        );
    }

    #[test]
    fn failed_multi_key_acquisition_rolls_back_atomically() {
        let registry = AuthoritySetRegistry::new();
        let available = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let occupied = key(7, ArtifactSlot::Prompt, ConflictCoverage::Exact);
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
    fn stale_release_does_not_clear_reused_set_id_with_new_holder() {
        assert_stale_release_does_not_clear_successor(1, 1, 1, 2);
    }

    #[test]
    fn stale_release_does_not_clear_reused_holder_with_new_set_id() {
        assert_stale_release_does_not_clear_successor(1, 1, 2, 1);
    }

    fn assert_stale_release_does_not_clear_successor(
        stale_set_id: u64,
        stale_holder_id: u64,
        successor_set_id: u64,
        successor_holder_id: u64,
    ) {
        let registry = AuthoritySetRegistry::new();
        let contested = key(7, ArtifactSlot::RelayJsonl, ConflictCoverage::Exact);
        let stale_set = AuthoritySetId::new(stale_set_id);
        let stale_holder = HolderInstance::new(stale_holder_id);
        drop(
            registry
                .acquire(request(
                    stale_set.get(),
                    ProviderDomain::Claude,
                    stale_holder.get(),
                    [contested.clone()],
                ))
                .unwrap(),
        );
        let _successor = registry
            .acquire(request(
                successor_set_id,
                ProviderDomain::Codex,
                successor_holder_id,
                [contested.clone()],
            ))
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
