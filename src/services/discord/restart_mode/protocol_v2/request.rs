use super::{
    fs::{
        self, ConfinedDir, ConfinedRuntimeRoot, FsError, FsErrorKind, PublicationCapabilityV2,
        StageCreation,
    },
    values::{ChannelIdentityV2, NonceV2, ProviderIdentityV2, RequestIdV2, ValueError},
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, OnceLock};

const EPOCH: u32 = 2;
const SCHEMA: u32 = 1;
const STORE_DIR: &str = "discord_restart_reports_v2";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnboundRestartRequestInputV2 {
    provider: String,
    channel: String,
    nonce: String,
    requested_generation: u64,
}

impl UnboundRestartRequestInputV2 {
    pub(super) fn new(
        provider: String,
        channel: String,
        nonce: String,
        requested_generation: u64,
    ) -> Self {
        Self {
            provider,
            channel,
            nonce,
            requested_generation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UnboundRestartRequestDraftV2 {
    provider: ProviderIdentityV2,
    channel: ChannelIdentityV2,
    nonce: NonceV2,
    requested_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CanonicalRequestPathV2 {
    provider_component: String,
    channel_component: String,
    request_component: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnboundRestartRequestV2 {
    request_id: RequestIdV2,
    provider: ProviderIdentityV2,
    channel: ChannelIdentityV2,
    nonce: NonceV2,
    requested_generation: u64,
    raw: Box<[u8]>,
    path: CanonicalRequestPathV2,
}

impl UnboundRestartRequestV2 {
    pub(super) fn request_id(&self) -> &RequestIdV2 {
        &self.request_id
    }

    pub(super) fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub(super) fn path(&self) -> &CanonicalRequestPathV2 {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestPublicationErrorV2 {
    InvalidInput,
    MissingRuntimeRoot,
    Filesystem(FsErrorKind),
    PoisonedStore,
    PrelinkStopped,
}

impl From<FsError> for RequestPublicationErrorV2 {
    fn from(error: FsError) -> Self {
        Self::Filesystem(error.kind())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RequestPublicationDispositionV2 {
    Published {
        request: UnboundRestartRequestV2,
        raw: Box<[u8]>,
        maintenance: Option<fs::MaintenanceCleanup>,
    },
    OrdinaryFailure(RequestPublicationErrorV2),
    AlreadyPublishedCollision,
    Indeterminate(RequestPublicationErrorV2),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequestV2 {
    epoch: u32,
    schema: u32,
    request_id: String,
    provider_hex: String,
    channel_hex: String,
    nonce: String,
    requested_generation: u64,
}

struct StoreInner {
    root: ConfinedRuntimeRoot,
    poisoned: bool,
}

type SharedStore = Arc<Mutex<StoreInner>>;
static PRODUCTION_STORE: OnceLock<SharedStore> = OnceLock::new();
static PRODUCTION_STORE_INIT: Mutex<()> = Mutex::new(());

pub(super) fn publish_unbound_request_v2(
    input: UnboundRestartRequestInputV2,
) -> RequestPublicationDispositionV2 {
    let capability = match fs::issue_publication_capability() {
        Ok(capability) => capability,
        Err(error) => {
            return RequestPublicationDispositionV2::OrdinaryFailure(error.into());
        }
    };
    publish_supported(capability, input)
}

fn publish_supported(
    capability: PublicationCapabilityV2,
    input: UnboundRestartRequestInputV2,
) -> RequestPublicationDispositionV2 {
    let store = match request_store(capability) {
        Ok(store) => store,
        Err(error) => return RequestPublicationDispositionV2::OrdinaryFailure(error),
    };
    let mut store = match store.lock() {
        Ok(store) => store,
        Err(_) => {
            return RequestPublicationDispositionV2::OrdinaryFailure(
                RequestPublicationErrorV2::PoisonedStore,
            );
        }
    };
    if store.poisoned {
        return RequestPublicationDispositionV2::OrdinaryFailure(
            RequestPublicationErrorV2::PoisonedStore,
        );
    }
    match publish_locked(&mut store, input) {
        Ok(disposition) => disposition,
        Err(error) => RequestPublicationDispositionV2::OrdinaryFailure(error),
    }
}

fn publish_locked(
    store: &mut StoreInner,
    input: UnboundRestartRequestInputV2,
) -> Result<RequestPublicationDispositionV2, RequestPublicationErrorV2> {
    let draft = validate(input)?;
    let request_id = RequestIdV2::mint_v4();
    let provider_component = hex::encode(draft.provider.as_str().as_bytes());
    let channel_component = hex::encode(draft.channel.as_str().as_bytes());
    let request_component = request_id.as_str().to_owned();
    let path = CanonicalRequestPathV2 {
        provider_component,
        channel_component,
        request_component,
    };
    let raw = encode(&request_id, &draft, &path)?;

    let root_dir = store.root.root_dir();
    let mut session = store.root.mutation_session()?;
    let requests = child(&mut session, &root_dir, "requests")?;
    let provider = child(&mut session, &requests, &path.provider_component)?;
    let channel = child(&mut session, &provider, &path.channel_component)?;
    let _canonical_parent = child(&mut session, &channel, &path.request_component)?;
    let staging = child(&mut session, &root_dir, "staging")?;
    let stage_name = format!("{}.request.v2.json", request_id.as_str());
    let writer = match session.create_stage(&staging, &stage_name)? {
        StageCreation::Writer(writer) => writer,
        StageCreation::Collision(_) => {
            return Ok(RequestPublicationDispositionV2::AlreadyPublishedCollision);
        }
    };
    let _sealed = writer.seal(&raw)?;

    // Semantic RED: the final typestates and durable stage exist, but no canonical link is made.
    Ok(RequestPublicationDispositionV2::OrdinaryFailure(
        RequestPublicationErrorV2::PrelinkStopped,
    ))
}

fn child(
    session: &mut fs::MutationSession<'_>,
    parent: &ConfinedDir,
    component: &str,
) -> Result<ConfinedDir, RequestPublicationErrorV2> {
    session
        .open_or_create_child(parent, component)
        .into_result()
        .map_err(Into::into)
}

fn validate(
    input: UnboundRestartRequestInputV2,
) -> Result<UnboundRestartRequestDraftV2, RequestPublicationErrorV2> {
    Ok(UnboundRestartRequestDraftV2 {
        provider: ProviderIdentityV2::parse(&input.provider).map_err(invalid)?,
        channel: ChannelIdentityV2::parse(&input.channel).map_err(invalid)?,
        nonce: NonceV2::parse(&input.nonce).map_err(invalid)?,
        requested_generation: input.requested_generation,
    })
}

fn invalid(_: ValueError) -> RequestPublicationErrorV2 {
    RequestPublicationErrorV2::InvalidInput
}

fn encode(
    request_id: &RequestIdV2,
    draft: &UnboundRestartRequestDraftV2,
    path: &CanonicalRequestPathV2,
) -> Result<Vec<u8>, RequestPublicationErrorV2> {
    serde_json::to_vec(&WireRequestV2 {
        epoch: EPOCH,
        schema: SCHEMA,
        request_id: request_id.as_str().to_owned(),
        provider_hex: path.provider_component.clone(),
        channel_hex: path.channel_component.clone(),
        nonce: draft.nonce.as_str().to_owned(),
        requested_generation: draft.requested_generation,
    })
    .map_err(|_| RequestPublicationErrorV2::InvalidInput)
}

fn request_store(_: PublicationCapabilityV2) -> Result<SharedStore, RequestPublicationErrorV2> {
    #[cfg(test)]
    if let Some(store) = TEST_STORE.with(|slot| slot.borrow().clone()) {
        return Ok(store);
    }
    if let Some(store) = PRODUCTION_STORE.get() {
        return Ok(store.clone());
    }
    let _initialization = PRODUCTION_STORE_INIT
        .lock()
        .map_err(|_| RequestPublicationErrorV2::PoisonedStore)?;
    if let Some(store) = PRODUCTION_STORE.get() {
        return Ok(store.clone());
    }
    let root = super::super::super::runtime_store::runtime_root()
        .ok_or(RequestPublicationErrorV2::MissingRuntimeRoot)?
        .join(STORE_DIR);
    let store = Arc::new(Mutex::new(StoreInner {
        root: ConfinedRuntimeRoot::open(&root)?,
        poisoned: false,
    }));
    PRODUCTION_STORE
        .set(store.clone())
        .map_err(|_| RequestPublicationErrorV2::PoisonedStore)?;
    Ok(store)
}

#[cfg(test)]
thread_local! {
    static TEST_STORE: std::cell::RefCell<Option<SharedStore>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct TestStoreGuard;

#[cfg(test)]
impl Drop for TestStoreGuard {
    fn drop(&mut self) {
        TEST_STORE.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
fn install_test_store(root: ConfinedRuntimeRoot) -> TestStoreGuard {
    TEST_STORE.with(|slot| {
        assert!(slot.borrow().is_none(), "test store already installed");
        *slot.borrow_mut() = Some(Arc::new(Mutex::new(StoreInner {
            root,
            poisoned: false,
        })));
    });
    TestStoreGuard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn publish_mints_distinct_uuid_v4_and_survives_reload() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(&cwd).unwrap();
        let store_path = temp.path().join(STORE_DIR);
        fs::create_dir(&store_path).unwrap();
        let store_relative = store_path.strip_prefix(&cwd).unwrap();
        let _store = install_test_store(ConfinedRuntimeRoot::open(store_relative).unwrap());
        let input = || {
            UnboundRestartRequestInputV2::new(
                "Claude/β".to_owned(),
                "thread:001".to_owned(),
                "nonce-1".to_owned(),
                7,
            )
        };

        let first = publish_unbound_request_v2(input());
        let second = publish_unbound_request_v2(input());
        let published = match (&first, &second) {
            (
                RequestPublicationDispositionV2::Published {
                    request: left,
                    raw: left_raw,
                    ..
                },
                RequestPublicationDispositionV2::Published {
                    request: right,
                    raw: right_raw,
                    ..
                },
            ) => {
                let left_path = canonical_file(&store_path, left.path());
                let right_path = canonical_file(&store_path, right.path());
                left.request_id() != right.request_id()
                    && uuid::Uuid::parse_str(left.request_id().as_str())
                        .unwrap()
                        .get_version_num()
                        == 4
                    && uuid::Uuid::parse_str(right.request_id().as_str())
                        .unwrap()
                        .get_version_num()
                        == 4
                    && fs::read(left_path).ok().as_deref() == Some(&**left_raw)
                    && fs::read(right_path).ok().as_deref() == Some(&**right_raw)
            }
            _ => false,
        };
        assert!(
            published,
            "expected two Published results with distinct UUIDv4 IDs and exact canonical bytes; first={first:?}, second={second:?}"
        );
    }

    fn canonical_file(root: &std::path::Path, path: &CanonicalRequestPathV2) -> std::path::PathBuf {
        root.join("requests")
            .join(&path.provider_component)
            .join(&path.channel_component)
            .join(&path.request_component)
            .join("request.v2.json")
    }
}
