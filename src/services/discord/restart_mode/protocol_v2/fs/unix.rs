use super::{
    Attempt, BoundedRead, CleanupFacts, ConfinedDir, ConfinedRuntimeRoot, DirectoryIdentity,
    DirectoryLocator, DirectoryMutation, DirectoryTypeFact, FsError, FsOperation, InvalidRootFact,
    LineageSeal, MutationFacts, OpenAttemptFact, OpenDirectoryFact, PinnedStage,
    PlatformStageCreation, PreparedChild, PreparedRegular, PreparedSeal, PreparedStage,
    RegularFile, RootLineage, SealedLinkFacts, StageCleanup,
};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::Read as _,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
    sync::Arc,
};

pub(super) type DirHandle = OwnedFd;
pub(super) type FileHandle = OwnedFd;
pub(super) type MutationLock = OwnedFd;
pub(super) const MUTATION_SUPPORTED: bool = true;
pub(super) const REGULAR_READ_SUPPORTED: bool = true;
pub(super) const SEALED_STAGE_SUPPORTED: bool = true;

const OPEN_DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
const OPEN_REGULAR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
const CREATE_STAGE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
const CREATE_STAGE_MODE: libc::mode_t = 0o600;

struct RootPlan {
    anchor: &'static CStr,
    components: Vec<CString>,
}

pub(in super::super) fn open_runtime_root(path: &Path) -> Result<ConfinedRuntimeRoot, FsError> {
    let plan = parse_root(path)?;
    let (mut fd, anchor) = open_directory(libc::AT_FDCWD, plan.anchor)?;
    let mut root = anchor;
    let mut locator = DirectoryLocator::Anchor {
        component: plan.anchor.to_owned(),
    };
    for component in &plan.components {
        locator = DirectoryLocator::Child {
            parent: root.identity,
            component: component.clone(),
        };
        (fd, root) = open_directory(fd.as_raw_fd(), component)?;
    }
    let seal = Arc::new(LineageSeal);
    Ok(ConfinedRuntimeRoot {
        lineage: RootLineage {
            anchor: anchor.identity,
            root: root.identity,
        },
        directory: ConfinedDir {
            fd: Arc::new(fd),
            open: root,
            seal,
            locator,
        },
    })
}

pub(super) fn lock_mutation(root: &ConfinedRuntimeRoot) -> Result<MutationLock, FsError> {
    let (fd, opened) = open_directory(root.directory.fd.as_raw_fd(), c".")?;
    #[cfg(test)]
    TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        let keep = trace.len().saturating_sub(2);
        trace.truncate(keep);
    });
    if opened.identity != root.directory.open.identity {
        return Err(FsError::identity_mismatch(
            root.directory.open.identity,
            opened.identity,
        ));
    }
    // The nonblocking lock coordinates only cooperating writers sharing this trusted root inode;
    // a nested root inode is a separate mutation authority with its own independent OFD.
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(FsError::io(FsOperation::LockMutation, last_errno()));
    }
    Ok(fd)
}

fn parse_root(path: &Path) -> Result<RootPlan, FsError> {
    if path.as_os_str().is_empty() {
        return Err(FsError::invalid(InvalidRootFact::Empty));
    }
    let mut absolute = false;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => return Err(FsError::invalid(InvalidRootFact::ParentDir)),
            Component::Normal(value) => components.push(
                CString::new(value.as_bytes())
                    .map_err(|_| FsError::invalid(InvalidRootFact::Nul))?,
            ),
            Component::Prefix(_) => return Err(FsError::invalid(InvalidRootFact::Prefix)),
        }
    }
    Ok(RootPlan {
        anchor: if absolute { c"/" } else { c"." },
        components,
    })
}

fn open_directory(
    parent_fd: RawFd,
    component: &CStr,
) -> Result<(OwnedFd, OpenDirectoryFact), FsError> {
    let flags = OPEN_DIRECTORY_FLAGS;
    let call = (parent_fd, component, flags);
    // SAFETY: component is NUL-terminated and parent_fd is either AT_FDCWD or owned by caller.
    let result_fd = unsafe { libc::openat(call.0, call.1.as_ptr(), call.2) };
    if result_fd == -1 {
        let raw_errno = last_errno();
        #[cfg(test)]
        record_open(call, result_fd, Some(raw_errno), None);
        return Err(FsError::io(FsOperation::Open, raw_errno));
    }
    // SAFETY: openat returned a new descriptor; adoption precedes every fallible trace action.
    let fd = unsafe { OwnedFd::from_raw_fd(result_fd) };
    // SAFETY: fd is live and F_GETFD takes no variadic argument.
    let fd_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    let fd_flags_errno = (fd_flags == -1).then(last_errno);
    #[cfg(test)]
    record_open(call, result_fd, None, Some((fd_flags, fd_flags_errno)));
    if let Some(raw_errno) = fd_flags_errno {
        return Err(FsError::io(FsOperation::GetFdFlags, raw_errno));
    }
    if fd_flags & libc::FD_CLOEXEC == 0 {
        return Err(FsError::missing_close_on_exec(fd_flags));
    }

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    #[cfg(test)]
    let injected_failure: Option<()> = FSTAT_FAILURE.with(|failure| {
        let remaining = failure.get()?;
        failure.set(remaining.checked_sub(1));
        (remaining == 0).then_some(())
    });
    #[cfg(not(test))]
    let injected_failure: Option<()> = None;
    // SAFETY: fd is live and stat points to writable storage for one libc::stat.
    let result = injected_failure.map_or_else(
        || unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) },
        |_| -1,
    );
    if result == -1 {
        let raw_errno = injected_failure.map_or_else(last_errno, |_| libc::EIO);
        #[cfg(test)]
        record_fstat(fd.as_raw_fd(), result, Some(raw_errno), None);
        return Err(FsError::io(FsOperation::Fstat, raw_errno));
    }
    // SAFETY: a successful fstat initialized the complete value.
    let stat = unsafe { stat.assume_init() };
    let fact = directory_fact(&stat);
    #[cfg(test)]
    record_fstat(fd.as_raw_fd(), result, None, Some(fact));
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(FsError::not_directory(fact.file_type.mode));
    }
    Ok((fd, fact))
}

fn directory_fact(stat: &libc::stat) -> OpenDirectoryFact {
    OpenDirectoryFact {
        identity: DirectoryIdentity {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        },
        file_type: DirectoryTypeFact {
            mode: stat.st_mode as u32,
        },
    }
}

pub(super) fn open_regular(prepared: PreparedRegular) -> Result<RegularFile, FsError> {
    let PreparedRegular(parent, locator) = prepared;
    let DirectoryLocator::Child { component, .. } = &locator else {
        unreachable!("prepared regular locator must name a child")
    };
    let name = component.as_c_str();
    let parent_fd = parent.fd.as_raw_fd();
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_ptr = stat.as_mut_ptr();
    let (_, errno, observed) = mutation_call(
        FsOperation::Observe,
        (parent_fd, name, libc::AT_SYMLINK_NOFOLLOW),
        || unsafe {
            libc::fstatat(
                parent_fd,
                name.as_ptr(),
                stat_ptr,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        |_| {
            let fact = directory_fact(unsafe { &*stat_ptr });
            (fact, Some(fact))
        },
    );
    if let Some(raw) = errno {
        return Err(FsError::io(FsOperation::Observe, raw));
    }
    let observed = observed.expect("successful preflight has a fact");
    if !is_regular(observed) {
        return Err(FsError::not_regular(observed.file_type.mode));
    }
    #[cfg(test)]
    POST_OBSERVE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    let (_, errno, fd) = mutation_call(
        FsOperation::Open,
        (parent_fd, name, OPEN_REGULAR_FLAGS),
        || unsafe { libc::openat(parent_fd, name.as_ptr(), OPEN_REGULAR_FLAGS) },
        |raw_fd| (unsafe { OwnedFd::from_raw_fd(raw_fd) }, None),
    );
    let Some(fd) = fd else {
        return Err(FsError::io(
            FsOperation::Open,
            errno.expect("failed open has errno"),
        ));
    };
    let opened = verify_regular_fd(&fd)?;
    if observed.identity != opened.identity {
        return Err(FsError::identity_mismatch(
            observed.identity,
            opened.identity,
        ));
    }
    Ok(RegularFile {
        fd,
        open: opened,
        locator,
    })
}

fn verify_regular_fd(fd: &OwnedFd) -> Result<OpenDirectoryFact, FsError> {
    let (flags, errno, _) = mutation_call(
        FsOperation::GetFdFlags,
        (fd.as_raw_fd(), c"", libc::F_GETFD),
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) },
        |flags| (flags, None),
    );
    if let Some(raw) = errno {
        return Err(FsError::io(FsOperation::GetFdFlags, raw));
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(FsError::missing_close_on_exec(flags));
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_ptr = stat.as_mut_ptr();
    let (_, errno, opened) = mutation_call(
        FsOperation::Fstat,
        (fd.as_raw_fd(), c"", 0),
        || unsafe { libc::fstat(fd.as_raw_fd(), stat_ptr) },
        |_| {
            #[cfg(test)]
            let injected = INJECTED_FSTAT_FACT.with(std::cell::Cell::take);
            #[cfg(not(test))]
            let injected = None;
            let fact = injected.unwrap_or_else(|| directory_fact(unsafe { &*stat_ptr }));
            (fact, Some(fact))
        },
    );
    if let Some(raw) = errno {
        return Err(FsError::io(FsOperation::Fstat, raw));
    }
    let opened = opened.expect("successful fstat has a fact");
    if !is_regular(opened) {
        return Err(FsError::not_regular(opened.file_type.mode));
    }
    Ok(opened)
}

pub(super) fn read_bounded(fd: FileHandle, limit: usize) -> Result<BoundedRead, FsError> {
    let ceiling = limit
        .checked_add(1)
        .ok_or_else(|| FsError::read_limit_overflow(limit))?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(ceiling as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            FsError::io(FsOperation::Read, error.raw_os_error().unwrap_or(libc::EIO))
        })?;
    Ok(if bytes.len() > limit {
        BoundedRead::Oversize
    } else {
        BoundedRead::Complete(bytes)
    })
}

pub(super) fn create_stage(prepared: PreparedStage) -> Result<PlatformStageCreation, FsError> {
    let PreparedStage(parent, locator) = prepared;
    let name = child_component(&locator);
    let parent_fd = parent.fd.as_raw_fd();
    let (_, errno, fd) = mutation_call(
        FsOperation::Open,
        (parent_fd, name, CREATE_STAGE_FLAGS),
        || unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                CREATE_STAGE_FLAGS,
                CREATE_STAGE_MODE as libc::c_int,
            )
        },
        |raw_fd| (unsafe { OwnedFd::from_raw_fd(raw_fd) }, None),
    );
    if let Some(fd) = fd {
        let (_, errno, _) = mutation_call(
            FsOperation::Chmod,
            (fd.as_raw_fd(), c"", CREATE_STAGE_MODE as i32),
            || unsafe { libc::fchmod(fd.as_raw_fd(), CREATE_STAGE_MODE) },
            |_| ((), None),
        );
        if let Some(raw) = errno {
            return Err(FsError::io(FsOperation::Chmod, raw));
        }
        let opened = verify_regular_fd(&fd)?;
        let mode = opened.file_type.mode & 0o7777;
        if mode != CREATE_STAGE_MODE as u32 {
            return Err(FsError::mode_mismatch(CREATE_STAGE_MODE as u32, mode));
        }
        return Ok(PlatformStageCreation::Writer(fd, opened));
    }
    let raw = errno.expect("failed stage create has errno");
    if raw == libc::EEXIST {
        let incumbent = open_regular(PreparedRegular(parent, locator))?;
        return Ok(PlatformStageCreation::Collision(
            incumbent.fd,
            incumbent.open,
        ));
    }
    Err(FsError::io(FsOperation::Open, raw))
}

