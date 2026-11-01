//! One submission's open transcript and byte boundary; pane idleness is never its receipt.
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::sync::Arc;

/// A pane stays the same across handoff and SID rehydration, but not a respawn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaneIdentity {
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    nonce: SpawnNonceMarker,
}
impl PaneIdentity {
    fn from_binding(session: &str, binding: &TuiRuntimeBinding) -> Option<Self> {
        let nonce = binding_context::observe_spawn_nonce_marker(session);
        (nonce != SpawnNonceMarker::Unreadable).then(|| Self {
            runtime_kind: binding.runtime_kind,
            output_path: binding.output_path.clone(),
            nonce,
        })
    }
    pub(crate) fn capture(session: &str) -> Option<Self> {
        crate::services::tmux_common::with_tmux_source_authority(session, |authority| {
            let binding =
                tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                    authority,
                )?;
            Self::from_binding(session, &binding)
        })
    }
}

#[derive(Debug)]
pub(crate) struct UserReceipt {
    session: String,
    pane: Arc<PaneIdentity>,
    sid: String,
    file: Option<File>,
    offset: u64,
    #[cfg(unix)]
    scanned: u64,
    #[cfg(unix)]
    partial: Vec<u8>,
    end: Option<u64>,
    ended: bool,
    frame: String,
}

impl UserReceipt {
    /// Share the writer's snapshot, never recapture its boundary for a second consumer.
    pub(crate) fn copy_for_native(&self) -> Option<Self> {
        Some(Self {
            session: self.session.clone(),
            pane: self.pane.clone(),
            sid: self.sid.clone(),
            file: match &self.file {
                Some(file) => Some(file.try_clone().ok()?),
                None => None,
            },
            offset: self.offset,
            #[cfg(unix)]
            scanned: self.offset,
            #[cfg(unix)]
            partial: Vec::new(),
            end: None,
            ended: false,
            frame: self.frame.clone(),
        })
    }

    pub(crate) fn pane_identity(&self) -> Arc<PaneIdentity> {
        self.pane.clone()
    }

