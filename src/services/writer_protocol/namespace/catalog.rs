//! Reviewed physical roots bound to stable writer-authority coordinates.
use super::lexical::{
    LexicalError, NormalizedAbsolute, SealedLexicalRoot, register_posix_exact_root,
    register_windows_drive_exact_root, register_windows_unc_exact_root,
};
use crate::services::writer_protocol::authority::{
    ArtifactSlot, AuthorityActor, AuthoritySetId, SessionKey,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootOwner {
    Posix = 1,
    Drive = 2,
    Unc = 3,
}

#[derive(Clone, Copy)]
pub(in crate::services::writer_protocol) struct RootPair<'a> {
    pub canonical: &'a [u8],
    pub legacy: &'a [u8],
}

#[derive(Clone, Copy)]
pub(in crate::services::writer_protocol) struct ReviewedRoots<'a> {
    pub posix: RootPair<'a>,
    pub drive: RootPair<'a>,
    pub unc: RootPair<'a>,
}

#[derive(Clone, Debug)]
struct Binding {
    owner: RootOwner,
    root: SealedLexicalRoot,
    target: NormalizedAbsolute,
}

pub(in crate::services::writer_protocol) struct CatalogAuthoritySet {
    set_id: AuthoritySetId,
    actor: AuthorityActor,
    session: SessionKey,
    artifacts: Vec<(u64, u64, ArtifactSlot)>,
}

pub(in crate::services::writer_protocol) type CatalogAuthorityParts = (
    AuthoritySetId,
    AuthorityActor,
    SessionKey,
    Vec<(u64, u64, ArtifactSlot)>,
);

impl CatalogAuthoritySet {
    pub(in crate::services::writer_protocol) fn into_parts(self) -> CatalogAuthorityParts {
        (self.set_id, self.actor, self.session, self.artifacts)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::writer_protocol) enum CatalogError {
    InvalidRoot,
    MalformedRoot,
    IncompleteRoots,
    OverlappingRoots,
    DuplicateLogicalTuple,
    InvalidArtifactSet,
    UnknownRoot,
    AmbiguousRoot,
}

impl From<LexicalError> for CatalogError {
    #[rustfmt::skip]
    fn from(error: LexicalError) -> Self { match error { LexicalError::MalformedRoot => Self::MalformedRoot, _ => Self::InvalidRoot } }
}

pub(in crate::services::writer_protocol) struct Catalog {
    bindings: Vec<Binding>,
}

pub(in crate::services::writer_protocol) const ARTIFACTS: &[(u64, ArtifactSlot)] = &[
    (1, ArtifactSlot::RelayJsonl),
    (2, ArtifactSlot::NativeTranscript),
    (3, ArtifactSlot::NativeRollout),
    (4, ArtifactSlot::Prompt),
    (5, ArtifactSlot::InputFifo),
    (6, ArtifactSlot::OwnerMarker),
    (7, ArtifactSlot::WrapperScript),
    (8, ArtifactSlot::RuntimeMarker),
];

fn make_binding(
    owner: RootOwner,
    spelling: &[u8],
    canonical: bool,
) -> Result<Binding, CatalogError> {
    let root = match (owner, canonical) {
        (RootOwner::Posix, _) => register_posix_exact_root(spelling),
        (RootOwner::Drive, _) => register_windows_drive_exact_root(spelling),
        (RootOwner::Unc, true) => register_windows_unc_exact_root(br"\\server\share"),
        (RootOwner::Unc, false) => register_windows_unc_exact_root(spelling),
    }
    .map_err(CatalogError::from)?;
    let target = root
        .normalize_candidate(spelling)
        .map_err(CatalogError::from)?
        .ok_or(CatalogError::MalformedRoot)?;
    Ok(Binding {
        owner,
        root,
        target,
    })
}

fn artifact_id(artifact: ArtifactSlot) -> Option<u64> {
    ARTIFACTS
        .iter()
        .find_map(|(id, candidate)| (*candidate == artifact).then_some(*id))
}

impl Catalog {
    pub(in crate::services::writer_protocol) fn build(
        roots: ReviewedRoots<'_>,
    ) -> Result<Self, CatalogError> {
        Self::build_with_artifacts(roots, ARTIFACTS)
    }