pub(super) fn seal_stage(prepared: PreparedSeal, bytes: &[u8]) -> Result<PinnedStage, FsError> {
    let PreparedSeal {
        parent,
        locator,
        writer,
        writer_open,
    } = prepared;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let mut offered = (bytes.len() - offset).min(i32::MAX as usize);
        #[cfg(test)]
        STAGE_WRITE_LIMIT.with(|limit| {
            if let Some(injected) = limit.replace(None) {
                offered = offered.min(injected.max(1));
            }
        });
        let (written, errno, _) = mutation_call(
            FsOperation::Write,
            (writer.as_raw_fd(), c"", offered as i32),
            || unsafe {
                libc::write(
                    writer.as_raw_fd(),
                    bytes[offset..offset + offered].as_ptr().cast(),
                    offered,
                ) as i32
            },
            |written| (written as usize, None),
        );
        if errno == Some(libc::EINTR) {
            continue;
        }
        if let Some(raw) = errno {
            return Err(FsError::io(FsOperation::Write, raw));
        }
        if written <= 0 || written as usize > offered {
            return Err(FsError::io(FsOperation::Write, libc::EIO));
        }
        offset += written as usize;
    }
    let (_, errno, _) = mutation_call(
        FsOperation::SyncFile,
        (writer.as_raw_fd(), c"", 0),
        || fsync_fd(writer.as_raw_fd()),
        |_| ((), None),
    );
    if let Some(raw) = errno {
        return Err(FsError::io(FsOperation::SyncFile, raw));
    }
    #[cfg(test)]
    POST_STAGE_SYNC.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let pinned = open_regular(PreparedRegular(parent, locator))?;
    if writer_open.identity != pinned.open.identity {
        return Err(FsError::identity_mismatch(
            writer_open.identity,
            pinned.open.identity,
        ));
    }
    drop(writer);
    Ok(PinnedStage(pinned.fd, pinned.open))
}

pub(super) fn link_stage(
    stage_parent: &ConfinedDir,
    stage_locator: &DirectoryLocator,
    target_parent: &ConfinedDir,
    target_locator: &DirectoryLocator,
    sealed: DirectoryIdentity,
) -> Result<SealedLinkFacts, FsError> {
    let stage_name = child_component(stage_locator);
    let target_name = child_component(target_locator);
    let stage_fd = stage_parent.fd.as_raw_fd();
    let target_fd = target_parent.fd.as_raw_fd();
    #[cfg(test)]
    let injected_report = LINK_REPORTED_ERRNO.with(std::cell::Cell::take);
    let result = unsafe {
        libc::linkat(
            stage_fd,
            stage_name.as_ptr(),
            target_fd,
            target_name.as_ptr(),
            0,
        )
    };
    let actual_errno = (result == -1).then(last_errno);
    #[cfg(test)]
    let reported_errno = actual_errno.or(injected_report);
    #[cfg(not(test))]
    let reported_errno = actual_errno;
    #[cfg(test)]
    record_link(
        (stage_fd, stage_name, target_fd, target_name, 0),
        result,
        reported_errno,
    );
    let link = reported_errno.map_or(Attempt::Succeeded(()), |raw_errno| {
        Attempt::Failed(super::IoFact {
            operation: FsOperation::Link,
            raw_errno,
        })
    });
    let target = observe_optional(target_fd, target_name);
    let parent_sync = sync_parent(target_fd);
    Ok(SealedLinkFacts {
        sealed,
        link,
        target,
        parent_sync,
    })
}

pub(super) fn cleanup_stage(
    stage_parent: &ConfinedDir,
    stage_locator: &DirectoryLocator,
    sealed: OpenDirectoryFact,
) -> StageCleanup {
    let name = child_component(stage_locator);
    let parent_fd = stage_parent.fd.as_raw_fd();
    match observe_optional(parent_fd, name) {
        Attempt::Succeeded(Some(observed)) if !is_regular(observed) => {
            return StageCleanup::Rejected(FsError::not_regular(observed.file_type.mode));
        }
        Attempt::Succeeded(Some(observed)) if observed.identity != sealed.identity => {
            return StageCleanup::Rejected(FsError::identity_mismatch(
                sealed.identity,
                observed.identity,
            ));
        }
        Attempt::Succeeded(Some(_)) => {}
        Attempt::Succeeded(None) => {
            return StageCleanup::Rejected(FsError::io(FsOperation::Observe, libc::ENOENT));
        }
        Attempt::Failed(io) => {
            return StageCleanup::Rejected(FsError::io(io.operation, io.raw_errno));
        }
        Attempt::NotAttempted => unreachable!("cleanup observation was not attempted"),
    }
    let (_, errno, removed) = mutation_call(
        FsOperation::Unlink,
        (parent_fd, name, 0),
        || unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) },
        |_| ((), None),
    );
    let unlink = attempt(FsOperation::Unlink, errno, removed);
    #[cfg(test)]
    POST_UNLINK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let observe = observe_optional(parent_fd, name);
    let parent_sync = sync_parent(parent_fd);
    StageCleanup::Attempted(CleanupFacts {
        unlink,
        observe,
        parent_sync,
    })
}

fn observe_optional(parent_fd: RawFd, name: &CStr) -> Attempt<Option<OpenDirectoryFact>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_ptr = stat.as_mut_ptr();
    let (_, errno, observed) = mutation_call(
        FsOperation::Observe,
        (parent_fd, name, libc::AT_SYMLINK_NOFOLLOW),
        || unsafe {
            libc::fstatat(
                parent_fd,
                name.as_ptr(),
                stat_ptr,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        |_| {
            let fact = directory_fact(unsafe { &*stat_ptr });
            (fact, Some(fact))
        },
    );
    match errno {
        Some(libc::ENOENT) => Attempt::Succeeded(None),
        Some(raw_errno) => Attempt::Failed(super::IoFact {
            operation: FsOperation::Observe,
            raw_errno,
        }),
        None => Attempt::Succeeded(Some(observed.expect("successful observation fact"))),
    }
}

fn sync_parent(parent_fd: RawFd) -> Attempt<()> {
    let (_, errno, synced) = mutation_call(
        FsOperation::SyncParent,
        (parent_fd, c"", 0),
        || fsync_fd(parent_fd),
        |_| ((), None),
    );
    attempt(FsOperation::SyncParent, errno, synced)
}

fn fsync_fd(fd: RawFd) -> libc::c_int {
    #[cfg(test)]
    ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().push(fd));
    unsafe { libc::fsync(fd) }
}

fn child_component(locator: &DirectoryLocator) -> &CStr {
    let DirectoryLocator::Child { component, .. } = locator else {
        unreachable!("descriptor-relative operation requires a child locator")
    };
    component.as_c_str()
}

const CREATE_MODE: libc::mode_t = 0o700;

pub(super) fn open_or_create_child(prepared: PreparedChild) -> DirectoryMutation {
    let PreparedChild(parent, locator) = prepared;
    let DirectoryLocator::Child { component, .. } = &locator else {
        unreachable!("prepared mutation locator must name a child")
    };
    let name = component.as_c_str();
    let parent_fd = parent.fd.as_raw_fd();
    let mut facts = MutationFacts {
        locator: locator.clone(),
        requested_mode: CREATE_MODE as u32,
        mkdir: Attempt::NotAttempted,
        sync: Attempt::NotAttempted,
        observe: Attempt::NotAttempted,
        open: Attempt::NotAttempted,
        fd_flags: Attempt::NotAttempted,
        fstat: Attempt::NotAttempted,
    };
    let mut report = None;

    let (_, errno, created) = mutation_call(
        FsOperation::Mkdir,
        (parent_fd, name, CREATE_MODE as i32),
        || unsafe { libc::mkdirat(parent_fd, name.as_ptr(), CREATE_MODE) },
        |_| ((), None),
    );
    facts.mkdir = attempt(FsOperation::Mkdir, errno, created);
    if let Some(raw) = errno.filter(|raw| *raw != libc::EEXIST) {
        report = Some(FsError::io(FsOperation::Mkdir, raw));
    }
    if errno.is_none() {
        let (_, errno, synced) = mutation_call(
            FsOperation::SyncParent,
            (parent_fd, c"", 0),
            || unsafe { libc::fsync(parent_fd) },
            |_| ((), None),
        );
        facts.sync = attempt(FsOperation::SyncParent, errno, synced);
        if let Some(raw) = errno {
            report.get_or_insert(FsError::io(FsOperation::SyncParent, raw));
        }
    }

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_ptr = stat.as_mut_ptr();
    let (_, errno, observed) = mutation_call(
        FsOperation::Observe,
        (parent_fd, name, libc::AT_SYMLINK_NOFOLLOW),
        || unsafe {
            libc::fstatat(
                parent_fd,
                name.as_ptr(),
                stat_ptr,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        |_| {
            let fact = directory_fact(unsafe { &*stat_ptr });
            (fact, Some(fact))
        },
    );
    facts.observe = attempt(FsOperation::Observe, errno, observed);
    if let Some(raw) = errno {
        report.get_or_insert(FsError::io(FsOperation::Observe, raw));
    }
    if let Some(fact) = observed.filter(|fact| !is_directory(*fact)) {
        report.get_or_insert(FsError::not_directory(fact.file_type.mode));
    }
    #[cfg(test)]
    POST_OBSERVE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    let (raw_fd, errno, fd) = mutation_call(
        FsOperation::Open,
        (parent_fd, name, OPEN_DIRECTORY_FLAGS),
        || unsafe { libc::openat(parent_fd, name.as_ptr(), OPEN_DIRECTORY_FLAGS) },
        |raw_fd| (unsafe { OwnedFd::from_raw_fd(raw_fd) }, None),
    );
    facts.open = attempt(
        FsOperation::Open,
        errno,
        fd.as_ref().map(|_| OpenAttemptFact { raw_fd }),
    );
    let Some(fd) = fd else {
        report.get_or_insert(FsError::io(
            FsOperation::Open,
            errno.expect("failed open errno"),
        ));
        return DirectoryMutation::Attempted(facts, Err(report.expect("reportable open failure")));
    };

    let (result, errno, fd_flags) = mutation_call(
        FsOperation::GetFdFlags,
        (fd.as_raw_fd(), c"", libc::F_GETFD),
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) },
        |flags| (flags, None),
    );
    facts.fd_flags = attempt(FsOperation::GetFdFlags, errno, fd_flags);
    if let Some(raw) = errno {
        report.get_or_insert(FsError::io(FsOperation::GetFdFlags, raw));
    } else if result & libc::FD_CLOEXEC == 0 {
        report.get_or_insert(FsError::missing_close_on_exec(result));
    }

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_ptr = stat.as_mut_ptr();
    let (_, errno, opened) = mutation_call(
        FsOperation::Fstat,
        (fd.as_raw_fd(), c"", 0),
        || unsafe { libc::fstat(fd.as_raw_fd(), stat_ptr) },
        |_| {
            #[cfg(test)]
            let injected = INJECTED_FSTAT_FACT.with(std::cell::Cell::take);
            #[cfg(not(test))]
            let injected = None;
            let fact = injected.unwrap_or_else(|| directory_fact(unsafe { &*stat_ptr }));
            (fact, Some(fact))
        },
    );
    facts.fstat = attempt(FsOperation::Fstat, errno, opened);
    if let Some(raw) = errno {
        report.get_or_insert(FsError::io(FsOperation::Fstat, raw));
    }
    if let Some(fact) = opened.filter(|fact| !is_directory(*fact)) {
        report.get_or_insert(FsError::not_directory(fact.file_type.mode));
    }
    if let (Some(observed), Some(opened)) = (observed, opened)
        && observed.identity != opened.identity
    {
        report.get_or_insert(FsError::identity_mismatch(
            observed.identity,
            opened.identity,
        ));
    }

    let child = report.map_or_else(
        || {
            Ok(ConfinedDir {
                fd: Arc::new(fd),
                open: opened.expect("accepted child has opened identity"),
                seal: parent.seal.clone(),
                locator,
            })
        },
        Err,
    );
    DirectoryMutation::Attempted(facts, child)
}

