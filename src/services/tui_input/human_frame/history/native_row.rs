use super::*;

pub(crate) struct NativeRow {
    pub(crate) record: serde_json::Value,
    pub(crate) file: [u64; 2],
    pub(crate) range: (u64, u64),
    pub(crate) digest: String,
}

pub(crate) fn read_native_row(
    path: &Path,
    source: [u64; 2],
    offset: u64,
    cap: u64,
    historical: bool,
) -> Option<NativeRow> {
    use std::io::{BufRead, Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let before = file.metadata().ok()?;
    let identity = file_id(&before);
    if !before.is_file()
        || identity[1] != source[1]
        || (!historical && identity[0] != source[0])
        || offset >= before.len()
    {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut raw = Vec::new();
    std::io::BufReader::new(file.take(cap.checked_add(1)?))
        .read_until(b'\n', &mut raw)
        .ok()?;
    let end = offset.checked_add(raw.len() as u64)?;
    let current = std::fs::symlink_metadata(path).ok()?;
    if raw.len() as u64 > cap
        || raw.last() != Some(&b'\n')
        || !current.is_file()
        || file_id(&current) != identity
        || current.len() < end
    {
        return None;
    }
    Some(NativeRow {
        record: serde_json::from_slice(&raw).ok()?,
        file: identity,
        range: (offset, end),
        digest: blake3::hash(&raw).to_hex().to_string(),
    })
}

pub(crate) fn native_prompt(record: &serde_json::Value, sid: &str) -> Option<String> {
    let uuid = record["uuid"].as_str()?;
    if sid.is_empty()
        || record["sessionId"] != sid
        || record["isMeta"] == true
        || record["isCompactSummary"] == true
        || record["isSidechain"] == true
        || uuid.is_empty()
        || uuid != uuid.trim()
    {
        return None;
    }
    match record["type"].as_str()? {
        "user" => {
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                record,
            )?;
            taken_text(record)
        }
        "attachment"
            if record["attachment"]["type"] == "queued_command"
                && record["attachment"]["commandMode"] == "prompt" =>
        {
            let text = record["attachment"]["prompt"].as_str()?;
            class_inner(text)
                .unwrap_or(text.trim())
                .starts_with("[User: ")
                .then(|| text.to_owned())
        }
        _ => None,
    }
}