    fn build_with_artifacts(
        roots: ReviewedRoots<'_>,
        artifacts: &[(u64, ArtifactSlot)],
    ) -> Result<Self, CatalogError> {
        let mut bindings = Vec::with_capacity(6);
        for (owner, pair) in [
            (RootOwner::Posix, roots.posix),
            (RootOwner::Drive, roots.drive),
            (RootOwner::Unc, roots.unc),
        ] {
            bindings.push(make_binding(owner, pair.canonical, true)?);
            bindings.push(make_binding(owner, pair.legacy, false)?);
        }
        if [RootOwner::Posix, RootOwner::Drive, RootOwner::Unc]
            .into_iter()
            .any(|owner| {
                bindings
                    .iter()
                    .filter(|binding| binding.owner == owner)
                    .count()
                    != 2
            })
        {
            return Err(CatalogError::IncompleteRoots);
        }
        for left in 0..bindings.len() {
            for right in left + 1..bindings.len() {
                if bindings[left].root.overlaps(&bindings[right].root) {
                    return Err(CatalogError::OverlappingRoots);
                }
            }
        }
        let mut tuples = Vec::new();
        for owner in [RootOwner::Posix, RootOwner::Drive, RootOwner::Unc] {
            for &(id, artifact) in artifacts {
                let tuple = (owner as u64, id, artifact);
                if tuples.contains(&tuple) {
                    return Err(CatalogError::DuplicateLogicalTuple);
                }
                tuples.push(tuple);
            }
        }
        if artifacts.len() != ARTIFACTS.len() {
            return Err(CatalogError::InvalidArtifactSet);
        }
        Ok(Self { bindings })
    }

    fn owner(&self, spelling: &[u8]) -> Result<RootOwner, CatalogError> {
        let mut owners = Vec::new();
        for binding in &self.bindings {
            let Ok(Some(value)) = binding.root.normalize_candidate(spelling) else {
                continue;
            };
            if value == binding.target && !owners.contains(&binding.owner) {
                owners.push(binding.owner);
            }
        }
        match owners.as_slice() {
            [owner] => Ok(*owner),
            [] => Err(CatalogError::UnknownRoot),
            _ => Err(CatalogError::AmbiguousRoot),
        }
    }