    /// Continue at the unread byte, retaining a partial row until its newline arrives.
    pub(crate) fn try_pin_native_user(
        &mut self,
    ) -> Result<Option<crate::services::provider::native_stop::NativeUserPin>, ()> {
        #[cfg(not(unix))]
        {
            Err(())
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileExt, MetadataExt};
            const CAP: u64 = 16 * 1024 * 1024;
            let session = self.session.clone();
            crate::services::tmux_common::with_tmux_source_authority(&session, |authority| {
                let Some(current) =
                    tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                        authority,
                    )
                else {
                    return Ok(None);
                };
                let Some(pane) = PaneIdentity::from_binding(&self.session, &current) else {
                    return Ok(None);
                };
                if &pane != self.pane.as_ref()
                    || current
                        .session_id
                        .as_deref()
                        .is_some_and(|sid| sid != self.sid)
                {
                    return Err(());
                }
                if self.file.is_none() {
                    let Ok(file) = open(&current.output_path) else {
                        return Ok(None);
                    };
                    self.file = Some(file);
                }
                let file = self.file.as_ref().ok_or(())?;
                let Ok(metadata) = file.metadata() else {
                    return Ok(None);
                };
                let Ok(path_metadata) = std::fs::symlink_metadata(&self.pane.output_path) else {
                    return Ok(None);
                };
                if !path_metadata.is_file()
                    || (metadata.dev(), metadata.ino())
                        != (path_metadata.dev(), path_metadata.ino())
                    || metadata.len() < self.scanned
                {
                    return Err(());
                }
                while self.scanned < metadata.len() {
                    let remaining = CAP.saturating_sub(self.scanned - self.offset);
                    if remaining == 0 {
                        return Err(());
                    }
                    let size = (metadata.len() - self.scanned)
                        .min(remaining)
                        .min(64 * 1024) as usize;
                    let mut bytes = vec![0; size];
                    if file.read_exact_at(&mut bytes, self.scanned).is_err() {
                        return Ok(None);
                    }
                    #[cfg(test)]
                    {
                        NATIVE_READ_BYTES.with(|total| total.set(total.get() + size as u64));
                    }
                    self.scanned += size as u64;
                    self.partial.extend_from_slice(&bytes);
                    if !bytes.contains(&b'\n') {
                        continue;
                    }
                    let mut consumed = 0;
                    for line in self.partial.split_inclusive(|byte| *byte == b'\n') {
                        if !line.ends_with(b"\n") {
                            break;
                        }
                        let start = self.scanned - self.partial.len() as u64 + consumed as u64;
                        consumed += line.len();
                        let record: serde_json::Value =
                            serde_json::from_slice(line).map_err(|_| ())?;
                        if record["isSidechain"] == true {
                            continue;
                        }
                        let Some((_, uuid)) =
                            tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                                &record,
                            )
                        else {
                            continue;
                        };
                        let exact = user_text(&record).is_some_and(|text| {
                            super::super::busy_inject::exact_receipt(&normalize(&text), &self.frame)
                        });
                        if record["sessionId"].as_str() != Some(self.sid.as_str())
                            || !exact
                            || !same_file(file, &self.pane.output_path)
                        {
                            return Err(());
                        }
                        if PaneIdentity::from_binding(&self.session, &current).as_ref()
                            != Some(self.pane.as_ref())
                        {
                            return Err(());
                        }
                        return Ok(Some(
                            crate::services::provider::native_stop::NativeUserPin {
                                uuid: uuid.filter(|id| !id.is_empty()).ok_or(())?,
                                source_file: (metadata.dev(), metadata.ino()),
                                range: (start, start + line.len() as u64),
                                receipt_start: self.offset,
                                row_digest: digest(line),
                                frame_digest: digest(self.frame.as_bytes()),
                                session_id: self.sid.clone(),
                                output_path: self.pane.output_path.clone(),
                                tmux_session: self.session.clone(),
                                spawn_nonce: match &self.pane.nonce {
                                    SpawnNonceMarker::Known(nonce) => Some(nonce.clone()),
                                    _ => None,
                                },
                                direct: false,
                            },
                        ));
                    }
                    self.partial.drain(..consumed);
                }
                if self.scanned - self.offset >= CAP {
                    Err(())
                } else {
                    Ok(None)
                }
            })
        }
    }

    pub(crate) fn before(session: &str, prompt: &str, sid: Option<&str>) -> Option<Self> {
        crate::services::tmux_common::with_tmux_source_authority(session, |authority| {
            let mut binding =
                tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                    authority,
                )?;
            binding.session_id = sid.map(str::to_owned).or(binding.session_id);
            if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui
                || binding.session_id.as_deref().is_none_or(str::is_empty)
            {
                return None;
            }
            Self::snapshot(session, prompt, binding, false)
        })
    }

    /// The launch already names its native session and transcript, which may not exist yet.
    pub(crate) fn before_fresh(session: &str, prompt: &str, path: &str, sid: &str) -> Option<Self> {
        crate::services::tmux_common::with_tmux_source_authority(session, |_| {
            Self::snapshot(
                session,
                prompt,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: path.into(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(sid.into()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
                true,
            )
        })
    }

    fn snapshot(
        session: &str,
        prompt: &str,
        binding: TuiRuntimeBinding,
        allow_missing: bool,
    ) -> Option<Self> {
        let pane = Arc::new(PaneIdentity::from_binding(session, &binding)?);
        let sid = binding.session_id.clone()?;
        let (file, offset) = match open(&binding.output_path) {
            Ok(mut file) => {
                let offset = file.seek(SeekFrom::End(0)).ok()?;
                (Some(file), offset)
            }
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                (None, 0)
            }
            Err(_) => return None,
        };
        Some(Self {
            session: session.into(),
            pane,
            sid,
            file,
            offset,
            #[cfg(unix)]
            scanned: offset,
            #[cfg(unix)]
            partial: Vec::new(),
            end: None,
            ended: false,
            frame: prompt.replace("\r\n", "\n").replace('\r', "\n"),
        })
    }

    pub(crate) fn seal(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        crate::services::tmux_common::with_tmux_source_authority(&self.session, |authority| {
            let Some(current) =
                tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                    authority,
                )
            else {
                return;
            };
            if PaneIdentity::from_binding(&self.session, &current).as_ref()
                != Some(self.pane.as_ref())
            {
                return;
            }
            if self.file.is_none() {
                self.file = open(&current.output_path).ok();
            }
            let Some(file) = self.file.as_ref() else {
                return;
            };
            if same_file(file, &current.output_path) {
                self.end = file
                    .metadata()
                    .ok()
                    .map(|m| m.len())
                    .filter(|end| *end >= self.offset);
            }
        });
    }

    /// An external turn uses the native reader's sealed byte range and original file identity.
    #[cfg(unix)]
    pub(crate) fn from_native_range(
        session: &str,
        prompt: &str,
        sid: Option<&str>,
        range: (u64, u64),
        source: (u64, u64),
    ) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        if range.0 >= range.1 || range.1 - range.0 > 16 * 1024 * 1024 {
            return None;
        }
        let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(session)?;
        if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
            return None;
        }
        let mut file = open(&binding.output_path).ok()?;
        let metadata = file.metadata().ok()?;
        if (metadata.dev(), metadata.ino()) != source || metadata.len() < range.1 {
            return None;
        }
        let sid = if let Some(sid) = sid.filter(|sid| !sid.is_empty()) {
            sid.to_owned()
        } else {
            // A resumed reader may miss Init; only this pinned native input may supply its SID.
            file.seek(SeekFrom::Start(range.0)).ok()?;
            let mut found = None;
            for line in BufReader::new(file.take(range.1 - range.0)).lines() {
                let record: serde_json::Value = serde_json::from_str(&line.ok()?).ok()?;
                if user_text(&record)
                    .is_some_and(|text| super::super::busy_inject::exact_receipt(&text, prompt))
                {
                    let native = record["sessionId"].as_str().filter(|sid| !sid.is_empty())?;
                    if found.replace(native.to_owned()).is_some() {
                        return None;
                    }
                }
            }
            found?
        };
        let mut receipt = Self::before(session, prompt, Some(&sid))?;
        let metadata = receipt.file.as_ref()?.metadata().ok()?;
        if (metadata.dev(), metadata.ino()) != source || metadata.len() < range.1 {
            return None;
        }
        receipt.offset = range.0;
        receipt.scanned = range.0;
        receipt.end = Some(range.1);
        receipt.ended = true;
        Some(receipt)
    }

    #[cfg(unix)]
    pub(crate) fn native_pin_before_tail(
        session: &str,
        prompt: &str,
        sid: Option<&str>,
        end: u64,
    ) -> Option<crate::services::provider::native_stop::NativeUserPin> {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(session)?;
        let file = open(&binding.output_path).ok()?;
        let metadata = file.metadata().ok()?;
        let start = end.saturating_sub(16 * 1024 * 1024);
        if end == 0 || end > metadata.len() {
            return None;
        }
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact_at(&mut bytes, start).ok()?;
        if bytes.last() != Some(&b'\n') {
            return None;
        }
        let row_start = bytes[..bytes.len() - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(start, |n| start + n as u64 + 1);
        if start != 0 && row_start == start {
            return None;
        }
        let mut receipt = Self::from_native_range(
            session,
            prompt,
            sid,
            (row_start, end),
            (metadata.dev(), metadata.ino()),
        )?;
        let mut pin = receipt.try_pin_native_user().ok()??;
        if pin.range != (row_start, end) {
            return None;
        }
        pin.direct = true;
        Some(pin)
    }

    pub(crate) fn confirmed(&mut self) -> bool {
        let Some(end) = self.end else {
            return false;
        };
        let session = self.session.clone();
        crate::services::tmux_common::with_tmux_source_authority(&session, |authority| {
            let Some(current) =
                tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                    authority,
                )
            else {
                return false;
            };
            if PaneIdentity::from_binding(&self.session, &current).as_ref()
                != Some(self.pane.as_ref())
            {
                return false;
            }
            self.matches_through(end)
        })
    }

    fn matches_through(&mut self, end: u64) -> bool {
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        if !same_file(file, &self.pane.output_path)
            || !file.metadata().is_ok_and(|m| m.len() >= end)
            || file.seek(SeekFrom::Start(self.offset)).is_err()
        {
            return false;
        }
        let mut reader = BufReader::new((&mut *file).take(end - self.offset));
        let mut line = Vec::new();
        let mut read = 0;
        let mut matches = 0;
        loop {
            let Ok(n) = reader.read_until(b'\n', &mut line) else {
                return false;
            };
            if n == 0 {
                break;
            }
            read += n;
            if read > 16 * 1024 * 1024 {
                return false;
            }
            if !line.ends_with(b"\n") {
                return false;
            }
            let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
                return false;
            };
            if record["sessionId"].as_str() == Some(self.sid.as_str())
                && let Some(text) = user_text(&record)
                && super::super::busy_inject::exact_receipt(&text, &self.frame)
            {
                matches += 1;
            }
            line.clear();
        }
        matches == 1 && same_file(file, &self.pane.output_path)
    }
}

