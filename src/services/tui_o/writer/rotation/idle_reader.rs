//! Reader work that can become due while source and spool stamps stay unchanged.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetiredWatch {
    Descriptor(Instant),
    PathUntil(Instant),
    Path,
    Expired,
}

impl RetiredWatch {
    fn expired(self, now: Instant) -> bool {
        match self {
            Self::Descriptor(until) | Self::PathUntil(until) => now >= until,
            Self::Path | Self::Expired => false,
        }
    }
}

impl Reader {
    pub(super) fn retire(&mut self, watch: RetiredWatch) {
        if !matches!(watch, RetiredWatch::Descriptor(_)) {
            self.capture = None;
        }
        self.retired_watch = Some(watch);
        self.quiet = None;
        self.dormant = None;
    }

    pub(super) fn watch_growth(
        &mut self,
        watch: RetiredWatch,
        store: &ChannelStore,
        now: Instant,
    ) -> bool {
        let grew = match watch {
            RetiredWatch::Descriptor(_) => self.descriptor_grew(),
            RetiredWatch::PathUntil(_) | RetiredWatch::Path => {
                store.cursor(&self.source).is_some_and(|cursor| {
                    Fingerprint::path(&self.source).is_ok_and(|fp| fp.len > cursor.captured_through)
                })
            }
            RetiredWatch::Expired => false,
        };
        if !grew && watch.expired(now) {
            self.retire(RetiredWatch::Expired);
        }
        grew
    }

    fn descriptor_grew(&self) -> bool {
        self.capture.as_ref().is_some_and(|capture| {
            capture
                .file_len()
                .is_ok_and(|len| len > capture.captured_through())
        })
    }
}

impl<B: BindingEvents> Sources<B> {
    pub(crate) fn reader_maintenance_due(&self) -> bool {
        let now = Instant::now();
        let wall = SystemTime::now();
        self.last_wall.is_some_and(|last| wall < last)
            || self
                .readers
                .iter()
                .any(|reader| !reader.parked && self.reader_due(reader, now, wall))
    }

    fn reader_due(&self, reader: &Reader, now: Instant, wall: SystemTime) -> bool {
        if let Some(watch) = reader.retired_watch {
            // An explicit transition can still hold a descriptor after its path is removed.
            return watch.expired(now)
                || (matches!(watch, RetiredWatch::Descriptor(_)) && reader.descriptor_grew());
        }
        if reader.rotated_at.is_none() {
            return false;
        }
        let Some(next) = self.rotation.successors.get(&reader.key) else {
            return false;
        };
        #[cfg(unix)]
        if self.provider == ShadowProvider::Codex
            && next.proof.is_none()
            && next.tmux_session.as_ref().is_some_and(|pane| {
                crate::services::codex_tui::execution_end::attempt::bound_candidate_pending(
                    self.channel,
                    pane,
                )
            })
        {
            return true;
        }
        if let Some(due) = reader.dormant {
            return now >= due;
        }
        if self.reader_config.is_some_and(|config| {
            reader
                .idle
                .as_ref()
                .is_none_or(|(_, at)| config.dormant && now - *at >= DORMANT_QUIET)
                || reader.persisted_at.is_none_or(|at| {
                    wall.duration_since(at)
                        .is_ok_and(|age| age >= WITNESS_REFRESH)
                })
        }) {
            return true;
        }
        if self.provider == ShadowProvider::Codex && next.proof.is_none() && self.replaced.is_none()
        {
            return true;
        }
        let quiet = reader.quiet.is_none_or(|(_, at)| now - at >= RETIRE_QUIET);
        quiet
            && (next.proof.is_some() || self.aged_replacement_due(reader, now, wall))
            && self
                .readers
                .iter()
                .any(|successor| successor.source == next.source && successor.captured_any)
    }

    pub(super) fn aged_replacement_due(
        &self,
        reader: &Reader,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        self.provider == ShadowProvider::Codex
            && self.replaced_source(reader)
            && reader.idle.as_ref().is_some_and(|(fp, at)| {
                // Herdr has no Codex end evidence. Its earlier silence threshold is authoritative.
                let silence = if crate::config::session_hosts::has_herdr_endpoint(self.channel) {
                    HERDR_UNPROVEN_SILENCE
                } else {
                    RETIRED_WATCH
                };
                now - *at >= RETIRE_QUIET
                    && wall
                        .duration_since(fp.modified)
                        .is_ok_and(|age| age >= silence)
            })
    }
}