    pub(in crate::services::writer_protocol) fn issue(
        &self,
        root: &[u8],
        session: SessionKey,
        artifacts: &[(u64, ArtifactSlot)],
        set_id: AuthoritySetId,
        actor: AuthorityActor,
    ) -> Result<CatalogAuthoritySet, CatalogError> {
        let owner = self.owner(root)?;
        let mut requested = artifacts.to_vec();
        requested.sort_unstable();
        if requested != ARTIFACTS {
            return Err(CatalogError::InvalidArtifactSet);
        }
        let mut issued = Vec::with_capacity(ARTIFACTS.len());
        for &(_, artifact) in ARTIFACTS {
            let id = artifact_id(artifact).ok_or(CatalogError::InvalidArtifactSet)?;
            issued.push((owner as u64, 16 * owner as u64 + id, artifact));
        }
        Ok(CatalogAuthoritySet {
            set_id,
            actor,
            session,
            artifacts: issued,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::writer_protocol::{
        ProviderDomain,
        authority::{
            AcquireError, AuthorityActor, AuthorityRequest, AuthoritySetId, AuthoritySetRegistry,
            HolderInstance, SessionKey,
        },
    };
    use std::collections::BTreeSet;

    type Observation = Vec<(u64, u64, u64, ArtifactSlot)>;
    #[rustfmt::skip]
    const ROOTS: [(&[u8], &[u8]); 3] = [(b"/Runtime/runtime/sessions", b"/tmp"), (br"C:\Runtime\runtime\sessions", br"C:\Temp"), (br"\\server\share\runtime\sessions", br"\\server\Temp")];

    fn pair(canonical: &'static [u8], legacy: &'static [u8]) -> RootPair<'static> {
        RootPair { canonical, legacy }
    }

    fn reviewed_roots() -> ReviewedRoots<'static> {
        ReviewedRoots {
            posix: pair(ROOTS[0].0, ROOTS[0].1),
            drive: pair(ROOTS[1].0, ROOTS[1].1),
            unc: pair(ROOTS[2].0, ROOTS[2].1),
        }
    }

    fn identity(set: u64, holder: u64) -> (AuthoritySetId, AuthorityActor) {
        (
            AuthoritySetId::new(set),
            AuthorityActor::new(Some(ProviderDomain::Codex), HolderInstance::new(holder)),
        )
    }

    #[rustfmt::skip]
    fn issue(catalog: &Catalog, root: &[u8], session: u64, set: u64, holder: u64) -> Result<CatalogAuthoritySet, CatalogError> {
        let (set_id, actor) = identity(set, holder);
        catalog.issue(root, SessionKey::new(session), ARTIFACTS, set_id, actor)
    }

    #[rustfmt::skip]
    fn request(catalog: &Catalog, root: &[u8], session: u64, set: u64, holder: u64) -> AuthorityRequest {
        AuthorityRequest::from_catalog(issue(catalog, root, session, set, holder).unwrap()).unwrap()
    }

    fn observation(catalog: &Catalog, root: &[u8], session: u64) -> Observation {
        let (_, _, session, artifacts) =
            issue(catalog, root, session, 91, 92).unwrap().into_parts();
        artifacts
            .into_iter()
            .map(|(namespace, alias, artifact)| (namespace, session.get(), alias, artifact))
            .collect()
    }

    fn assert_old_unc_spelling_is_malformed() {
        let mut roots = reviewed_roots();
        roots.unc.legacy = br"C:\Temp";
        assert!(matches!(
            Catalog::build(roots),
            Err(CatalogError::MalformedRoot)
        ));
        assert!(Catalog::build(reviewed_roots()).is_ok());
    }

    fn assert_one_winner(catalog: &Catalog) {
        for round in 0..16 {
            let registry = AuthoritySetRegistry::new();
            let start = std::sync::Barrier::new(3);
            let outcomes = std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    start.wait();
                    registry.acquire(request(catalog, b"/tmp", 7, 100 + round, 1))
                });
                let second = scope.spawn(|| {
                    start.wait();
                    registry.acquire(request(catalog, b"/tmp", 7, 200 + round, 2))
                });
                start.wait();
                [first.join().unwrap(), second.join().unwrap()]
            });
            assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        }
    }

    #[rustfmt::skip]
    #[test]
    fn canonical_and_legacy_session_aliases_share_exact_authority_key() {
        let catalog = Catalog::build(reviewed_roots()).unwrap();
        for (canonical, legacy) in ROOTS {
            assert_eq!(observation(&catalog, canonical, 7), observation(&catalog, legacy, 7));
        }
        let drive = observation(&catalog, ROOTS[1].1, 7);
        let unc = observation(&catalog, ROOTS[2].1, 7);
        assert_ne!(drive, unc);
        assert_ne!(drive, observation(&catalog, ROOTS[1].1, 8));
        assert_eq!(drive.iter().map(|entry| entry.2).collect::<BTreeSet<_>>().len(), 8);
        let registry = AuthoritySetRegistry::new();
        let _session = registry.acquire(request(&catalog, ROOTS[1].1, 7, 7, 7)).unwrap();
        assert!(registry.acquire(request(&catalog, ROOTS[1].1, 8, 8, 8)).is_ok());
        assert_old_unc_spelling_is_malformed();
    }

    #[rustfmt::skip]
    #[test]
    fn sealed_roots_issue_only_exact_reviewed_artifact_bindings() {
        let catalog = Catalog::build(reviewed_roots()).unwrap();
        for root in ROOTS.into_iter().flat_map(|pair| [pair.0, pair.1]) {
            let (set_id, actor) = identity(10, 11);
            let issued = catalog.issue(root, SessionKey::new(7), ARTIFACTS, set_id, actor).unwrap();
            let (actual_set, actual_actor, session, artifacts) = issued.into_parts();
            assert_eq!((actual_set, actual_actor, session), (set_id, actor, SessionKey::new(7)));
            assert_eq!(artifacts.len(), 8);
            assert_eq!(artifacts.iter().map(|entry| entry.2).collect::<Vec<_>>(), vec![ArtifactSlot::RelayJsonl, ArtifactSlot::NativeTranscript, ArtifactSlot::NativeRollout, ArtifactSlot::Prompt, ArtifactSlot::InputFifo, ArtifactSlot::OwnerMarker, ArtifactSlot::WrapperScript, ArtifactSlot::RuntimeMarker]);
        }
        let (set_id, actor) = identity(12, 13);
        for invalid in [ARTIFACTS[..7].to_vec(), vec![ARTIFACTS[0]; 8], {
            let mut redirected = ARTIFACTS.to_vec();
            redirected[0].1 = ArtifactSlot::RuntimeMarker;
            redirected
        }] {
            let result = catalog.issue(b"/tmp", SessionKey::new(7), &invalid, set_id, actor);
            assert_eq!(result.err(), Some(CatalogError::InvalidArtifactSet));
        }
        let registry = AuthoritySetRegistry::new();
        let _posix = registry.acquire(request(&catalog, ROOTS[0].1, 7, 20, 20)).unwrap();
        let _drive = registry.acquire(request(&catalog, ROOTS[1].1, 7, 21, 21)).unwrap();
        let _unc = registry.acquire(request(&catalog, ROOTS[2].1, 7, 22, 22)).unwrap();
        let _actor = registry.acquire(request(&catalog, ROOTS[0].1, 8, 23, 1)).unwrap();
        assert!(registry.acquire(request(&catalog, ROOTS[1].1, 8, 23, 2)).is_ok());
    }

    #[rustfmt::skip]
    #[test]
    fn duplicate_and_overlapping_catalog_bindings_are_rejected_atomically() {
        let mut overlapping = reviewed_roots();
        overlapping.posix.legacy = b"/Runtime/runtime";
        assert!(matches!(Catalog::build(overlapping), Err(CatalogError::OverlappingRoots)));
        let mut duplicate = ARTIFACTS.to_vec();
        duplicate[1] = duplicate[0];
        assert!(matches!(Catalog::build_with_artifacts(reviewed_roots(), &duplicate), Err(CatalogError::DuplicateLogicalTuple)));
        let catalog = Catalog::build(reviewed_roots()).unwrap();
        assert_one_winner(&catalog);
        let registry = AuthoritySetRegistry::new();
        let first = registry.acquire(request(&catalog, b"/tmp", 7, 30, 30)).unwrap();
        let successor = registry.acquire(request(&catalog, b"/tmp", 8, 30, 31)).unwrap();
        drop(first);
        let conflict = registry.acquire(request(&catalog, b"/tmp", 8, 32, 32));
        assert_eq!(conflict.unwrap_err(), AcquireError::Conflict);
        drop(successor);
        assert!(registry.acquire(request(&catalog, b"/tmp", 8, 33, 33)).is_ok());
    }

    #[rustfmt::skip]
    #[test]
    fn catalog_bindings_are_deterministic_and_injective() {
        let first = Catalog::build(reviewed_roots()).unwrap();
        let second = Catalog::build(reviewed_roots()).unwrap();
        let mut coordinates = BTreeSet::new();
        for (index, (root, _)) in ROOTS.into_iter().enumerate() {
            for session in [7, 8] {
                let forward = observation(&first, root, session);
                assert_eq!(forward, observation(&second, root, session));
                assert!(forward.iter().all(|entry| entry.0 == index as u64 + 1));
                coordinates.extend(forward);
            }
        }
        assert_eq!(coordinates.len(), 48);
        let all = ROOTS.into_iter().flat_map(|pair| [pair.0, pair.1]);
        let unique = all.map(|root| observation(&first, root, 7)).collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), 3);
        let mut reverse = ARTIFACTS.to_vec();
        reverse.reverse();
        let (set_id, actor) = identity(70, 70);
        let reversed = first.issue(ROOTS[0].0, SessionKey::new(7), &reverse, set_id, actor).unwrap().into_parts().3;
        assert_eq!(reversed, issue(&first, ROOTS[0].0, 7, 71, 71).unwrap().into_parts().3);
    }

    #[rustfmt::skip]
    #[test]
    fn unknown_roots_and_artifacts_never_receive_fallback_identity() {
        let catalog = Catalog::build(reviewed_roots()).unwrap();
        let (set_id, actor) = identity(40, 40);
        #[rustfmt::skip]
        let hostile: [&[u8]; 6] = [b"relative", b"/unknown", b"/tmp/../escape", br"\\?\server\Temp", br"\\server\share\Temp", b"/\xff"];
        for root in hostile {
            let result = catalog.issue(root, SessionKey::new(7), ARTIFACTS, set_id, actor);
            assert!(result.is_err());
        }
        let mut unknown = ARTIFACTS.to_vec();
        unknown[0].0 = 99;
        let invalid = catalog.issue(ROOTS[2].1, SessionKey::new(7), &unknown, set_id, actor);
        assert_eq!(invalid.err(), Some(CatalogError::InvalidArtifactSet));
        assert!(issue(&catalog, ROOTS[2].1, 7, 40, 40).is_ok());
        let mut ambiguous = Catalog::build(reviewed_roots()).unwrap();
        let mut forged = ambiguous.bindings[0].clone();
        forged.owner = RootOwner::Drive;
        ambiguous.bindings.push(forged);
        assert_eq!(ambiguous.owner(ROOTS[0].0), Err(CatalogError::AmbiguousRoot));
    }
}