fn attempt<T>(operation: FsOperation, errno: Option<i32>, value: Option<T>) -> Attempt<T> {
    match errno {
        Some(raw_errno) => Attempt::Failed(super::IoFact {
            operation,
            raw_errno,
        }),
        None => Attempt::Succeeded(value.expect("successful call has a fact")),
    }
}

fn is_directory(fact: OpenDirectoryFact) -> bool {
    fact.file_type.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
}

fn is_regular(fact: OpenDirectoryFact) -> bool {
    fact.file_type.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
}

fn mutation_call<T>(
    _operation: FsOperation,
    operands: (RawFd, &CStr, i32),
    call: impl FnOnce() -> i32,
    success: impl FnOnce(i32) -> (T, Option<OpenDirectoryFact>),
) -> (i32, Option<i32>, Option<T>) {
    #[cfg(test)]
    let injected = MUTATION_FAULT.with(|fault| {
        let (selected, result, errno) = fault.get()?;
        (selected == _operation
            && (errno.is_some()
                || _operation == FsOperation::GetFdFlags
                || (_operation == FsOperation::Fstat
                    && INJECTED_FSTAT_FACT.with(|fact| fact.get().is_some()))))
        .then(|| {
            fault.set(None);
            (result, errno)
        })
    });
    #[cfg(not(test))]
    let injected = None;
    let (result, errno) = injected.unwrap_or_else(|| {
        let result = call();
        (result, (result == -1).then(last_errno))
    });
    let (value, fact) = if errno.is_none() {
        let (value, fact) = success(result);
        (Some(value), fact)
    } else {
        (None, None)
    };
    #[cfg(test)]
    PANIC_AFTER_OPEN_ADOPTION.with(|fd| {
        if _operation == FsOperation::Open && fd.get() == Some(-1) {
            fd.set(Some(result));
            panic!("injected panic after open adoption");
        }
    });
    #[cfg(test)]
    {
        super::PREFLIGHT_ACTIVITY.with(|activity| activity.set(activity.get() | 12));
        let (fd, component, operand) = operands;
        MUTATION_TRACE.with(|trace| {
            trace.borrow_mut().push(MutationTrace(
                _operation,
                (fd, component.to_bytes().into(), operand),
                result,
                errno,
                fact,
            ))
        });
    }
    #[cfg(not(test))]
    let _ = (operands, fact);
    (result, errno, value)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
#[derive(Debug)]
enum TraceResult {
    Open(OpenResult),
    Fstat(FstatResult),
}

#[cfg(test)] #[rustfmt::skip] #[derive(Debug)]
pub(super) struct MutationTrace(FsOperation, (RawFd, Box<[u8]>, i32), i32, Option<i32>, Option<OpenDirectoryFact>);

#[cfg(test)]
#[derive(Debug)]
struct OpenResult {
    parent_fd: RawFd,
    component: Box<[u8]>,
    flags: libc::c_int,
    result_fd: RawFd,
    errno: Option<i32>,
    f_getfd_result: libc::c_int,
    f_getfd_errno: Option<i32>,
}

#[cfg(test)]
#[derive(Debug)]
struct FstatResult {
    fd: RawFd,
    result: libc::c_int,
    errno: Option<i32>,
    fact: Option<OpenDirectoryFact>,
}

#[cfg(test)]
thread_local! {
    static TRACE: std::cell::RefCell<Vec<TraceResult>> = const { std::cell::RefCell::new(Vec::new()) };
    static FSTAT_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static MUTATION_TRACE: std::cell::RefCell<Vec<MutationTrace>> = const { std::cell::RefCell::new(Vec::new()) };
    static MUTATION_FAULT: std::cell::Cell<Option<(FsOperation, i32, Option<i32>)>> = const { std::cell::Cell::new(None) };
    static INJECTED_FSTAT_FACT: std::cell::Cell<Option<OpenDirectoryFact>> = const { std::cell::Cell::new(None) };
    static PANIC_AFTER_OPEN_ADOPTION: std::cell::Cell<Option<RawFd>> = const { std::cell::Cell::new(None) };
    static POST_OBSERVE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static STAGE_WRITE_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static POST_STAGE_SYNC: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static LINK_REPORTED_ERRNO: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    static POST_UNLINK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static ACTUAL_FSYNC_FDS: std::cell::RefCell<Vec<RawFd>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(super) fn take_mutation_evidence() -> (u8, Vec<MutationTrace>) {
    (
        super::take_preflight_activity(),
        MUTATION_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut())),
    )
}

#[cfg(test)]
fn record_open(
    (parent_fd, component, flags): (RawFd, &CStr, libc::c_int),
    result_fd: RawFd,
    errno: Option<i32>,
    f_getfd: Option<(libc::c_int, Option<i32>)>,
) {
    let (f_getfd_result, f_getfd_errno) = f_getfd.unwrap_or((-1, None));
    TRACE.with(|trace| {
        trace.borrow_mut().push(TraceResult::Open(OpenResult {
            parent_fd,
            component: component.to_bytes().into(),
            flags,
            result_fd,
            errno,
            f_getfd_result,
            f_getfd_errno,
        }));
    });
}

#[cfg(test)]
fn record_fstat(
    fd: RawFd,
    result: libc::c_int,
    errno: Option<i32>,
    fact: Option<OpenDirectoryFact>,
) {
    TRACE.with(|trace| {
        trace.borrow_mut().push(TraceResult::Fstat(FstatResult {
            fd,
            result,
            errno,
            fact,
        }));
    });
}

#[cfg(test)]
fn record_link(
    (stage_fd, stage_component, target_fd, target_component, flags): (
        RawFd,
        &CStr,
        RawFd,
        &CStr,
        libc::c_int,
    ),
    result: libc::c_int,
    errno: Option<i32>,
) {
    super::PREFLIGHT_ACTIVITY.with(|activity| activity.set(activity.get() | 12));
    MUTATION_TRACE.with(|trace| {
        trace.borrow_mut().push(MutationTrace(
            FsOperation::Link,
            (stage_fd, stage_component.to_bytes().into(), target_fd),
            if errno.is_some() { -1 } else { result },
            errno,
            None,
        ));
    });
    let _ = (target_component, flags);
}

#[cfg(test)]
mod high_risk_recovery {
    use super::*;
    use crate::services::discord::restart_mode::protocol_v2::fs::FsErrorKind;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

