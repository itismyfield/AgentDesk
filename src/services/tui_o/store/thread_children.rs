//! Child init ordering, prepared behind the compiled capability until complete activation.

use std::fs::{self, File};
use std::io;

use super::{InitOrigin, Initialized, OStore, StoreError, durable};
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::session_host::surface_policy::InitScope;
use crate::services::session_host::surface_policy::VerifiedSurface;

const FLOOR: &str = "thread_children.floor";
const CONTENT: &[u8] = b"1\n";

impl OStore {
    pub(super) fn init_channel_checked(
        &self,
        init: &Initialized,
        origin: InitOrigin,
        scope: &InitScope,
        verified: Option<&VerifiedSurface>,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let outside = !scope.configured(init.channel);
        let reject = |why: &str| StoreError::Rejected(why.into());
        if outside {
            let parent = init
                .policy_parent
                .ok_or_else(|| reject("unconfigured channel requires policy_parent"))?;
            if !enabled
                || !verified.is_some_and(|surface| {
                    surface.matches(init.channel, parent, scope.kind(parent))
                })
            {
                return Err(reject(
                    "child output requires verified surface and complete activation",
                ));
            }
            self.publish_child_floor()?;
        } else if init.policy_parent.is_some() {
            return Err(reject(
                "policy_parent requires an unconfigured verified child",
            ));
        }
        self.publish_init(init, origin)
    }

    fn publish_child_floor(&self) -> io::Result<()> {
        let path = self.root.join(FLOOR);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = durable::create_once(&path, CONTENT)
                    && error.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if !fs::symlink_metadata(&path)?.is_file() || fs::read(&path)? != CONTENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child floor is not a regular schema-1 marker",
            ));
        }
        // A failed init never lowers the durable deployment floor.
        File::open(self.root.join(FLOOR))?.sync_all()?;
        fsync_parent_dir(&path)
    }
}

#[cfg(test)]
mod retirement_tests;

#[cfg(test)]
impl OStore {
    fn fixture_scope(channels: &[u64]) -> std::sync::Arc<InitScope> {
        std::sync::Arc::new(InitScope::fixture(channels))
    }

    pub(crate) fn init_fixture_channel(&self, init: &Initialized) -> Result<(), StoreError> {
        self.init_channel(init, &Self::fixture_scope(&[init.channel]))
    }

    pub(crate) fn init_fixture_channel_as(
        &self,
        init: &Initialized,
        origin: InitOrigin,
    ) -> Result<(), StoreError> {
        self.init_channel_as(init, origin, &Self::fixture_scope(&[init.channel]))
    }

    pub(crate) fn begin_fixture_era_as(
        &self,
        init: &Initialized,
        origin: InitOrigin,
    ) -> Result<(), StoreError> {
        self.begin_era_as(init, origin, &Self::fixture_scope(&[init.channel]))
    }

    pub(crate) fn begin_fixture_era(
        &self,
        channels: &[u64],
        at: chrono::DateTime<chrono::Utc>,
        init_for: impl FnMut(u64) -> Result<Initialized, StoreError>,
    ) -> Result<super::OEra, StoreError> {
        self.begin_era(channels, at, &Self::fixture_scope(channels), init_for)
    }
}
