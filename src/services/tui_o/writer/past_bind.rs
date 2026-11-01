//! Where a source first bound after the switch starts owing, holding one an adoption left unpinned.

use super::binding::{BindingCause, BindingEvent};
use super::rotation::bound_source;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::rotation::Boundary;

/// Whether `log` bound `source` before `seq` under an unattached name with its session and path, or
/// its path and inode when either session is empty (any device); an unreadable log counts as bound.
pub(super) fn unread_before(
    log: Option<&[BindingEvent]>,
    seq: u64,
    source: &SourceId,
    attached: impl Fn(&SourceId) -> bool,
) -> bool {
    let Some(events) = log else {
        return true;
    };
    let earlier = events.iter().filter(|event| event.seq < seq);
    let mut unread = earlier.filter_map(bound_source).filter(|b| !attached(b));
    unread.any(|bound| {
        let unnamed = bound.session_id.is_empty() || source.session_id.is_empty();
        let session = bound.session_id == source.session_id || (unnamed && bound.ino == source.ino);
        session && bound.path == source.path
    })
}

/// The boundary of a source first bound by `cause`, with a verified parent when `parented`.
pub(super) fn first_boundary(cause: BindingCause, parented: bool, unread: bool) -> Boundary {
    match (cause, parented) {
        // An adoption left this past source unpinned, so its records are old: hold them.
        _ if unread => Boundary::Pending {
            candidates: vec![0],
        },
        (BindingCause::Startup | BindingCause::Clear, _) => Boundary::Owed { from: 0 },
        (BindingCause::Unknown, _) | (_, false) => Boundary::Pending {
            candidates: vec![0],
        },
        _ => Boundary::Undecided,
    }
}
