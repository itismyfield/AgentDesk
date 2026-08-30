use super::{
    ConfinedDir, ConfinedRuntimeRoot, DirectoryIdentity, DirectoryTypeFact, FsError, FsOperation,
    InvalidRootFact, OpenDirectoryFact, RootLineage,
};
use std::{
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
};

pub(super) type DirHandle = OwnedFd;

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
    for component in &plan.components {
        (fd, root) = open_directory(fd.as_raw_fd(), component)?;
    }
    Ok(ConfinedRuntimeRoot {
        lineage: RootLineage {
            anchor: anchor.identity,
            root: root.identity,
        },
        directory: ConfinedDir { fd, open: root },
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
    // SAFETY: component is NUL-terminated and parent_fd is either AT_FDCWD or owned by caller.
    let result_fd = unsafe { libc::openat(parent_fd, component.as_ptr(), OPEN_DIRECTORY_FLAGS) };
    if result_fd == -1 {
        let raw_errno = last_errno();
        #[cfg(test)]
        record_open(parent_fd, component, result_fd, Some(raw_errno), -1, None);
        return Err(FsError::io(FsOperation::Open, raw_errno));
    }
    // SAFETY: openat returned a new descriptor; adoption precedes every fallible trace action.
    let fd = unsafe { OwnedFd::from_raw_fd(result_fd) };
    // SAFETY: fd is live and F_GETFD takes no variadic argument.
    let fd_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    let fd_flags_errno = (fd_flags == -1).then(last_errno);
    #[cfg(test)]
    record_open(
        parent_fd,
        component,
        result_fd,
        None,
        fd_flags,
        fd_flags_errno,
    );
    if let Some(raw_errno) = fd_flags_errno {
        return Err(FsError::io(FsOperation::GetFdFlags, raw_errno));
    }
    if fd_flags & libc::FD_CLOEXEC == 0 {
        return Err(FsError::missing_close_on_exec(fd_flags));
    }

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fd is live and stat points to writable storage for one libc::stat.
    let result = unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) };
    if result == -1 {
        let raw_errno = last_errno();
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
}

#[cfg(test)]
fn record_open(
    parent_fd: RawFd,
    component: &CStr,
    result_fd: RawFd,
    errno: Option<i32>,
    f_getfd_result: libc::c_int,
    f_getfd_errno: Option<i32>,
) {
    TRACE.with(|trace| {
        trace.borrow_mut().push(TraceResult::Open(OpenResult {
            parent_fd,
            component: component.to_bytes().into(),
            flags: OPEN_DIRECTORY_FLAGS,
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
                assert_eq!(
                    open.flags,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW
                );
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
}
