//! #5948 (I18): where a supervisor forward's bytes came from —
//! `SupervisorFrameSourceAuthority` plus the terminal/tail span split. PURE MOVE
//! out of the parent `supervisor_relay`, which re-exports it; both stay under
//! the 700-line cap.

use crate::services::cluster::stream_relay::{ReadSource, SourceFileIdentity};
use crate::services::discord::tmux::tmux_watcher::loop_poll_prologue::WatcherSourceAuthority;

#[derive(Clone, Copy)]
pub(in crate::services::discord::tmux::tmux_watcher) struct SupervisorFrameSourceAuthority {
    pub(in crate::services::discord::tmux::tmux_watcher) generation_mtime_ns: i64,
    pub(in crate::services::discord::tmux::tmux_watcher) source_stamp:
        Option<crate::services::cluster::stream_relay::SourceStamp>,
    /// #5948 (I18): absolute JSONL byte range the payload was read from, when
    /// known. It rides the authority rather than a per-helper parameter because
    /// it answers the same provenance question one coordinate finer. `None`
    /// when unnameable; a named range is authoritative, so never invented here.
    pub(in crate::services::discord::tmux::tmux_watcher) source_span: Option<(u64, u64)>,
    /// The opened file the read came from; kept out of `WatcherSourceAuthority`
    /// equality so the watcher generation never splits a UTF-8 carry.
    pub(in crate::services::discord::tmux::tmux_watcher) source_file: SourceFileIdentity,
    /// The reading watcher's `watcher_instance_id`, once the forward names it.
    pub(in crate::services::discord::tmux::tmux_watcher) producer: Option<u64>,
}

impl SupervisorFrameSourceAuthority {
    /// Both halves or nothing: a frame without a named file and watcher folds
    /// under the sink's single-source rules.
    pub(in crate::services::discord::tmux::tmux_watcher) fn read_source(
        self,
    ) -> Option<ReadSource> {
        let producer = self.producer?;
        (self.source_file != SourceFileIdentity::Unavailable).then_some(ReadSource {
            file: self.source_file,
            producer,
        })
    }
}

impl From<i64> for SupervisorFrameSourceAuthority {
    fn from(generation_mtime_ns: i64) -> Self {
        Self {
            generation_mtime_ns,
            source_stamp: None,
            source_span: None,
            source_file: SourceFileIdentity::Unavailable,
            producer: None,
        }
    }
}

impl From<WatcherSourceAuthority> for SupervisorFrameSourceAuthority {
    fn from(authority: WatcherSourceAuthority) -> Self {
        Self {
            generation_mtime_ns: authority.generation_mtime_ns,
            source_stamp: authority.source_stamp,
            source_span: None,
            source_file: authority.source_file,
            producer: None,
        }
    }
}

/// #5948 (I18): attach the absolute source byte range a forward carries to the
/// authority that already describes the forward's provenance.
pub(in crate::services::discord::tmux::tmux_watcher) fn source_authority_with_span(
    source_authority: impl Into<SupervisorFrameSourceAuthority>,
    source_span: Option<(u64, u64)>,
) -> SupervisorFrameSourceAuthority {
    SupervisorFrameSourceAuthority {
        source_span,
        ..source_authority.into()
    }
}

type SourceSpan = Option<(u64, u64)>;

/// #5948 (I18): split one contiguous source byte range at the same boundary
/// `split_decoded_chunk_at_terminal_boundary` splits the payload at, so each
/// forwarded frame names exactly the bytes it carries. `None` in ⇒ `None` out on
/// both sides: a range the caller cannot name must never be invented for it.
pub(in crate::services::discord::tmux::tmux_watcher) fn split_source_span_at_terminal_boundary(
    span: SourceSpan,
    terminal_len: usize,
) -> (SourceSpan, SourceSpan) {
    let Some((start, end)) = span else {
        return (None, None);
    };
    let boundary = start.saturating_add(terminal_len as u64).min(end);
    let terminal = (boundary > start).then_some((start, boundary));
    let tail = (end > boundary).then_some((boundary, end));
    (terminal, tail)
}