    const EXPECTED_OPEN_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    #[rustfmt::skip]
    const EXPECTED_REGULAR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    #[rustfmt::skip]
    const EXPECTED_STAGE_CREATE_FLAGS: libc::c_int = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    const MUTATION_LOCK_CHILD: &str = "AGENTDESK_5604_MUTATION_LOCK_CHILD";
    const SEALED_STAGE_TEST: &str = "services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::sealed_stage_link_records_identity_and_cleanup_stays_maintenance_only";
    const SYMLINK_ERRNO: i32 = libc::ENOTDIR;
    #[rustfmt::skip] fn stage_writer<'a, 'b>(session: &'a mut super::super::MutationSession<'b>, parent: &ConfinedDir, name: &str) -> super::super::StageWriter<'a, 'b> { match session.create_stage(parent, name) { Ok(super::super::StageCreation::Writer(writer)) => writer, Ok(super::super::StageCreation::Collision(_)) => panic!("unexpected collision for {name}"), Err(error) => panic!("stage create failed for {name}: {error:?}") } }
    #[rustfmt::skip] fn synced_stage<'a, 'b>(session: &'a mut super::super::MutationSession<'b>, parent: &ConfinedDir, name: &str, bytes: &[u8]) -> super::super::SyncedStage<'a, 'b> { stage_writer(session, parent, name).seal(bytes).unwrap() }
    #[rustfmt::skip] fn cleanup_facts(cleanup: StageCleanup) -> CleanupFacts { match cleanup { StageCleanup::Attempted(facts) => facts, StageCleanup::Rejected(error) => panic!("unexpected cleanup rejection: {error:?}") } }
    fn mutation_lock_child() -> bool {
        use std::io::{Read as _, Write as _};
        let Some(value) = std::env::var_os(MUTATION_LOCK_CHILD) else {
            return false;
        };
        let value = value.to_string_lossy();
        let Some((parent, path)) = value.split_once(':') else {
            return false;
        };
        if parent != unsafe { libc::getppid() }.to_string() {
            return false;
        }
        let mut root = open_runtime_root(Path::new(path)).unwrap();
        let error = match root.mutation_session() {
            Ok(_) => panic!("contended mutation lock was acquired"),
            Err(error) => error,
        };
        assert!(
            matches!(error.kind(), FsErrorKind::Io(fact) if fact.operation() == FsOperation::LockMutation && (fact.raw_errno() == libc::EWOULDBLOCK || fact.raw_errno() == libc::EAGAIN))
        );
        println!("AGENTDESK_5604_LOCK_CONTENDED");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
        let session = root.mutation_session().unwrap();
        println!("AGENTDESK_5604_LOCK_REACQUIRED");
        std::io::stdout().flush().unwrap();
        drop(session);
        true
    }
    #[rustfmt::skip]
    #[test]
    fn sealed_stage_link_records_identity_and_cleanup_stays_maintenance_only() {
        if mutation_lock_child() { return }
        use super::super::{LinkOutcome, MaintenanceCleanup, SealedLinkDisposition, StageCreation}; use FsOperation::*;
        let sealed_id = DirectoryIdentity { device: 5, inode: 7 }; let other_id = DirectoryIdentity { device: 5, inode: 11 }; let io = |operation, raw_errno| super::super::IoFact { operation, raw_errno };
        let regular = |identity| OpenDirectoryFact { identity, file_type: DirectoryTypeFact { mode: libc::S_IFREG as u32 | 0o600 } };
        let reports = [Attempt::Succeeded(()), Attempt::Failed(io(Link, libc::EEXIST)), Attempt::Failed(io(Link, libc::EIO))];
        let targets = [Attempt::Succeeded(Some(regular(sealed_id))), Attempt::Succeeded(Some(regular(other_id))), Attempt::Succeeded(None), Attempt::Failed(io(Observe, libc::EACCES))];
        let syncs = [Attempt::Succeeded(()), Attempt::Failed(io(SyncParent, libc::EIO))]; use SealedLinkDisposition::*;
        let expected = [LinkedNormally, Indeterminate, Indeterminate, Indeterminate, Indeterminate, Indeterminate, Indeterminate, Indeterminate, ObservedAfterReportedError, Indeterminate, CleanupEligible, CleanupEligible, Indeterminate, Indeterminate, Indeterminate, Indeterminate, ObservedAfterReportedError, Indeterminate, CleanupEligible, CleanupEligible, Indeterminate, Indeterminate, Indeterminate, Indeterminate];
        let mut row = 0; for link in reports { for target in targets { for parent_sync in syncs { let facts = SealedLinkFacts { sealed: sealed_id, link, target, parent_sync }; assert_eq!(super::super::reduce_sealed_link(facts), expected[row], "24-row reducer row {row}"); row += 1; } } } assert_eq!(row, 24);

        let cwd = std::env::current_dir().unwrap(); let temp = tempfile::tempdir_in(&cwd).unwrap(); let base = temp.path(); let stage_path = base.join("stage"); let target_path = base.join("target"); fs::create_dir_all(&stage_path).unwrap(); fs::create_dir_all(&target_path).unwrap();
        let relative = base.strip_prefix(&cwd).unwrap(); let mut root = open_runtime_root(relative).unwrap(); let other = open_runtime_root(relative).unwrap(); let parent = root.directory.clone(); let foreign = other.directory.clone();
        let mut setup = root.mutation_session().unwrap(); let (_, stage) = attempted(setup.open_or_create_child(&parent, "stage")); let (_, target) = attempted(setup.open_or_create_child(&parent, "target")); let stage = stage.unwrap(); let target = target.unwrap(); drop(setup); take_mutation_evidence(); ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().clear());
        let root_fd = root.directory.fd.as_raw_fd(); let root_identity = root.directory.open.identity; let mut holder = root.mutation_session().unwrap(); let lock_fd = holder.lock.as_raw_fd(); let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit(); assert_eq!(unsafe { libc::fstat(lock_fd, stat.as_mut_ptr()) }, 0); assert_ne!(lock_fd, root_fd); assert_ne!(unsafe { libc::fcntl(lock_fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0); assert_eq!(directory_fact(unsafe { &*stat.as_ptr() }).identity, root_identity); let held = stage_writer(&mut holder, &stage, "lock-live");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap()).args([SEALED_STAGE_TEST, "--exact", "--nocapture", "--test-threads=1"]).env(MUTATION_LOCK_CHILD, format!("{}:{}", std::process::id(), base.display())).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap(); let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap()); let mut line = String::new(); loop { line.clear(); assert_ne!(std::io::BufRead::read_line(&mut stdout, &mut line).unwrap(), 0); if line.contains("AGENTDESK_5604_LOCK_CONTENDED") { break } }
        drop(held); drop(holder); drop(child.stdin.take()); let mut rest = String::new(); std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap(); let status = child.wait().unwrap(); assert!(status.success() && rest.contains("AGENTDESK_5604_LOCK_REACQUIRED")); fs::remove_file(stage_path.join("lock-live")).unwrap(); assert_eq!((fsync_fd(-1), last_errno()), (-1, libc::EBADF)); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [-1]);
        let mut session = root.mutation_session().unwrap(); let old_umask = unsafe { libc::umask(0o777) }; let writer = stage_writer(&mut session, &stage, "main"); unsafe { libc::umask(old_umask) }; let writer_fd = writer.writer.as_raw_fd(); STAGE_WRITE_LIMIT.with(|limit| limit.set(Some(2))); let synced = writer.seal(b"abcdef").unwrap(); let sealed = synced.token.sealed; let sealed_fd = synced.token.sealed_fd.as_raw_fd();
        let (_, trace) = take_mutation_evidence(); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [writer_fd]); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Open, Chmod, GetFdFlags, Fstat, Write, Write, SyncFile, Observe, Open, GetFdFlags, Fstat]);
        assert_eq!((trace[0].1.0, &*trace[0].1.1, trace[0].1.2, trace[0].2), (stage.fd.as_raw_fd(), b"main".as_slice(), EXPECTED_STAGE_CREATE_FLAGS, writer_fd)); assert_eq!((trace[1].0, trace[1].1.0, trace[1].1.2), (Chmod, writer_fd, 0o600)); assert_eq!((trace[4].1.2, trace[4].2, trace[5].1.2, trace[5].2), (2, 2, 4, 4)); assert_closed(writer_fd); assert_ne!(unsafe { libc::fcntl(sealed_fd, libc::F_GETFD) }, -1); assert_eq!(fs::read(stage_path.join("main")).unwrap(), b"abcdef");
        let meta = fs::metadata(stage_path.join("main")).unwrap(); assert_eq!((meta.mode() & 0o777, sealed.identity), (0o600, DirectoryIdentity { device: meta.dev(), inode: meta.ino() })); assert!(matches!(&synced.token.locator, DirectoryLocator::Child { parent, component } if (*parent, component.as_bytes()) == (stage.open.identity, b"main")));

        take_mutation_evidence(); ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().clear()); let LinkOutcome::Durable(durable) = synced.link(&target, "canonical") else { panic!("different-parent link was not durable") }; let (_, trace) = take_mutation_evidence(); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [target.fd.as_raw_fd()]); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Link, Observe, SyncParent]); assert_eq!((trace[0].1.0, &*trace[0].1.1, trace[0].1.2), (stage.fd.as_raw_fd(), b"main".as_slice(), target.fd.as_raw_fd())); assert!(target_path.join("canonical").exists());
        assert_eq!(durable.evidence, LinkedNormally); assert!(matches!(durable.facts.link, Attempt::Succeeded(())) && matches!(durable.facts.target, Attempt::Succeeded(Some(fact)) if fact.identity == sealed.identity) && matches!(durable.facts.parent_sync, Attempt::Succeeded(())));
        take_mutation_evidence(); ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().clear()); let MaintenanceCleanup(cleanup) = durable.cleanup(super::super::post_verification_cleanup_grant()); let cleanup = cleanup_facts(cleanup); let (_, trace) = take_mutation_evidence(); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [stage.fd.as_raw_fd()]); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Observe, Unlink, Observe, SyncParent]); assert_eq!((cleanup.unlink, cleanup.observe, cleanup.parent_sync), (Attempt::Succeeded(()), Attempt::Succeeded(None), Attempt::Succeeded(()))); assert!(!stage_path.join("main").exists() && target_path.join("canonical").exists());

        take_mutation_evidence(); MUTATION_FAULT.with(|fault| fault.set(Some((GetFdFlags, 0, None)))); let Err(error) = session.create_stage(&stage, "writer-cloexec") else { panic!("writer without CLOEXEC was accepted") }; let (_, trace) = take_mutation_evidence(); assert_eq!(error.kind(), FsErrorKind::MissingCloseOnExec { fd_flags: 0 }); assert_closed(trace[0].2); fs::remove_file(stage_path.join("writer-cloexec")).unwrap();
        let mismatched = OpenDirectoryFact { identity: sealed_id, file_type: DirectoryTypeFact { mode: libc::S_IFREG as u32 | 0o644 } }; INJECTED_FSTAT_FACT.with(|fact| fact.set(Some(mismatched))); MUTATION_FAULT.with(|fault| fault.set(Some((Fstat, 0, None)))); let Err(error) = session.create_stage(&stage, "mode-mismatch") else { panic!("non-0600 stage was accepted") }; assert_eq!(error.kind(), FsErrorKind::ModeMismatch { expected: 0o600, actual: 0o644 }); fs::remove_file(stage_path.join("mode-mismatch")).unwrap();
        let writer = stage_writer(&mut session, &stage, "pin-cloexec"); let writer_fd = writer.writer.as_raw_fd(); take_mutation_evidence(); MUTATION_FAULT.with(|fault| fault.set(Some((GetFdFlags, 0, None)))); let Err(error) = writer.seal(b"x") else { panic!("pin without CLOEXEC was accepted") }; let (_, trace) = take_mutation_evidence(); assert_eq!(error.kind(), FsErrorKind::MissingCloseOnExec { fd_flags: 0 }); assert_closed(writer_fd); assert_closed(trace[3].2); fs::remove_file(stage_path.join("pin-cloexec")).unwrap();
        let writer = stage_writer(&mut session, &stage, "pin-nonregular"); let writer_fd = writer.writer.as_raw_fd(); let injected = OpenDirectoryFact { identity: writer.writer_open.identity, file_type: DirectoryTypeFact { mode: libc::S_IFIFO as u32 | 0o600 } }; take_mutation_evidence(); INJECTED_FSTAT_FACT.with(|fact| fact.set(Some(injected))); MUTATION_FAULT.with(|fault| fault.set(Some((Fstat, 0, None)))); let Err(error) = writer.seal(b"x") else { panic!("nonregular pin was accepted") }; let (_, trace) = take_mutation_evidence(); assert!(matches!(error.kind(), FsErrorKind::NotRegular(fact) if fact == injected.file_type)); assert_closed(writer_fd); assert_closed(trace[3].2); fs::remove_file(stage_path.join("pin-nonregular")).unwrap();
        let writer = stage_writer(&mut session, &stage, "sync-eio"); let writer_fd = writer.writer.as_raw_fd(); MUTATION_FAULT.with(|fault| fault.set(Some((SyncFile, -1, Some(libc::EIO))))); let Err(error) = writer.seal(b"x") else { panic!("failed file sync produced a sealed token") }; assert_eq!(error.kind(), FsErrorKind::Io(io(SyncFile, libc::EIO))); assert_closed(writer_fd); fs::remove_file(stage_path.join("sync-eio")).unwrap();

        take_mutation_evidence(); MUTATION_FAULT.with(|fault| fault.set(Some((Write, -1, Some(libc::EINTR))))); let alias_synced = synced_stage(&mut session, &stage, "alias", b"retry"); let (_, trace) = take_mutation_evidence(); let writes = trace.iter().filter(|row| row.0 == Write).collect::<Vec<_>>(); assert_eq!((writes.len(), writes[0].3, writes[1].3), (2, Some(libc::EINTR), None)); let mut case_alias = stage.clone(); case_alias.locator = DirectoryLocator::Child { parent: stage.open.identity, component: CString::new("STAGE").unwrap() };
        take_mutation_evidence(); let LinkOutcome::Prelink(alias) = alias_synced.link(&case_alias, "CANONICAL") else { panic!("directory alias was not pre-link") }; assert!(matches!(alias.error.kind(), FsErrorKind::DirectoryAlias { source, target } if source == target)); let (activity, trace) = take_mutation_evidence(); assert_eq!((activity, trace.is_empty()), (3, true));
        let alias_retired = stage_path.join("alias-retired"); fs::rename(stage_path.join("alias"), &alias_retired).unwrap(); fs::write(stage_path.join("alias"), b"retry").unwrap(); take_mutation_evidence(); assert!(matches!(alias.cleanup(), StageCleanup::Rejected(error) if matches!(error.kind(), FsErrorKind::IdentityMismatch { observed, opened } if observed != opened))); let (_, trace) = take_mutation_evidence(); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Observe]); fs::remove_file(stage_path.join("alias")).unwrap(); fs::remove_file(alias_retired).unwrap();

        let cross = synced_stage(&mut session, &stage, "cross", b"x"); take_mutation_evidence(); let LinkOutcome::Prelink(cross) = cross.link(&foreign, "..") else { panic!("cross-lineage link was not pre-link") }; assert_eq!(cross.error.kind(), FsErrorKind::CrossLineage); let (activity, trace) = take_mutation_evidence(); assert_eq!((activity, trace.is_empty()), (0, true)); assert!(matches!(cleanup_facts(cross.cleanup()).unlink, Attempt::Succeeded(())));
        fs::write(stage_path.join("collision"), b"old").unwrap(); let StageCreation::Collision(collision) = session.create_stage(&stage, "collision").unwrap() else { panic!("incumbent became a writer") }; assert!(collision.facts.is_none()); take_mutation_evidence(); MUTATION_FAULT.with(|fault| fault.set(Some((Unlink, -1, Some(libc::EACCES))))); let failed = cleanup_facts(collision.cleanup()); assert!(matches!(failed.unlink, Attempt::Failed(io) if io.raw_errno() == libc::EACCES) && matches!(failed.observe, Attempt::Succeeded(Some(_))) && matches!(failed.parent_sync, Attempt::Succeeded(()))); fs::remove_file(stage_path.join("collision")).unwrap();
        fs::write(stage_path.join("substituted"), b"old").unwrap(); let StageCreation::Collision(substituted) = session.create_stage(&stage, "substituted").unwrap() else { unreachable!() }; let sealed_before = substituted.token.sealed.identity; let source = stage_path.join("substituted"); let retired = stage_path.join("substituted-retired"); MUTATION_FAULT.with(|fault| fault.set(Some((Unlink, -1, Some(libc::EIO))))); POST_UNLINK.with(|hook| *hook.borrow_mut() = Some(Box::new(move || { fs::rename(&source, &retired).unwrap(); fs::write(&source, b"new").unwrap(); }))); let substituted = cleanup_facts(substituted.cleanup()); assert!(matches!(substituted.observe, Attempt::Succeeded(Some(fact)) if fact.identity != sealed_before)); fs::remove_file(stage_path.join("substituted")).unwrap(); fs::remove_file(stage_path.join("substituted-retired")).unwrap();
        std::os::unix::fs::symlink("missing", stage_path.join("special")).unwrap(); assert!(matches!(session.create_stage(&stage, "special"), Err(error) if matches!(error.kind(), FsErrorKind::NotRegular(_)))); fs::remove_file(stage_path.join("special")).unwrap();
        let nofollow = synced_stage(&mut session, &stage, "nofollow-source", b"sealed"); std::os::unix::fs::symlink("../stage/nofollow-source", target_path.join("nofollow-target")).unwrap(); let LinkOutcome::Collision(nofollow) = nofollow.link(&target, "nofollow-target") else { panic!("symlink collision was treated as durable") }; let facts = nofollow.facts.unwrap(); assert!(matches!(facts.link, Attempt::Failed(io) if io.raw_errno() == libc::EEXIST) && matches!(facts.target, Attempt::Succeeded(Some(fact)) if fact.file_type.mode & libc::S_IFMT as u32 == libc::S_IFLNK as u32)); assert!(matches!(cleanup_facts(nofollow.cleanup()).unlink, Attempt::Succeeded(()))); fs::remove_file(target_path.join("nofollow-target")).unwrap();

        let swap = synced_stage(&mut session, &stage, "swap", b"same"); let swap_sealed = swap.token.sealed.identity; let retired = stage_path.join("swap-retired"); fs::rename(stage_path.join("swap"), &retired).unwrap(); fs::write(stage_path.join("swap"), b"same").unwrap(); let LinkOutcome::Indeterminate(indeterminate) = swap.link(&target, "swap-target") else { panic!("substituted source gained authority") }; assert!(matches!(indeterminate.facts.target, Attempt::Succeeded(Some(fact)) if fact.identity != swap_sealed)); drop(indeterminate); assert!(stage_path.join("swap").exists() && target_path.join("swap-target").exists()); fs::remove_file(stage_path.join("swap")).unwrap(); fs::remove_file(target_path.join("swap-target")).unwrap(); fs::remove_file(retired).unwrap();
        fs::write(target_path.join("occupied"), b"canonical").unwrap(); let occupied = synced_stage(&mut session, &stage, "occupied-stage", b"stage"); LINK_REPORTED_ERRNO.with(|fault| fault.set(Some(libc::EIO))); take_mutation_evidence(); ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().clear()); let LinkOutcome::Collision(mut collision) = occupied.link(&target, "occupied") else { panic!("EEXIST row was not collision cleanup") }; let (_, trace) = take_mutation_evidence(); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [target.fd.as_raw_fd()]); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Link, Observe, SyncParent]); let facts = collision.facts.unwrap(); assert!(matches!(facts.link, Attempt::Failed(io) if io.raw_errno() == libc::EEXIST) && matches!(facts.target, Attempt::Succeeded(Some(fact)) if fact.identity != facts.sealed) && matches!(facts.parent_sync, Attempt::Succeeded(()))); assert_eq!(LINK_REPORTED_ERRNO.with(std::cell::Cell::get), None); collision.token.target.as_mut().unwrap().parent.open.identity = stage.open.identity; take_mutation_evidence(); assert!(matches!(collision.cleanup(), StageCleanup::Rejected(error) if matches!(error.kind(), FsErrorKind::DirectoryAlias { source, target } if source == target))); assert!(take_mutation_evidence().1.is_empty()); fs::remove_file(stage_path.join("occupied-stage")).unwrap(); assert_eq!(fs::read(target_path.join("occupied")).unwrap(), b"canonical");
        let no_leak = synced_stage(&mut session, &stage, "no-leak", b"next"); let LinkOutcome::Durable(no_leak) = no_leak.link(&target, "no-leak-target") else { panic!("reported fault leaked into next link") }; assert_eq!(no_leak.evidence, LinkedNormally); let MaintenanceCleanup(cleanup) = no_leak.cleanup(super::super::post_verification_cleanup_grant()); assert!(matches!(cleanup_facts(cleanup).unlink, Attempt::Succeeded(())));

        let lost = synced_stage(&mut session, &stage, "lost", b"durable"); let lost_identity = lost.token.sealed.identity; LINK_REPORTED_ERRNO.with(|fault| fault.set(Some(libc::EIO))); take_mutation_evidence(); ACTUAL_FSYNC_FDS.with(|fds| fds.borrow_mut().clear()); let LinkOutcome::Durable(lost) = lost.link(&target, "lost-target") else { panic!("lost reply did not recover durable authority") }; let (_, trace) = take_mutation_evidence(); assert_eq!(ACTUAL_FSYNC_FDS.with(|fds| std::mem::take(&mut *fds.borrow_mut())), [target.fd.as_raw_fd()]); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Link, Observe, SyncParent]); assert_eq!(lost.evidence, ObservedAfterReportedError); assert!(matches!(lost.facts.link, Attempt::Failed(io) if io.raw_errno() == libc::EIO) && matches!(lost.facts.target, Attempt::Succeeded(Some(fact)) if fact.identity == lost_identity) && matches!(lost.facts.parent_sync, Attempt::Succeeded(()))); POST_UNLINK.with(|hook| *hook.borrow_mut() = Some(Box::new(|| MUTATION_FAULT.with(|fault| fault.set(Some((Observe, -1, Some(libc::EIO)))))))); let MaintenanceCleanup(cleanup) = lost.cleanup(super::super::post_verification_cleanup_grant()); let cleanup = cleanup_facts(cleanup); assert!(matches!(cleanup.unlink, Attempt::Succeeded(())) && matches!(cleanup.observe, Attempt::Failed(io) if io.raw_errno() == libc::EIO) && matches!(cleanup.parent_sync, Attempt::Succeeded(()))); assert_eq!(fs::read(target_path.join("lost-target")).unwrap(), b"durable");
        let maintenance = synced_stage(&mut session, &stage, "maintenance", b"authority"); let LinkOutcome::Durable(maintenance) = maintenance.link(&target, "maintenance-target") else { panic!("maintenance fixture did not link") }; MUTATION_FAULT.with(|fault| fault.set(Some((SyncParent, -1, Some(libc::EIO))))); let MaintenanceCleanup(cleanup) = maintenance.cleanup(super::super::post_verification_cleanup_grant()); let cleanup = cleanup_facts(cleanup); assert!(matches!(cleanup.unlink, Attempt::Succeeded(())) && matches!(cleanup.observe, Attempt::Succeeded(None)) && matches!(cleanup.parent_sync, Attempt::Failed(io) if io.raw_errno() == libc::EIO)); assert_eq!(fs::read(target_path.join("maintenance-target")).unwrap(), b"authority");
        let unsynced = synced_stage(&mut session, &stage, "unsynced", b"x"); MUTATION_FAULT.with(|fault| fault.set(Some((SyncParent, -1, Some(libc::EIO))))); let LinkOutcome::Indeterminate(unsynced) = unsynced.link(&target, "unsynced-target") else { panic!("failed parent sync became durable") }; assert!(matches!(unsynced.facts.parent_sync, Attempt::Failed(io) if io.raw_errno() == libc::EIO)); drop(unsynced); assert!(stage_path.join("unsynced").exists() && target_path.join("unsynced-target").exists()); fs::remove_file(stage_path.join("unsynced")).unwrap(); fs::remove_file(target_path.join("unsynced-target")).unwrap();

        let replacement = stage_path.join("seal-replacement"); let retired = stage_path.join("seal-retired"); let writer = stage_writer(&mut session, &stage, "seal-replacement"); let writer_fd = writer.writer.as_raw_fd(); POST_STAGE_SYNC.with(|hook| *hook.borrow_mut() = Some(Box::new(move || { fs::rename(&replacement, &retired).unwrap(); fs::write(&replacement, b"same").unwrap(); }))); let Err(error) = writer.seal(b"same") else { panic!("writer/pin identity mismatch was accepted") }; assert!(matches!(error.kind(), FsErrorKind::IdentityMismatch { observed, opened } if observed != opened)); assert_closed(writer_fd); fs::remove_file(stage_path.join("seal-replacement")).unwrap(); fs::remove_file(stage_path.join("seal-retired")).unwrap();
        drop(session); drop((stage, target, foreign, other, root)); assert!(target_path.join("canonical").exists() && target_path.join("lost-target").exists());
    }
    #[rustfmt::skip] fn make_fifo(path: &Path) { let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()); let path = CString::new(bytes).unwrap(); assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0); }
    #[rustfmt::skip] fn assert_regular_trace(trace: &[MutationTrace], parent_fd: RawFd, name: &[u8]) -> RawFd { use FsOperation::*; assert_eq!(trace.len(), 4); let raw_fd = trace[1].2; assert!(raw_fd >= 0); let expected = [(Observe, parent_fd, name, libc::AT_SYMLINK_NOFOLLOW), (Open, parent_fd, name, EXPECTED_REGULAR_FLAGS), (GetFdFlags, raw_fd, b"".as_slice(), libc::F_GETFD), (Fstat, raw_fd, b"".as_slice(), 0)]; for (row, expected) in trace.iter().zip(expected) { assert_eq!((row.0, row.1.0, &*row.1.1, row.1.2), expected); } assert!(trace.iter().all(|row| row.3.is_none())); assert_eq!((trace[0].2, trace[3].2), (0, 0)); assert_ne!(trace[2].2 & libc::FD_CLOEXEC, 0); raw_fd }
    #[rustfmt::skip]
    #[test]
    fn regular_open_rejects_special_nodes_and_bounded_reads_stay_bounded() {
        const TEST_NAME: &str = "services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::regular_open_rejects_special_nodes_and_bounded_reads_stay_bounded";
        const CHILD: &str = "AGENTDESK_5605_M06_MSR_CHILD";
        const FIFO_READY: &str = "AGENTDESK_5605_M06_FIFO_READY";
        let ppid = unsafe { libc::getppid() }.to_string();
        // Non-hostile test discriminator: the exact tag plus actual ppid prevents accidental recursion/collision.
        let mode = std::env::var(CHILD).ok().and_then(|value| { let (mode, parent) = value.split_once(':')?; (matches!(mode, "fifo" | "full") && parent == ppid).then(|| mode.to_owned()) });
        if mode.is_none() {
            let run = |mode: &str| {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"]).env(CHILD, format!("{mode}:{}", std::process::id())).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
                let started = std::time::Instant::now(); let mut child = command.spawn().unwrap(); let deadline = started + std::time::Duration::from_secs(10); let mut timed_out = false;
                let output = loop { if child.try_wait().unwrap().is_some() { break child.wait_with_output().unwrap(); } if std::time::Instant::now() >= deadline { timed_out = true; let killed = child.kill(); let reaped = child.wait_with_output(); killed.unwrap(); break reaped.unwrap(); } std::thread::sleep(std::time::Duration::from_millis(20)); };
                let elapsed = started.elapsed(); let stdout = String::from_utf8_lossy(&output.stdout); let selected = stdout.matches("running 1 test").count(); let passed = stdout.matches("1 passed; 0 failed; 0 ignored; 0 measured;").count(); let markers = stdout.matches(FIFO_READY).count();
                assert_eq!(selected, 1); if mode == "fifo" { assert_eq!(markers, 1); if timed_out { assert!(elapsed >= std::time::Duration::from_millis(9_500) && elapsed < std::time::Duration::from_secs(12)); } }
                println!("AGENTDESK_5605_M06_EVIDENCE mode={mode} selected1={selected} pass1={passed} marker_count={markers} timed_out={timed_out} reaped=true elapsed_ms={}", elapsed.as_millis());
                assert!(!timed_out, "{mode} child remained blocked after readiness for {elapsed:?}"); assert_child(output);
            };
            run("fifo"); run("full"); return;
        }
        let fifo_only = mode.as_deref() == Some("fifo");
        use FsOperation::*; use std::io::Write as _;
        let cwd = std::env::current_dir().unwrap(); let temp = tempfile::tempdir_in(&cwd).unwrap(); let path = temp.path(); let nested_path = path.join("nested"); fs::create_dir(&nested_path).unwrap();
        if fifo_only { fs::write(nested_path.join("fifo-race"), b"regular").unwrap(); }
        let relative = path.strip_prefix(&cwd).unwrap(); let mut root = open_runtime_root(relative).unwrap(); let parent = root.directory.clone(); let mut session = root.mutation_session().unwrap(); let (_, nested) = attempted(session.open_or_create_child(&parent, "nested")); let nested = nested.unwrap(); drop(session); take_mutation_evidence();
        if fifo_only {
            let fifo_race = nested_path.join("fifo-race"); MUTATION_FAULT.with(|fault| fault.set(Some((GetFdFlags, libc::FD_CLOEXEC, None))));
            POST_OBSERVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || { fs::remove_file(&fifo_race).unwrap(); make_fifo(&fifo_race); print!("{FIFO_READY}\n"); std::io::stdout().flush().unwrap(); })));
            let error = root.open_regular(&nested, "fifo-race").unwrap_err(); let (activity, trace) = take_mutation_evidence(); let raw_fd = trace[1].2;
            assert!(raw_fd >= 0);
            assert!(matches!(error.kind(), FsErrorKind::NotRegular(fact) if fact.mode() & libc::S_IFMT as u32 == libc::S_IFIFO as u32));
            assert_eq!((activity, trace.iter().map(|row| row.0).collect::<Vec<_>>()), (15, vec![Observe, Open, GetFdFlags, Fstat])); assert!(trace.iter().all(|row| row.3.is_none()));
            assert_eq!((trace[0].1.0, &*trace[0].1.1, trace[0].1.2), (nested.fd.as_raw_fd(), b"fifo-race".as_slice(), libc::AT_SYMLINK_NOFOLLOW)); assert_eq!((trace[1].1.0, &*trace[1].1.1), (nested.fd.as_raw_fd(), b"fifo-race".as_slice()));
            assert_eq!((trace[2].1.0, &*trace[2].1.1, trace[2].1.2, trace[2].2), (raw_fd, b"".as_slice(), libc::F_GETFD, libc::FD_CLOEXEC)); assert_eq!((trace[3].1.0, trace[3].2), (raw_fd, 0));
            assert_eq!(trace[3].4.unwrap().file_type.mode & libc::S_IFMT as u32, libc::S_IFIFO as u32); assert_closed(raw_fd); return;
        }
        fs::write(nested_path.join("regular"), b"abc").unwrap(); fs::write(nested_path.join("grow"), b"a").unwrap(); fs::write(nested_path.join("overflow"), b"x").unwrap(); fs::create_dir(path.join("directory")).unwrap(); make_fifo(&path.join("fifo")); let _socket = std::os::unix::net::UnixListener::bind(path.join("socket")).unwrap(); std::os::unix::fs::symlink("nested/regular", path.join("link")).unwrap();
        let other = open_runtime_root(relative).unwrap(); let foreign = other.directory.clone();
        assert!(matches!(root.open_regular(&foreign, "..").unwrap_err().kind(), FsErrorKind::CrossLineage)); let (activity, trace) = take_mutation_evidence(); assert_eq!((activity, trace.is_empty()), (0, true));
        assert!(matches!(root.open_regular(&parent, "..").unwrap_err().kind(), FsErrorKind::InvalidComponent(super::super::InvalidComponentFact::Parent))); let (_, trace) = take_mutation_evidence(); assert!(trace.is_empty());
        for name in ["directory", "fifo", "socket", "link"] { let error = root.open_regular(&parent, name).unwrap_err(); assert!(matches!(error.kind(), FsErrorKind::NotRegular(_))); let (activity, trace) = take_mutation_evidence(); assert_eq!((activity, trace.len()), (15, 1)); assert_eq!((trace[0].0, trace[0].1.0, &*trace[0].1.1, trace[0].1.2), (Observe, parent.fd.as_raw_fd(), name.as_bytes(), libc::AT_SYMLINK_NOFOLLOW)); }
        let dev = open_runtime_root(Path::new("/dev")).unwrap(); assert!(matches!(dev.open_regular(&dev.directory, "null").unwrap_err().kind(), FsErrorKind::NotRegular(_))); let (_, trace) = take_mutation_evidence(); assert_eq!((trace.len(), trace[0].0, &*trace[0].1.1), (1, Observe, b"null".as_slice()));
        for (operation, result, errno, expected) in [(GetFdFlags, 0, None, FsErrorKind::MissingCloseOnExec { fd_flags: 0 }), (Fstat, -1, Some(libc::EIO), FsErrorKind::Io(super::super::IoFact { operation: Fstat, raw_errno: libc::EIO }))] { MUTATION_FAULT.with(|fault| fault.set(Some((operation, result, errno)))); let error = root.open_regular(&nested, "regular").unwrap_err(); assert_eq!(error.kind(), expected); let (_, trace) = take_mutation_evidence(); let raw_fd = trace[1].2; assert_eq!(trace.last().unwrap().0, operation); assert_closed(raw_fd); }
        let metadata = fs::metadata(nested_path.join("regular")).unwrap(); let injected = OpenDirectoryFact { identity: DirectoryIdentity { device: metadata.dev(), inode: metadata.ino() }, file_type: DirectoryTypeFact { mode: libc::S_IFIFO as u32 | 0o600 } }; INJECTED_FSTAT_FACT.with(|fact| fact.set(Some(injected))); MUTATION_FAULT.with(|fault| fault.set(Some((Fstat, 0, None)))); let error = root.open_regular(&nested, "regular").unwrap_err(); assert!(matches!(error.kind(), FsErrorKind::NotRegular(fact) if fact == injected.file_type)); let (_, trace) = take_mutation_evidence(); let raw_fd = assert_regular_trace(&trace, nested.fd.as_raw_fd(), b"regular"); assert_closed(raw_fd);
        PANIC_AFTER_OPEN_ADOPTION.with(|fd| fd.set(Some(-1))); let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| root.open_regular(&nested, "regular"))); let adopted = PANIC_AFTER_OPEN_ADOPTION.with(|fd| fd.replace(None)).unwrap(); assert!(panic.is_err() && adopted >= 0); let (_, trace) = take_mutation_evidence(); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), [Observe]); assert_closed(adopted);
        let swap = nested_path.join("swap"); let replacement = nested_path.join("replacement"); fs::write(&swap, b"old").unwrap(); fs::write(&replacement, b"new").unwrap(); POST_OBSERVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || fs::rename(replacement, swap).unwrap()))); let error = root.open_regular(&nested, "swap").unwrap_err(); assert!(matches!(error.kind(), FsErrorKind::IdentityMismatch { observed, opened } if observed != opened)); let (_, trace) = take_mutation_evidence(); let raw_fd = assert_regular_trace(&trace, nested.fd.as_raw_fd(), b"swap"); assert_closed(raw_fd);
        std::os::unix::fs::symlink("regular", nested_path.join("oracle-link")).unwrap(); let oracle_name = c"oracle-link"; let oracle = unsafe { libc::openat(nested.fd.as_raw_fd(), oracle_name.as_ptr(), EXPECTED_REGULAR_FLAGS) }; let final_symlink_errno = last_errno(); assert_eq!(oracle, -1);
        let link_race = nested_path.join("link-race"); fs::write(&link_race, b"regular").unwrap(); POST_OBSERVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || { fs::remove_file(&link_race).unwrap(); std::os::unix::fs::symlink("regular", &link_race).unwrap(); }))); assert_io(root.open_regular(&nested, "link-race").unwrap_err(), (Open, final_symlink_errno)); let (_, trace) = take_mutation_evidence(); assert_eq!((trace.len(), trace[1].0, trace[1].1.0, &*trace[1].1.1, trace[1].1.2, trace[1].3), (2, Open, nested.fd.as_raw_fd(), b"link-race".as_slice(), EXPECTED_REGULAR_FLAGS, Some(final_symlink_errno)));
        let grow = nested_path.join("grow"); POST_OBSERVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || std::fs::OpenOptions::new().append(true).open(grow).unwrap().write_all(&[b'z'; 64]).unwrap()))); let growing = root.open_regular(&nested, "grow").unwrap(); let (_, trace) = take_mutation_evidence(); assert_regular_trace(&trace, nested.fd.as_raw_fd(), b"grow"); let raw_fd = growing.fd.as_raw_fd(); let probe_raw = unsafe { libc::dup(raw_fd) }; assert!(probe_raw >= 0); let probe = unsafe { OwnedFd::from_raw_fd(probe_raw) }; assert_eq!(growing.read_bounded(3).unwrap(), super::super::BoundedRead::Oversize); assert_eq!(unsafe { libc::lseek(probe.as_raw_fd(), 0, libc::SEEK_CUR) }, 4); assert_closed(raw_fd); drop(probe);
        let overflow = root.open_regular(&nested, "overflow").unwrap(); take_mutation_evidence(); let raw_fd = overflow.fd.as_raw_fd(); assert_eq!(overflow.read_bounded(usize::MAX).unwrap_err().kind(), FsErrorKind::ReadLimitOverflow { limit: usize::MAX }); assert_closed(raw_fd);
        let survivor = root.open_regular(&nested, "regular").unwrap(); let (_, trace) = take_mutation_evidence(); let raw_fd = assert_regular_trace(&trace, nested.fd.as_raw_fd(), b"regular"); let metadata = fs::metadata(nested_path.join("regular")).unwrap(); assert_eq!(survivor.open.identity, DirectoryIdentity { device: metadata.dev(), inode: metadata.ino() }); assert!(matches!(&survivor.locator, DirectoryLocator::Child { parent, component } if (*parent, component.as_bytes()) == (nested.open.identity, b"regular")));
        drop((parent, foreign, nested, other, dev, root)); assert_ne!(unsafe { libc::fcntl(raw_fd, libc::F_GETFD) }, -1); assert_eq!(survivor.read_bounded(3).unwrap(), super::super::BoundedRead::Complete(b"abc".to_vec())); assert_closed(raw_fd);
    }
    #[rustfmt::skip] fn attempted(mutation: DirectoryMutation) -> (MutationFacts, Result<ConfinedDir, FsError>) { match mutation { DirectoryMutation::Attempted(facts, child) => (facts, child), DirectoryMutation::Rejected(error) => panic!("unexpected rejection: {error:?}") } }
    #[rustfmt::skip] fn traced<T>(trace: &[MutationTrace], operation: FsOperation, success: impl Fn(&MutationTrace) -> T) -> Attempt<T> { let Some(row) = trace.iter().find(|row| row.0 == operation) else { return Attempt::NotAttempted }; row.3.map_or_else(|| Attempt::Succeeded(success(row)), |raw_errno| Attempt::Failed(super::super::IoFact { operation, raw_errno })) }
    #[rustfmt::skip] fn assert_facts(facts: &MutationFacts, trace: &[MutationTrace]) {
        assert_eq!(facts.mkdir, traced(trace, FsOperation::Mkdir, |_| ())); assert_eq!(facts.sync, traced(trace, FsOperation::SyncParent, |_| ())); assert_eq!(facts.observe, traced(trace, FsOperation::Observe, |row| row.4.unwrap())); assert_eq!(facts.open, traced(trace, FsOperation::Open, |row| OpenAttemptFact { raw_fd: row.2 })); assert_eq!(facts.fd_flags, traced(trace, FsOperation::GetFdFlags, |row| row.2)); assert_eq!(facts.fstat, traced(trace, FsOperation::Fstat, |row| row.4.unwrap()));
    }
    #[rustfmt::skip] #[test] fn directory_mutation_records_sync_facts_and_owned_fds_outlive_parents() {
        const TEST_NAME: &str = "services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::directory_mutation_records_sync_facts_and_owned_fds_outlive_parents"; const CHILD: &str = "AGENTDESK_5606_MUTATION_CHILD"; let ppid = unsafe { libc::getppid() }.to_string();
        if std::env::var(CHILD).ok().as_deref() != Some(ppid.as_str()) {
            let output = std::process::Command::new(std::env::current_exe().unwrap()).args([TEST_NAME, "--exact", "--test-threads=1"]).env(CHILD, std::process::id().to_string()).output().unwrap(); assert_child(output); return;
        }
        use FsOperation::*;
        let cwd = std::env::current_dir().unwrap(); let temp = tempfile::tempdir_in(&cwd).unwrap(); let path = temp.path(); let relative = path.strip_prefix(&cwd).unwrap();
        fs::write(path.join("file"), b"x").unwrap(); std::os::unix::fs::symlink(path, path.join("link")).unwrap();
        let mut root = open_runtime_root(relative).unwrap(); let other = open_runtime_root(relative).unwrap();
        let parent = root.directory.clone(); let foreign = other.directory.clone(); let parent_fd = parent.fd.as_raw_fd();
        let mut session = root.mutation_session().unwrap(); let old_umask = unsafe { libc::umask(0o027) };
        fs::DirBuilder::new().mode(0o700).create(path.join("control")).unwrap(); fs::DirBuilder::new().mode(0o750).create(path.join("existing")).unwrap();
        take_mutation_evidence(); let (fresh, fresh_child) = attempted(session.open_or_create_child(&parent, "fresh")); unsafe { libc::umask(old_umask) };
        let fresh_child = fresh_child.unwrap(); let (activity, trace) = take_mutation_evidence(); assert_eq!(activity, 15); assert_facts(&fresh, &trace);
        let Attempt::Succeeded(OpenAttemptFact { raw_fd }) = fresh.open else { unreachable!() }; let Attempt::Succeeded(flags) = fresh.fd_flags else { unreachable!() };
        let expected = [(Mkdir, parent_fd, b"fresh".as_slice(), 0o700), (SyncParent, parent_fd, b"", 0), (Observe, parent_fd, b"fresh", libc::AT_SYMLINK_NOFOLLOW), (Open, parent_fd, b"fresh", OPEN_DIRECTORY_FLAGS), (GetFdFlags, raw_fd, b"", libc::F_GETFD), (Fstat, raw_fd, b"", 0)];
        for (row, expected) in trace.iter().zip(expected) { assert_eq!((row.0, row.1.0, &*row.1.1, row.1.2), expected); }
        assert_eq!(trace.iter().map(|row| row.2).collect::<Vec<_>>(), [0, 0, 0, raw_fd, flags, 0]); assert!(trace.iter().all(|row| row.3.is_none()));
        let Attempt::Succeeded(observed) = fresh.observe else { unreachable!() }; let Attempt::Succeeded(opened) = fresh.fstat else { unreachable!() };
        assert_eq!((fresh.requested_mode, observed.file_type.mode & 0o777, opened.identity, flags & libc::FD_CLOEXEC), (0o700, fs::metadata(path.join("control")).unwrap().mode() & 0o777, observed.identity, libc::FD_CLOEXEC)); assert_eq!(fresh.locator, fresh_child.locator);
        take_mutation_evidence(); let existing_mode = fs::metadata(path.join("existing")).unwrap().mode();
        let (exists, existing_child) = attempted(session.open_or_create_child(&parent, "existing")); let existing_child = existing_child.unwrap(); let (_, trace) = take_mutation_evidence(); assert_facts(&exists, &trace);
        assert!(matches!(exists.mkdir, Attempt::Failed(io) if io.raw_errno == libc::EEXIST) && matches!(exists.sync, Attempt::NotAttempted)); assert_eq!(fs::metadata(path.join("existing")).unwrap().mode(), existing_mode);
        for (name, operation, result, errno, order) in [
            ("existing", Mkdir, -1, Some(libc::EACCES), &[Mkdir, Observe, Open, GetFdFlags, Fstat][..]), ("missing", Mkdir, -1, Some(libc::EACCES), &[Mkdir, Observe, Open][..]), ("sync-fault", SyncParent, -1, Some(libc::EIO), &[Mkdir, SyncParent, Observe, Open, GetFdFlags, Fstat][..]), ("observe-fault", Observe, -1, Some(libc::EIO), &[Mkdir, SyncParent, Observe, Open, GetFdFlags, Fstat][..]),
            ("open-fault", Open, -1, Some(libc::EIO), &[Mkdir, SyncParent, Observe, Open][..]), ("flags-fault", GetFdFlags, -1, Some(libc::EIO), &[Mkdir, SyncParent, Observe, Open, GetFdFlags, Fstat][..]), ("no-cloexec", GetFdFlags, 0, None, &[Mkdir, SyncParent, Observe, Open, GetFdFlags, Fstat][..]), ("fstat-fault", Fstat, -1, Some(libc::EIO), &[Mkdir, SyncParent, Observe, Open, GetFdFlags, Fstat][..]),
        ] {
            MUTATION_FAULT.with(|fault| fault.set(Some((operation, result, errno)))); let (facts, child) = attempted(session.open_or_create_child(&parent, name));
            let expected_error = errno.map_or(FsErrorKind::MissingCloseOnExec { fd_flags: result }, |raw_errno| FsErrorKind::Io(super::super::IoFact { operation, raw_errno })); assert_eq!(child.unwrap_err().kind(), expected_error);
            let (activity, trace) = take_mutation_evidence(); assert_eq!(activity, 15); assert_eq!(trace.iter().map(|row| row.0).collect::<Vec<_>>(), order);
            let selected = trace.iter().find(|row| row.0 == operation).unwrap(); assert_eq!((selected.2, selected.3), (result, errno)); assert_facts(&facts, &trace);
            if let Attempt::Succeeded(open) = facts.open { assert_closed(open.raw_fd); }
        }
        fs::create_dir(path.join("opened-nondir")).unwrap(); let meta = fs::metadata(path.join("opened-nondir")).unwrap(); let non_dir = OpenDirectoryFact { identity: DirectoryIdentity { device: meta.dev(), inode: meta.ino() }, file_type: DirectoryTypeFact { mode: libc::S_IFREG as u32 | 0o600 } };
        INJECTED_FSTAT_FACT.with(|fact| fact.set(Some(non_dir))); MUTATION_FAULT.with(|fault| fault.set(Some((Fstat, 0, None)))); let (facts, child) = attempted(session.open_or_create_child(&parent, "opened-nondir")); assert!(matches!(child.unwrap_err().kind(), FsErrorKind::NotDirectory(fact) if fact == non_dir.file_type));
        let (_, trace) = take_mutation_evidence(); assert_facts(&facts, &trace); let Attempt::Succeeded(open) = facts.open else { unreachable!() }; assert_closed(open.raw_fd);
        PANIC_AFTER_OPEN_ADOPTION.with(|fd| fd.set(Some(-1))); let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.open_or_create_child(&parent, "adoption-panic"))); let adopted = PANIC_AFTER_OPEN_ADOPTION.with(|fd| fd.replace(None)).unwrap(); assert!(panic.is_err() && adopted >= 0); assert_closed(adopted); take_mutation_evidence();
        for name in ["file", "link"] { take_mutation_evidence(); let (facts, child) = attempted(session.open_or_create_child(&parent, name)); assert!(matches!(child.unwrap_err().kind(), FsErrorKind::NotDirectory(fact) if matches!(facts.observe, Attempt::Succeeded(observed) if fact == observed.file_type))); let (_, trace) = take_mutation_evidence(); assert_facts(&facts, &trace); assert!(matches!(facts.open, Attempt::Failed(io) if io.raw_errno == SYMLINK_ERRNO)); }
        let swap = path.join("swap"); let retired = path.join("swap-retired"); POST_OBSERVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || { fs::rename(&swap, &retired).unwrap(); fs::create_dir(&swap).unwrap(); })));
        let (mismatch, child) = attempted(session.open_or_create_child(&parent, "swap")); assert!(matches!(child.unwrap_err().kind(), FsErrorKind::IdentityMismatch { observed, opened } if observed != opened && matches!(mismatch.observe, Attempt::Succeeded(fact) if observed == fact.identity) && matches!(mismatch.fstat, Attempt::Succeeded(fact) if opened == fact.identity)));
        let (_, trace) = take_mutation_evidence(); assert_facts(&mismatch, &trace); if let Attempt::Succeeded(open) = mismatch.open { assert_closed(open.raw_fd); }
        let nested_fd = existing_child.fd.as_raw_fd(); let (nested, nested_child) = attempted(session.open_or_create_child(&existing_child, "leaf")); let nested_child = nested_child.unwrap();
        let (_, trace) = take_mutation_evidence(); assert_facts(&nested, &trace); assert_eq!(trace[0].1.0, nested_fd);
        assert!(matches!(&nested.locator, DirectoryLocator::Child { parent, component } if (*parent, component.as_bytes()) == (existing_child.open.identity, b"leaf")) && nested.locator == nested_child.locator);
        take_mutation_evidence(); assert!(matches!(session.open_or_create_child(&foreign, ".."), DirectoryMutation::Rejected(error) if error.kind() == FsErrorKind::CrossLineage));
        let (activity, trace) = take_mutation_evidence(); assert_eq!((activity, trace.is_empty()), (0, true)); let survivor = fresh_child.clone();
        drop(session); drop((fresh_child, nested_child, existing_child, parent, foreign, other)); drop(root);
        assert_ne!(unsafe { libc::fcntl(survivor.fd.as_raw_fd(), libc::F_GETFD) }, -1); drop(survivor); assert_closed(raw_fd);
    }
    fn assert_io(error: FsError, expected: (FsOperation, i32)) {
        let FsErrorKind::Io(io) = error.kind() else {
            panic!("expected I/O error, got {error:?}");
        };
        assert_eq!((io.operation(), io.raw_errno()), expected);
    }

    fn assert_closed(fd: RawFd) {
        // SAFETY: F_GETFD takes no variadic argument and never dereferences fd.
        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!((result, last_errno()), (-1, libc::EBADF));
    }

    fn assert_child(output: std::process::Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success()
                && stdout.contains("running 1 test")
                && stdout.contains("1 passed; 0 failed; 0 ignored; 0 measured;"),
            "isolated child stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn absolute_and_relative_root_traversal_record_exact_pairs_and_identity() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(&cwd).unwrap();
        let nested = temp.path().join("outer/inner");
        fs::create_dir_all(&nested).unwrap();
        let relative = nested.strip_prefix(&cwd).unwrap().to_owned();
        for path in [PathBuf::from("/"), PathBuf::from("."), nested, relative] {
            TRACE.with(|trace| trace.borrow_mut().clear());
            let root = open_runtime_root(&path).unwrap();
            let trace = TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()));
            let mut expected = vec![if path.is_absolute() {
                b"/".as_slice()
            } else {
                b"."
            }];
            expected.extend(path.components().filter_map(|component| match component {
                Component::Normal(value) => Some(value.as_bytes()),
                _ => None,
            }));
            let mut pairs = trace.chunks_exact(2);
            assert!(pairs.remainder().is_empty());
            assert_eq!(pairs.len(), expected.len());
            let mut prior_result = libc::AT_FDCWD;
            let mut final_fact = None;
            for (pair, component) in pairs.by_ref().zip(expected) {
                let (TraceResult::Open(open), TraceResult::Fstat(fstat)) = (&pair[0], &pair[1])
                else {
                    panic!("trace was not an exact open/fstat pair: {pair:?}");
                };
                assert_eq!(open.parent_fd, prior_result);
                assert_eq!(&*open.component, component);
                assert_eq!(open.flags, EXPECTED_OPEN_FLAGS);
                assert!(open.result_fd >= 0);
                assert_eq!(open.errno, None);
                assert!(open.f_getfd_result >= 0);
                assert_eq!(open.f_getfd_errno, None);
                assert_ne!(open.f_getfd_result & libc::FD_CLOEXEC, 0);
                assert_eq!(fstat.fd, open.result_fd);
                assert_eq!(fstat.result, 0);
                assert_eq!(fstat.errno, None);
                final_fact = fstat.fact;
                prior_result = open.result_fd;
            }
            let metadata = fs::metadata(&path).unwrap();
            let independent = DirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            assert_eq!(root.identity(), independent);
            assert_eq!(root.lineage().root(), independent);
            assert_eq!(final_fact.unwrap().identity, independent);
        }

        for (path, expected) in [
            ("", InvalidRootFact::Empty),
            ("..", InvalidRootFact::ParentDir),
            ("child/../escape", InvalidRootFact::ParentDir),
        ] {
            TRACE.with(|trace| trace.borrow_mut().clear());
            assert_eq!(
                open_runtime_root(Path::new(path)).unwrap_err().kind(),
                FsErrorKind::InvalidRoot(expected)
            );
            assert!(TRACE.with(|trace| trace.borrow().is_empty()));
        }
    }

    #[test]
    fn root_traversal_and_path_replacement_keep_the_pinned_inode() {
        const TEST_NAME: &str = "services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::root_traversal_and_path_replacement_keep_the_pinned_inode";
        const ISOLATED_ENV: &str = "AGENTDESK_5608_PINNED_INODE_CHILD";
        // SAFETY: getppid has no preconditions.
        let parent_pid = unsafe { libc::getppid() }.to_string();
        if std::env::var(ISOLATED_ENV).ok().as_deref() != Some(parent_pid.as_str()) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST_NAME, "--exact", "--test-threads=1"])
                .env(ISOLATED_ENV, std::process::id().to_string())
                .output()
                .unwrap();
            assert_child(output);
            return;
        }
        let assert_successful_open = |open: &OpenResult, parent_fd: RawFd, component: &[u8]| {
            let observed = (open.parent_fd, &*open.component, open.flags);
            assert_eq!(observed, (parent_fd, component, EXPECTED_OPEN_FLAGS));
            assert!(open.result_fd >= 0 && open.f_getfd_result >= 0);
            assert_eq!((open.errno, open.f_getfd_errno), (None, None));
            assert_ne!(open.f_getfd_result & libc::FD_CLOEXEC, 0);
        };

        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(&cwd).unwrap();
        let root_path = temp.path();
        let relative = root_path.strip_prefix(&cwd).unwrap();
        let temp_name = relative.as_os_str().as_bytes();
        let nested = root_path.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root_path.join("file"), b"not a directory").unwrap();
        fs::write(nested.join("file"), b"not a directory").unwrap();
        std::os::unix::fs::symlink(root_path, root_path.join("root-link")).unwrap();
        std::os::unix::fs::symlink(&cwd, nested.join("link")).unwrap();

        let successful_prefix = |trace: &[TraceResult], expected: &[&[u8]], keep: Option<RawFd>| {
            assert_eq!(trace.len(), expected.len() * 2);
            let mut prior = libc::AT_FDCWD;
            let mut final_fact = None;
            for (pair, component) in trace.chunks_exact(2).zip(expected) {
                let [TraceResult::Open(open), TraceResult::Fstat(fstat)] = pair else {
                    panic!("successful prefix was not open/fstat: {pair:?}");
                };
                assert_successful_open(open, prior, component);
                let observed = (fstat.fd, fstat.result, fstat.errno);
                assert_eq!(observed, (open.result_fd, 0, None));
                if Some(open.result_fd) != keep {
                    assert_closed(open.result_fd);
                }
                final_fact = fstat.fact;
                prior = open.result_fd;
            }
            (final_fact.unwrap(), prior)
        };

        let reject = |suffix: &str, ok: &[&[u8]], failed: &[u8], errno: i32| {
            let error = open_runtime_root(&relative.join(suffix)).unwrap_err();
            assert_io(error, (FsOperation::Open, errno));
            let trace = TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()));
            let (TraceResult::Open(open), prefix) = trace.split_last().unwrap() else {
                panic!("failed component had a trailing fstat: {trace:?}");
            };
            let prior = successful_prefix(prefix, ok, None).1;
            assert_eq!(
                (open.parent_fd, &*open.component, open.flags),
                (prior, failed, EXPECTED_OPEN_FLAGS)
            );
            assert_eq!((open.result_fd, open.errno), (-1, Some(errno)));
            assert_eq!((open.f_getfd_result, open.f_getfd_errno), (-1, None));
            assert_closed(prior);
        };
        let root_ok = [b".".as_slice(), temp_name];
        let nested_ok = [root_ok[0], temp_name, b"nested"];
        reject("missing/leaf", &root_ok, b"missing", libc::ENOENT);
        reject("nested/missing", &nested_ok, b"missing", libc::ENOENT);
        reject("root-link/nested", &root_ok, b"root-link", SYMLINK_ERRNO);
        reject("nested/link", &nested_ok, b"link", SYMLINK_ERRNO);
        reject("file/leaf", &root_ok, b"file", libc::ENOTDIR);
        reject("nested/file", &nested_ok, b"file", libc::ENOTDIR);

        for (path, expected) in [(Path::new("."), b".".as_slice()), (Path::new("/"), b"/")] {
            let anchor = open_runtime_root(path).unwrap();
            assert!(matches!(&anchor.directory.locator,
                DirectoryLocator::Anchor { component } if component.as_bytes() == expected));
        }
        TRACE.with(|trace| trace.borrow_mut().clear());
        let identity = |metadata: &fs::Metadata| DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let mut root = open_runtime_root(&relative.join("nested")).unwrap();
        let keep = root.directory.fd.as_raw_fd();
        let trace = TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()));
        let (final_fact, raw_fd) = successful_prefix(&trace, &nested_ok, Some(keep));
        assert_eq!(raw_fd, keep);
        let other = open_runtime_root(&relative.join("nested")).unwrap();
        assert_eq!(root.lineage(), other.lineage());
        assert!(!Arc::ptr_eq(&root.directory.seal, &other.directory.seal));
        let parent_identity = identity(&fs::metadata(root_path).unwrap());
        assert!(matches!(&root.directory.locator,
            DirectoryLocator::Child { parent, component }
                if (*parent, component.as_bytes()) == (parent_identity, b"nested")));
        TRACE.with(|trace| trace.borrow_mut().clear());
        let foreign = other.directory.clone();
        let parent = root.directory.clone();
        let parent_identity = root.identity();
        let mut session = root.mutation_session().unwrap();
        super::super::take_preflight_activity();
        let error = session.prepare_child(&foreign, "..").unwrap_err();
        assert_eq!(error.kind(), FsErrorKind::CrossLineage);
        assert_eq!(super::super::take_preflight_activity(), 0);
        for (value, fact) in [
            ("", super::super::InvalidComponentFact::Empty),
            (".", super::super::InvalidComponentFact::Current),
            ("..", super::super::InvalidComponentFact::Parent),
            ("slash/name", super::super::InvalidComponentFact::Character),
            ("back\\slash", super::super::InvalidComponentFact::Character),
            ("white space", super::super::InvalidComponentFact::Character),
            ("nul\0", super::super::InvalidComponentFact::Character),
            ("é", super::super::InvalidComponentFact::Character),
            ("/absolute", super::super::InvalidComponentFact::Character),
        ] {
            assert_eq!(
                session.prepare_child(&parent, value).unwrap_err().kind(),
                FsErrorKind::InvalidComponent(fact)
            );
        }
        for expected in ["AZaz09_-", "..."] {
            let candidate = session.prepare_child(&parent, expected).unwrap();
            assert!(matches!(&candidate.1,
                DirectoryLocator::Child { component, .. }
                    if component.as_bytes() == expected.as_bytes()));
        }
        let prepared = session.prepare_child(&parent, "a..b").unwrap();
        assert_eq!(super::super::take_preflight_activity(), 3);
        assert_eq!(prepared.0.fd.as_raw_fd(), keep);
        let DirectoryLocator::Child {
            parent: locator_parent,
            component,
        } = &prepared.1
        else {
            panic!("prepared child did not retain a child locator")
        };
        assert_eq!(*locator_parent, parent_identity);
        assert_eq!(component.as_bytes(), b"a..b");
        assert!(TRACE.with(|trace| trace.borrow().is_empty()));
        drop((session, parent, foreign, other));
        let original = identity(&fs::metadata(&nested).unwrap());
        let original_pair = (original, original);
        let retired = nested.with_extension("retired");
        fs::rename(&nested, &retired).unwrap();
        std::os::unix::fs::symlink(&cwd, &nested).unwrap();
        assert!(!cwd.join("pinned-mutation").exists());
        let pinned_parent = root.directory.clone();
        let mut mutation = root.mutation_session().unwrap();
        let (_, pinned_child) =
            attempted(mutation.open_or_create_child(&pinned_parent, "pinned-mutation"));
        let pinned_child = pinned_child.unwrap();
        assert!(retired.join("pinned-mutation").is_dir());
        assert!(!nested.join("pinned-mutation").exists());
        drop((pinned_child, mutation, pinned_parent));
        let replacement = identity(&fs::metadata(&nested).unwrap());
        let clone = fs::File::from(root.directory.fd.try_clone().unwrap());
        let pinned = identity(&clone.metadata().unwrap());
        fs::remove_file(&nested).unwrap();
        fs::rename(&retired, &nested).unwrap();
        assert_eq!(root.lineage().anchor(), replacement);
        assert_eq!((root.lineage().root(), root.identity()), original_pair);
        assert_eq!((final_fact.identity, pinned), original_pair);
        assert_ne!(pinned, replacement);
        drop(root);
        let live = unsafe { libc::fcntl(prepared.0.fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(live, -1);
        drop(prepared);
        assert_closed(raw_fd);

        FSTAT_FAILURE.with(|failure| failure.set(Some(1)));
        let error = open_runtime_root(relative).unwrap_err();
        assert_io(error, (FsOperation::Fstat, libc::EIO));
        let trace = TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()));
        let prior = successful_prefix(&trace[..2], &root_ok[..1], None).1;
        let [TraceResult::Open(open), TraceResult::Fstat(fstat)] = &trace[2..] else {
            panic!("final injected trace was not one exact open/fstat: {trace:?}");
        };
        assert_successful_open(open, prior, temp_name);
        let observed = (fstat.fd, fstat.result, fstat.errno, fstat.fact);
        assert_eq!(observed, (open.result_fd, -1, Some(libc::EIO), None));
        assert_closed(open.result_fd);
        assert_closed(prior);
    }
}
