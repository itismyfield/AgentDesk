//! The durable grant owner is supplied separately from the socket and its checker.

use std::future::Future;
use std::pin::Pin;

use super::{HerdrCreateOutcome, HerdrLaunchEndpoint};
use crate::services::session_host::policy::permit::{CreatePermit, LaunchAdmissionGrant};

pub(crate) type PermitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CreatePermit, String>> + Send + 'a>>;

pub(crate) struct UserDefaultCreateRequest {
    pub endpoint: HerdrLaunchEndpoint,
    pub label: String,
    pub cwd: std::path::PathBuf,
    pub command: String,
    pub permit: CreatePermit,
}

#[async_trait::async_trait]
pub(crate) trait LaunchGrantStore: Send + Sync {
    async fn load(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<LaunchAdmissionGrant, String>;
    /// Success acknowledges durable recording of this exact grant; failures grant no authority.
    async fn save(&self, grant: LaunchAdmissionGrant) -> Result<(), String>;
}

pub(crate) struct UnintroducedGrantStore;
#[async_trait::async_trait]
impl LaunchGrantStore for UnintroducedGrantStore {
    async fn load(&self, _: &HerdrLaunchEndpoint, _: &str) -> Result<LaunchAdmissionGrant, String> {
        Err("launch admission grant store is not introduced".into())
    }
    async fn save(&self, _: LaunchAdmissionGrant) -> Result<(), String> {
        Err("launch admission grant store is not introduced".into())
    }
}
pub(crate) static UNINTRODUCED: UnintroducedGrantStore = UnintroducedGrantStore;

pub(crate) fn unsupported_create() -> HerdrCreateOutcome {
    HerdrCreateOutcome::NotSent("user-default create adapter unavailable".into())
}
