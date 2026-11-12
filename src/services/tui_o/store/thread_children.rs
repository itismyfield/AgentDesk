//! Child init ordering, prepared behind the compiled capability until complete activation.

use std::fs::{self, File};
use std::io;

use super::{InitOrigin, Initialized, OStore, StoreError, durable};
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::session_host::surface_policy::VerifiedSurface;
use crate::services::tui_o::channel_policy::BootChannels;

pub(crate) const OUTPUT_ENABLED: bool = false;
const FLOOR: &str = "thread_children.floor";
const CONTENT: &[u8] = b"1\n";

impl OStore {
    pub(super) fn init_channel_checked(
        &self,
        init: &Initialized,
        origin: InitOrigin,
        boot: Option<&BootChannels>,
        verified: Option<&VerifiedSurface>,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let outside = boot.is_some_and(|boot| !boot.configured_binding(init.channel));
        let reject = |why: &str| StoreError::Rejected(why.into());
        if outside {
            let parent = init
                .policy_parent
                .ok_or_else(|| reject("unconfigured channel requires policy_parent"))?;
            if !enabled
                || !verified.is_some_and(|surface| {
                    surface.matches(init.channel, parent, boot.and_then(|b| b.kind(parent)))
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
        File::open(&path)?.sync_all()?;
        fsync_parent_dir(&path)
    }
}

#[cfg(test)]
mod retirement;
