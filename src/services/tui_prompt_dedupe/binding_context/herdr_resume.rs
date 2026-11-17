//! A restart's immutable native read boundary, shared by launch, hooks and restored input.

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use sha2::{Digest, Sha256};

#[cfg(unix)]
use super::{BindingContext, execution_context};
#[cfg(unix)]
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeStart {
    pub operation: String,
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub offset: u64,
}

#[cfg(unix)]
impl ResumeStart {
    pub(crate) fn validate(&self, source: &SourceId) -> Result<(), String> {
        let metadata = std::fs::metadata(&self.path).map_err(|e| e.to_string())?;
        if self.operation.trim().is_empty()
            || self.offset == 0
            || !self.path.is_absolute()
            || self.path != source.path
            || (self.dev, self.ino) != (source.dev, source.ino)
            || (metadata.dev(), metadata.ino()) != (self.dev, self.ino)
            || !metadata.is_file()
            || metadata.len() < self.offset
        {
            return Err("codex herdr resume: source or restart boundary changed".into());
        }
        Ok(())
    }

    /// Capture only after the caller confirmed the old execution ended; O must have captured EOF.
    pub(crate) fn capture(
        operation: String,
        channel: u64,
        source: &SourceId,
        store: &crate::services::tui_o::store::OStore,
    ) -> Result<Self, String> {
        let mut file = File::open(&source.path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let start = Self {
            operation,
            path: source.path.clone(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            offset: metadata.len(),
        };
        start.validate(source)?;
        if start.offset > 0 {
            file.seek(SeekFrom::End(-1)).map_err(|e| e.to_string())?;
            let mut last = [0];
            file.read_exact(&mut last).map_err(|e| e.to_string())?;
            if last != [b'\n'] {
                return Err("codex herdr resume: incomplete transcript at EOF".into());
            }
        }
        let cursors = store.peek_cursors(channel).map_err(|e| format!("{e:?}"))?;
        let mut same = cursors.iter().filter(|cursor| cursor.source == *source);
        let cursor = same
            .next()
            .ok_or("codex herdr resume: no matching O cursor")?;
        if same.next().is_some() || cursor.captured_through != start.offset || cursor.retired {
            return Err("codex herdr resume: O cursor must equal the ended source EOF".into());
        }
        file.rewind().map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut prefix = file.take(start.offset);
        let mut buffer = [0; 64 * 1024];
        let mut read = 0;
        loop {
            let n = prefix.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            read += n as u64;
        }
        if read != start.offset || format!("{:x}", hash.finalize()) != cursor.prefix_hash {
            return Err("codex herdr resume: O cursor prefix differs".into());
        }
        start.validate(source)?;
        Ok(start)
    }
}

#[cfg(unix)]
pub(crate) fn start_offset(context: &BindingContext, source: &SourceId) -> Result<u64, String> {
    if context.launch_mode == "fresh" && context.resume_start.is_none() {
        return Ok(0);
    }
    let start = context
        .resume_start
        .as_ref()
        .ok_or("codex herdr resume: no durable start")?;
    if context.provider != "codex"
        || context.launch_mode != "resume"
        || context.expected_native_session_id.as_deref() != Some(source.session_id.as_str())
        || execution_context("codex", &context.execution_nonce)? != *context
    {
        return Err("codex herdr resume: incarnation identity differs".into());
    }
    start.validate(source)?;
    Ok(start.offset)
}

#[cfg(unix)]
pub(crate) fn pending_start(nonce: &str) -> Result<Option<(SourceId, u64)>, String> {
    let context = execution_context("codex", nonce)?;
    if context.launch_mode == "fresh" && context.resume_start.is_none() {
        return Ok(None);
    }
    let start = context
        .resume_start
        .as_ref()
        .ok_or("codex herdr resume: no durable start")?;
    let source = SourceId {
        session_id: context
            .expected_native_session_id
            .clone()
            .ok_or("codex herdr resume: no native session")?,
        path: start.path.clone(),
        dev: start.dev,
        ino: start.ino,
    };
    let offset = start_offset(&context, &source)?;
    Ok(Some((source, offset)))
}
