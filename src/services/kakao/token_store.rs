//! Opt-in credential persistence. Filesystem guarantees live in utils.
use super::KakaoError;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredTokens {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub generation: u64,
}
pub(super) struct TokenStore {
    #[cfg(unix)]
    dir: crate::utils::secret_file::private_directory::PrivateDirectory,
    #[cfg(unix)]
    filename: String,
    #[cfg(unix)]
    _lock: std::fs::File,
}
impl TokenStore {
    pub(super) fn from_process(account: &str) -> Result<Option<Self>, KakaoError> {
        let Some(dir) = std::env::var_os("AGENTDESK_KAKAO_TOKEN_STORE_DIR") else {
            return Ok(None);
        };
        Self::open(std::path::Path::new(&dir), account).map(Some)
    }
    #[cfg(unix)]
    fn open(path: &std::path::Path, account: &str) -> Result<Self, KakaoError> {
        super::validate_account_id(account)?;
        let dir = crate::utils::secret_file::private_directory::PrivateDirectory::open(path)
            .map_err(|_| KakaoError::CredentialPersistence)?;
        let lock = dir
            .lock(&format!("{account}.lock"))
            .map_err(|_| KakaoError::CredentialPersistence)?;
        Ok(Self {
            dir,
            filename: format!("{account}.json"),
            _lock: lock,
        })
    }
    #[cfg(not(unix))]
    fn open(_path: &std::path::Path, _account: &str) -> Result<Self, KakaoError> {
        Err(KakaoError::InvalidConfiguration(
            "durable Kakao credentials require Unix owner-only storage",
        ))
    }
    #[cfg(unix)]
    pub(super) fn load(&self) -> Result<Option<StoredTokens>, KakaoError> {
        self.dir
            .read(&self.filename)
            .map_err(|_| KakaoError::CredentialPersistence)?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| KakaoError::CredentialPersistence)
            })
            .transpose()
    }
    #[cfg(unix)]
    pub(super) fn save(&self, tokens: &StoredTokens) -> Result<(), KakaoError> {
        let bytes = serde_json::to_vec(tokens).map_err(|_| KakaoError::CredentialPersistence)?;
        self.dir.write_atomic(&self.filename,&bytes).map_err(|stage|{tracing::warn!(?stage,"Kakao credential persistence failed; new credentials retained and dispatch blocked");KakaoError::CredentialPersistence})
    }
    #[cfg(not(unix))]
    pub(super) fn load(&self) -> Result<Option<StoredTokens>, KakaoError> {
        Err(KakaoError::CredentialPersistence)
    }
    #[cfg(not(unix))]
    pub(super) fn save(&self, _tokens: &StoredTokens) -> Result<(), KakaoError> {
        Err(KakaoError::CredentialPersistence)
    }
    #[cfg(all(test, unix))]
    pub(super) fn for_test(path: &std::path::Path, account: &str) -> Self {
        // macOS temporary directories may be reached through the system /var symlink.
        Self::open(&path.canonicalize().unwrap(), account).unwrap()
    }
}
