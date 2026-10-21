//! A preview is a compare-and-swap token; publishing never renames the logical symlink.

use super::{editor, file};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, fchown};
use std::path::{Path, PathBuf};

#[path = "apply_fs.rs"]
mod apply_fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Preview,
    Lock,
    Backup,
    BackupSync,
    Metadata,
    Checker,
    CandidateSync,
    Revalidate,
    Rename,
    Publish,
    ParentSync,
    PostCheck,
}
#[derive(Debug)]
pub(crate) struct ApplyFailure {
    pub stage: Stage,
    pub published: bool,
    pub backup: Option<PathBuf>,
    pub detail: String,
}
#[derive(Debug, Clone)]
enum Original {
    Existing(file::FileSnapshot),
    Missing(file::MissingToken),
}
#[derive(Debug, Clone)]
pub(crate) struct ApplyPlan {
    logical: PathBuf,
    target: PathBuf,
    parent: (u64, u64),
    original: Original,
    candidate: String,
}
/// Backups and the stable lock sidecar remain for explicit manual cleanup.
#[derive(Debug)]
pub(crate) struct Applied {
    pub logical: PathBuf,
    pub target: PathBuf,
    pub before_hash: Option<[u8; 32]>,
    pub after_hash: [u8; 32],
    pub backup: Option<PathBuf>,
    pub original_absent: bool,
}
/// The blocking apply owner supplies a full local checker, never a semantic-key-only check.
pub(crate) trait CandidateChecker {
    fn check(&self, path: &Path) -> Result<(), String>;
}
impl<F: Fn(&Path) -> Result<(), String>> CandidateChecker for F {
    fn check(&self, path: &Path) -> Result<(), String> {
        self(path)
    }
}
impl ApplyPlan {
    pub(crate) fn requires_write(&self) -> bool {
        match &self.original {
            Original::Existing(read) => read.bytes != self.candidate.as_bytes(),
            Original::Missing(_) => true,
        }
    }
    pub(crate) fn candidate(&self) -> &str {
        &self.candidate
    }
    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
    fn unchanged(&self) -> Result<(), String> {
        if file::directory_identity(self.target.parent().ok_or("target parent")?)
            .map_err(|e| format!("{e:?}"))?
            != self.parent
        {
            return Err("target parent changed".into());
        }
        match &self.original {
            Original::Existing(read) => read.token.unchanged(),
            Original::Missing(missing) => missing.unchanged(),
        }
        .map_err(|e| format!("preview changed: {e:?}"))
    }
}
fn failure(stage: Stage, detail: impl ToString) -> ApplyFailure {
    ApplyFailure {
        stage,
        detail: detail.to_string(),
        published: false,
        backup: None,
    }
}
pub(crate) fn preview(logical: &Path) -> Result<ApplyPlan, ApplyFailure> {
    let read = (|| {
        let (original, target, bytes) = match std::fs::symlink_metadata(logical) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let missing = file::MissingToken::read(logical).map_err(|e| format!("{e:?}"))?;
                (
                    Original::Missing(missing.clone()),
                    missing.target,
                    Vec::new(),
                )
            }
            Err(e) => return Err(e.to_string()),
            Ok(_) => {
                let read = file::read(logical).map_err(|e| format!("{e:?}"))?;
                (
                    Original::Existing(read.clone()),
                    read.token.resolved_path().into(),
                    read.bytes,
                )
            }
        };
        let raw = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let candidate = editor::candidate(raw)
            .map_err(|e| format!("manual config normalization required: {e:?}"))?;
        let parent = file::directory_identity(target.parent().ok_or("target parent")?)
            .map_err(|e| format!("{e:?}"))?;
        Ok(ApplyPlan {
            logical: logical.into(),
            target,
            parent,
            original,
            candidate,
        })
    })();
    read.map_err(|e: String| {
        failure(
            Stage::Preview,
            format!(
                "{}: session.resume_agents_on_restore: {e}",
                logical.display()
            ),
        )
    })
}
pub(crate) fn apply(
    plan: &ApplyPlan,
    checker: &dyn CandidateChecker,
) -> Result<Applied, ApplyFailure> {
    let mut stage = Stage::Lock;
    let mut published = false;
    let mut backup = None;
    let result = (|| {
        let parent = plan.target.parent().ok_or("target parent")?;
        let name = plan
            .target
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("target name")?;
        let directory = apply_fs::Directory::open(parent)?;
        let held = directory.file().metadata().map_err(|e| e.to_string())?;
        if (held.dev(), held.ino()) != plan.parent {
            return Err("target parent changed".into());
        }
        // A stable sidecar survives target replacement and serializes every alias of this target.
        let lock = directory.lock(name)?;
        let metadata = lock.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
            return Err("unsafe apply lock".into());
        }
        lock.lock().map_err(|e| e.to_string())?;
        plan.unchanged()?;
        if !plan.requires_write() {
            stage = Stage::Checker;
            checkpoint(stage)?;
            checker.check(&plan.logical)?;
            plan.unchanged()?;
            let Original::Existing(original) = &plan.original else {
                return Err("missing unchanged source".into());
            };
            return Ok(Applied {
                logical: plan.logical.clone(),
                target: plan.target.clone(),
                before_hash: Some(original.token.digest()),
                after_hash: original.token.digest(),
                backup: None,
                original_absent: false,
            });
        }
        if let Original::Existing(original) = &plan.original {
            stage = Stage::Backup;
            checkpoint(stage)?;
            let mut file = directory.unique(".agentdesk-original-")?;
            file.write_all(&original.bytes).map_err(|e| e.to_string())?;
            stage = Stage::BackupSync;
            sync(file.as_file(), stage)?;
            let path = file.keep();
            backup = Some(path);
            sync_parent(&directory.sync_anchor(), stage)?;
        }
        let mut temporary = directory.unique(".agentdesk-candidate-")?;
        temporary
            .write_all(plan.candidate.as_bytes())
            .map_err(|e| e.to_string())?;
        stage = Stage::Metadata;
        checkpoint(stage)?;
        let initial = temporary.as_file().metadata().map_err(|e| e.to_string())?;
        let (uid, gid, mode) = match &plan.original {
            Original::Existing(read) => read.token.ownership(),
            Original::Missing(_) => (initial.uid(), initial.gid(), 0o600),
        };
        fchown(temporary.as_file(), Some(uid), Some(gid)).map_err(|e| e.to_string())?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        let metadata = temporary.as_file().metadata().map_err(|e| e.to_string())?;
        if (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777) != (uid, gid, mode) {
            return Err("metadata preservation failed".into());
        }
        let token = file::read(temporary.path())
            .map_err(|e| format!("{e:?}"))?
            .token;
        if token.dev_ino() != (metadata.dev(), metadata.ino()) {
            return Err("candidate path left held parent".into());
        }
        stage = Stage::Checker;
        checkpoint(stage)?;
        checker.check(temporary.path())?;
        token
            .unchanged()
            .map_err(|e| format!("candidate changed: {e:?}"))?;
        stage = Stage::CandidateSync;
        sync(temporary.as_file(), stage)?;
        stage = Stage::Revalidate;
        checkpoint(stage)?;
        plan.unchanged()?;
        stage = Stage::Rename;
        checkpoint(stage)?;
        plan.unchanged()?;
        stage = Stage::Publish;
        checkpoint(stage)?;
        temporary.persist(name)?;
        published = true;
        stage = Stage::ParentSync;
        sync_parent(&directory.sync_anchor(), stage)?;
        stage = Stage::PostCheck;
        checkpoint(stage)?;
        let final_read = file::read(&plan.logical).map_err(|e| format!("{e:?}"))?;
        if file::directory_identity(parent).map_err(|e| format!("{e:?}"))? != plan.parent {
            return Err("published target parent changed".into());
        }
        let same_path = match &plan.original {
            Original::Existing(read) => read.token.same_path(&final_read.token),
            Original::Missing(missing) => missing.matches_path(&final_read.token),
        };
        if !same_path
            || final_read.token.digest() != token.digest()
            || final_read.token.dev_ino() != token.dev_ino()
            || final_read.token.ownership() != (uid, gid, mode)
            || final_read.bytes != plan.candidate.as_bytes()
            || !editor::is_false(&plan.candidate)
        {
            return Err("published config or symlink changed".into());
        }
        checker.check(&plan.logical)?;
        final_read
            .token
            .unchanged()
            .map_err(|e| format!("post-check changed: {e:?}"))?;
        if file::directory_identity(parent).map_err(|e| format!("{e:?}"))? != plan.parent {
            return Err("published target parent changed".into());
        }
        Ok(Applied {
            logical: plan.logical.clone(),
            target: plan.target.clone(),
            before_hash: match &plan.original {
                Original::Existing(read) => Some(read.token.digest()),
                Original::Missing(_) => None,
            },
            after_hash: token.digest(),
            backup: backup.clone(),
            original_absent: matches!(plan.original, Original::Missing(_)),
        })
    })();
    result.map_err(|detail: String| ApplyFailure {
        stage,
        published,
        backup,
        detail,
    })
}
fn sync(file: &File, stage: Stage) -> Result<(), String> {
    checkpoint(stage)?;
    file.sync_all().map_err(|e| e.to_string())
}
fn sync_parent(path: &Path, stage: Stage) -> Result<(), String> {
    checkpoint(stage)?;
    crate::services::discord::runtime_store::fsync_parent_dir(path).map_err(|e| e.to_string())
}
fn checkpoint(stage: Stage) -> Result<(), String> {
    #[cfg(test)]
    tests::run_hook(stage);
    #[cfg(test)]
    if FAULT.with(|fault| {
        let hit = fault.get() == Some(stage);
        if hit {
            fault.set(None);
        }
        hit
    }) {
        return Err(format!("injected {stage:?}"));
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<Option<Stage>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
