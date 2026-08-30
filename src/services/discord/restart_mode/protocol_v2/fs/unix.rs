use super::{
    ConfinedDir, ConfinedRuntimeRoot, DirectoryIdentity, DirectoryLocator, DirectoryTypeFact,
    FsError, FsOperation, InvalidRootFact, LineageSeal, OpenDirectoryFact, RootLineage,
};
use std::{
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
    sync::Arc,
};

pub(super) type DirHandle = OwnedFd;
pub(super) const MUTATION_SUPPORTED: bool = true;

const OPEN_DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;

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
    let identity = DirectoryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    };
    let file_type = DirectoryTypeFact {
        mode: stat.st_mode as u32,
    };
    let fact = OpenDirectoryFact {
        identity,
        file_type,
    };
    #[cfg(test)]
    record_fstat(fd.as_raw_fd(), result, None, Some(fact));
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(FsError::not_directory(file_type.mode));
    }
    Ok((fd, fact))
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
mod high_risk_recovery {
    use super::*;
    use crate::services::discord::restart_mode::protocol_v2::fs::FsErrorKind;
    use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

    const EXPECTED_OPEN_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    const SYMLINK_ERRNO: i32 = libc::ENOTDIR;
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
        let mut session = root.mutation_session();
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