#[cfg(all(test, unix))]
thread_local! { static NATIVE_READ_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(all(test, unix))]
pub(crate) fn native_read_byte_total() -> u64 {
    NATIVE_READ_BYTES.with(std::cell::Cell::get)
}

#[cfg(unix)]
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(unix)]
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Re-adopt only the stored row on the same live pane, never search for another input.
pub(crate) fn revalidate_native_pin(
    pin: &crate::services::provider::native_stop::NativeUserPin,
) -> bool {
    #[cfg(not(unix))]
    {
        let _ = pin;
        false
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileExt, MetadataExt};
        crate::services::tmux_common::with_tmux_source_authority(&pin.tmux_session, |authority| {
            let validate = || -> Option<()> {
                let binding =
                    tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(
                        authority,
                    )?;
                let pane = PaneIdentity::from_binding(&pin.tmux_session, &binding)?;
                let SpawnNonceMarker::Known(nonce) = pane.nonce else {
                    return None;
                };
                if pin.spawn_nonce.as_deref() != Some(nonce.as_str())
                    || binding.runtime_kind != RuntimeHandoffKind::ClaudeTui
                    || binding.output_path != pin.output_path
                    || binding
                        .session_id
                        .as_deref()
                        .is_some_and(|sid| sid != pin.session_id)
                    || pin.uuid.is_empty()
                    || pin.session_id.is_empty()
                    || pin.range.0 < pin.receipt_start
                    || pin.range.0 >= pin.range.1
                    || pin.range.1 - pin.range.0 > 16 * 1024 * 1024
                {
                    return None;
                }
                let file = open(&pin.output_path).ok()?;
                let metadata = file.metadata().ok()?;
                if (metadata.dev(), metadata.ino()) != pin.source_file
                    || metadata.len() < pin.range.1
                {
                    return None;
                }
                let mut bytes = vec![0; (pin.range.1 - pin.range.0) as usize];
                file.read_exact_at(&mut bytes, pin.range.0).ok()?;
                if !bytes.ends_with(b"\n") || digest(&bytes) != pin.row_digest {
                    return None;
                }
                let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                let (_, uuid) =
                    tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                        &record,
                    )?;
                let body = normalize(&user_text(&record)?);
                if uuid.as_deref() != Some(pin.uuid.as_str())
                    || record["sessionId"].as_str() != Some(pin.session_id.as_str())
                    || !(digest(body.as_bytes()) == pin.frame_digest
                        || super::super::busy_inject::pasted_inner(&body)
                            .is_some_and(|inner| digest(inner.as_bytes()) == pin.frame_digest))
                {
                    return None;
                }
                same_file(&file, &pin.output_path).then_some(())
            };
            validate().is_some()
        })
    }
}

