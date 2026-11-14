//! Boot consumes only complete, digest-checked native copies from the existing custody tree.
use super::*;
use crate::services::tui_input::human_frame::history::v2::native::{self, RestoredRecord};

pub(in crate::services::discord) fn persist_native_boot_record(
    root: &Path,
    record: &RestoredRecord,
) -> Result<(), String> {
    persist_record(root, record, true)
}

fn persist_record(root: &Path, record: &RestoredRecord, blocking: bool) -> Result<(), String> {
    let provider = ProviderKind::Claude;
    let ticket = &record.settlement.ticket;
    let message = ticket
        .owning_episode
        .as_ref()
        .map(|e| e.user_message_id)
        .or_else(|| ticket.key["identity"]["primary"].as_u64())
        .ok_or("native custody identity missing")?;
    let custody = root.join("discord_custody/claude");
    let _lock = super::super::custody_notice::retention::lock(&custody, blocking)
        .map_err(|e| e.to_string())?;
    let key = episode_key(&provider, [ticket.channel_id, message], None);
    let dir = custody.join(sha(key.to_string().as_bytes()));
    preserve_episode(
        root,
        &dir,
        key,
        vec![Item(
            "native_input",
            root.join("injected_frames.json"),
            serde_json::to_vec(record).map_err(|e| e.to_string()),
            None,
            None,
        )],
        runtime_store::process_generation_binding().generation,
    )
}

pub(in crate::services::discord) fn preserve_native_before_discard(
    path: &Path,
) -> Result<(), String> {
    if path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        != Some("claude")
    {
        return Ok(());
    }
    let root = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("native discard root missing")?;
    let channel = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or("native discard channel missing")?;
    for value in native::boot_records(&root.join("injected_frames.json"), "claude")? {
        let record = RestoredRecord::decode(value)?;
        if record.settlement.ticket.channel_id == channel {
            persist_record(root, &record, false)?;
        }
    }
    Ok(())
}

pub(in crate::services::discord) fn native_boot_records(
    root: &Path,
    provider: &ProviderKind,
) -> Result<Vec<RestoredRecord>, String> {
    let mut records = native::boot_records(&root.join("injected_frames.json"), provider.as_str())?
        .into_iter()
        .map(RestoredRecord::decode)
        .collect::<Result<Vec<_>, _>>()?;
    let custody = root.join("discord_custody").join(provider.as_str());
    let _lock =
        super::super::custody_notice::retention::lock(&custody, true).map_err(|e| e.to_string())?;
    for episode in fs::read_dir(&custody).map_err(|e| e.to_string())? {
        let episode = episode.map_err(|e| e.to_string())?;
        if !episode.file_type().map_err(|e| e.to_string())?.is_dir() {
            return Err("native custody episode is not a directory".into());
        }
        let mut revisions = fs::read_dir(episode.path())
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        revisions.sort_by_key(|entry| entry.file_name());
        for revision in revisions {
            if !revision.file_name().to_string_lossy().starts_with("rev-") {
                continue;
            }
            if !revision.file_type().map_err(|e| e.to_string())?.is_dir() {
                return Err("native custody revision is not a directory".into());
            }
            let manifest = revision.path().join("manifest.json");
            if !manifest.exists() {
                let files = fs::read_dir(revision.path())
                    .map_err(|e| e.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                if files.iter().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .ends_with("-native_input.json")
                }) {
                    return Err("native custody manifest missing".into());
                }
                continue;
            }
            if !fs::symlink_metadata(&manifest)
                .map_err(|e| e.to_string())?
                .is_file()
            {
                return Err("native custody manifest is not a file".into());
            }
            let value: Value =
                serde_json::from_slice(&fs::read(manifest).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let entries = value["entries"]
                .as_array()
                .ok_or("native custody entries missing")?;
            for entry in entries.iter().filter(|e| e["kind"] == "native_input") {
                if value["complete"] != true || !entry["error"].is_null() {
                    return Err("native custody copy incomplete".into());
                }
                let name = entry["copy"]
                    .as_str()
                    .ok_or("native custody copy missing")?;
                if Path::new(name).components().count() != 1
                    || !name.ends_with("-native_input.json")
                {
                    return Err("native custody copy path invalid".into());
                }
                let copy = revision.path().join(name);
                if !fs::symlink_metadata(&copy)
                    .map_err(|e| e.to_string())?
                    .is_file()
                {
                    return Err("native custody copy is not a file".into());
                }
                let bytes = fs::read(copy).map_err(|e| e.to_string())?;
                if entry["sha256"].as_str() != Some(sha(&bytes).as_str()) {
                    return Err("native custody digest mismatch".into());
                }
                let record = RestoredRecord::decode(
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
                )?;
                let ticket = &record.settlement.ticket;
                let message = ticket
                    .owning_episode
                    .as_ref()
                    .map(|e| e.user_message_id)
                    .or_else(|| ticket.key["identity"]["primary"].as_u64())
                    .ok_or("native custody identity missing")?;
                let key = episode_key(provider, [ticket.channel_id, message], None);
                if ticket.key["attempt"]["provider"] != provider.as_str()
                    || episode.file_name().to_str()
                        != Some(sha(key.to_string().as_bytes()).as_str())
                {
                    return Err("native custody episode mismatch".into());
                }
                records.push(record);
            }
        }
    }
    Ok(records)
}