fn open(path: &str) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("transcript is not a regular file"));
    }
    Ok(file)
}

fn same_file(file: &File, path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (file.metadata(), std::fs::symlink_metadata(path)) {
            (Ok(a), Ok(b)) => b.is_file() && a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        false
    }
}

fn user_text(record: &serde_json::Value) -> Option<String> {
    if record["type"] != "user" || record["isMeta"] == true || record["isSidechain"] == true {
        return None;
    }
    let content = &record["message"]["content"];
    if let Some(text) = content.as_str() {
        return Some(text.into());
    }
    let blocks = content.as_array()?;
    blocks
        .iter()
        .map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten())
        .collect::<Option<Vec<_>>>()
        .map(|v| v.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::io::Write;

    #[test]
    #[cfg(unix)]
    fn a2_receipt_survives_real_handoff_and_rehydration() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handoff.jsonl");
        std::fs::write(&path, "").unwrap();
        let session = format!("a2-handoff-{}", uuid::Uuid::new_v4());
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("receipt-sid".into()),
            last_offset: 0,
            relay_last_offset: None,
        };
        tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding.clone());
        let mut receipt = UserReceipt::before(&session, "my input", None).unwrap();
        writeln!(
            OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            json!({"type":"user", "sessionId":"receipt-sid", "message":{"content":"my input"}})
        )
        .unwrap();
        let (sender, _rx) = std::sync::mpsc::channel();
        crate::services::claude::emit_claude_tui_watcher_handoff(
            &sender,
            path.to_str().unwrap(),
            &session,
            &path,
        );
        assert!(
            tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)
                .unwrap()
                .session_id
                .is_none()
        );
        receipt.seal();
        assert!(
            receipt.confirmed(),
            "same pane receipt survives actual handoff before seal"
        );
        tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
        assert!(
            receipt.confirmed(),
            "same pane receipt survives SID rehydration"
        );
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }

    #[test]
    #[cfg(unix)]
    fn a2_fresh_receipt_keeps_the_boundary_before_transcript_creation() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.jsonl");
        let session = format!("a2-fresh-receipt-{}", uuid::Uuid::new_v4());
        let mut receipt =
            UserReceipt::before_fresh(&session, "fresh input", path.to_str().unwrap(), "sid")
                .unwrap();
        assert!(!receipt.confirmed());
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user", "sessionId":"sid", "message":{"content":"fresh input"}})
            ),
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        assert!(
            !receipt.confirmed(),
            "an open attempt must not seal the terminal window"
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\"assistant\"}\n")
            .unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        receipt.seal();
        assert!(receipt.confirmed());
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }

    // Exact receipt confirmation requires Unix device/inode identity.
    #[cfg(unix)]
    #[test]
    fn a2_receipt_requires_one_new_exact_user_on_the_same_binding_and_file() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let frame = "원문\n첫째 줄 · 7314abcd";
        let user =
            |body: &str| json!({"type":"user", "sessionId":"sid", "message":{"content":body}});
        for (case, before, after, expected) in [
            ("exact", String::new(), format!("{}\n", user(frame)), true),
            ("old", format!("{}\n", user(frame)), String::new(), false),
            (
                "suffix",
                String::new(),
                format!("{}\n", user(&format!("{frame} extra"))),
                false,
            ),
            (
                "duplicate",
                String::new(),
                format!("{}\n{}\n", user(frame), user(frame)),
                false,
            ),
            ("partial", String::new(), user(frame).to_string(), false),
            (
                "assistant",
                String::new(),
                format!(
                    "{}\n",
                    json!({"type":"assistant","sessionId":"sid","message":{"content":frame}})
                ),
                false,
            ),
            (
                "other session",
                String::new(),
                format!(
                    "{}\n",
                    json!({"type":"user","sessionId":"other","message":{"content":frame}})
                ),
                false,
            ),
            (
                "nontext",
                String::new(),
                format!(
                    "{}\n",
                    json!({"type":"user","sessionId":"sid","message":{"content":[{"type":"text","text":frame},{"type":"image"}]}})
                ),
                false,
            ),
        ] {
            let session = format!("a2-receipt-{}", uuid::Uuid::new_v4());
            let path = dir.path().join(format!("{session}.jsonl"));
            std::fs::write(&path, before).unwrap();
            let binding = TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            };
            tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding.clone());
            let mut receipt = UserReceipt::before(&session, frame, None).expect("bound transcript");
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(after.as_bytes())
                .unwrap();
            receipt.seal();
            assert_eq!(receipt.confirmed(), expected, "{case}");
            if expected {
                let mut progressed = binding.clone();
                progressed.last_offset = std::fs::metadata(&path).unwrap().len();
                progressed.input_fifo_path = Some("compatibility-fifo".into());
                progressed.relay_output_path = Some("compatibility-relay".into());
                tui_prompt_dedupe::register_tmux_runtime_binding(&session, progressed);
                assert!(
                    receipt.confirmed(),
                    "learned transport aliases do not change the native input source"
                );
            }
            let replacement = path.with_extension("new");
            std::fs::write(&replacement, format!("{}\n", user(frame))).unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            assert!(
                !receipt.confirmed(),
                "a replaced inode cannot confirm the old attempt"
            );
            tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a2_windows_receipt_refuses_unproven_file_identity() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unproven-identity.jsonl");
        let session = format!("a2-unproven-identity-{}", uuid::Uuid::new_v4());
        let frame = "same-turn exact input";
        std::fs::write(&path, "").unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut receipt = UserReceipt::before(&session, frame, None).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user", "sessionId":"sid", "message":{"content":frame}})
            ),
        )
        .unwrap();
        receipt.seal();
        let confirmed = receipt.confirmed();
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        assert!(
            !confirmed,
            "an exact same-turn row cannot prove the file identity on Windows"
        );
    }

    #[test]
    fn a2_receipt_cannot_borrow_a_later_turn_identical_record() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.jsonl");
        let session = format!("a2-late-receipt-{}", uuid::Uuid::new_v4());
        std::fs::write(&path, "").unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut receipt = UserReceipt::before(&session, "same input", None).unwrap();
        receipt.seal();
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user", "sessionId":"sid", "message":{"content":"same input"}})
            ),
        )
        .unwrap();
        assert!(
            !receipt.confirmed(),
            "a later turn's identical user record is outside this attempt"
        );
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}
