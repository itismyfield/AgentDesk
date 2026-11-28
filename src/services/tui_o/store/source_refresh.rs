//! A durable intent completes a same-file cursor rekey after interruption.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::rotation::{BOUNDARY_FILE, Rotation};
use super::spool::{self, source_key};
use super::{
    CURSOR_DIR, ChannelStore, INIT_FILE, LEDGER_FILE, SPOOL_DIR, StoreError, damage, durable,
    ledger,
};
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::tui_o::shadow::{SourceId, capture};

const INTENT: &str = "source_refresh";
const STAGE: &str = "source_refresh_stage";

#[derive(Serialize, Deserialize)]
struct Intent {
    old: String,
    new: String,
    segments: Vec<u64>,
}

impl Intent {
    fn spool_paths<'a>(&'a self, dir: &Path, key: &str) -> impl Iterator<Item = PathBuf> + use<'a> {
        let dir = dir.join(SPOOL_DIR);
        let key = key.to_owned();
        self.segments
            .iter()
            .map(move |start| dir.join(format!("{key}-{start:020}.seg")))
    }

    fn paths<'a>(&'a self, dir: &Path) -> impl Iterator<Item = (PathBuf, PathBuf)> + use<'a> {
        let cursor = |key: &str| dir.join(CURSOR_DIR).join(format!("{key}.json"));
        let unchanged = [INIT_FILE, BOUNDARY_FILE, LEDGER_FILE].map(|name| {
            let path = dir.join(name);
            (path.clone(), path)
        });
        unchanged
            .into_iter()
            .chain([(cursor(&self.old), cursor(&self.new))])
            .chain(
                self.spool_paths(dir, &self.old)
                    .zip(self.spool_paths(dir, &self.new)),
            )
    }
}

fn staged(dir: &Path, index: usize) -> PathBuf {
    dir.join(STAGE).join(format!("{index}.tmp"))
}

pub(super) fn recover(dir: &Path) -> Result<Option<durable::LockedFile>, StoreError> {
    let file = match File::open(dir) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let lock = durable::LockedFile::lock(file)?;
    finish(dir)?;
    let stage = dir.join(STAGE);
    if stage.try_exists()? {
        durable::sweep_tmp(&stage)?;
    }
    Ok(Some(lock))
}

pub(super) fn snapshot(dir: &Path) -> Result<Option<durable::LockedFile>, StoreError> {
    let file = match File::open(dir) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let lock = durable::LockedFile::try_shared(file)?;
    if dir.join(INTENT).try_exists()? {
        return Err(io::Error::from(io::ErrorKind::WouldBlock).into());
    }
    Ok(Some(lock))
}

fn finish(dir: &Path) -> Result<(), StoreError> {
    let path = dir.join(INTENT);
    let Some(intent) = durable::read_json::<Intent>(&path)? else {
        return Ok(());
    };
    let key = |key: &str| key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !key(&intent.old)
        || !key(&intent.new)
        || intent.old == intent.new
        || !intent.segments.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(damage("invalid source refresh intent"));
    }
    for (index, (_, target)) in intent.paths(dir).enumerate() {
        let stage = staged(dir, index);
        match fs::rename(&stage, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && target.is_file() => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(any(test, feature = "test-support"))]
        durable::fault::strike(&target, durable::fault::Step::RefreshInstall)?;
        fsync_parent_dir(&target)?;
        fsync_parent_dir(&stage)?;
    }
    for (previous, _) in intent.paths(dir).filter(|(old, new)| old != new) {
        durable::unlink_synced(&previous, true)?;
    }
    Ok(durable::unlink_synced(&path, false)?)
}

fn replace_source(value: &mut Value, old: &Value, new: &Value) -> bool {
    if value == old {
        value.clone_from(new);
        return true;
    }
    let mut changed = false;
    let mut visit = |item: &mut Value| changed |= replace_source(item, old, new);
    match value {
        Value::Array(items) => items.iter_mut().for_each(&mut visit),
        Value::Object(fields) => fields.values_mut().for_each(&mut visit),
        _ => {}
    }
    changed
}

fn project_line(
    output: &mut impl Write,
    line: &[u8],
    (from, to): (&Value, &Value),
    (old, new): (&SourceId, &SourceId),
) -> Result<(), StoreError> {
    use ledger::input_notices::MigrationSource;

    let mut entry: Value = serde_json::from_slice(line)?;
    if let Some(cohort) = entry.pointer_mut("/entry/entry/snapshot/sources") {
        let mut sources: Vec<MigrationSource> = serde_json::from_value(cohort.clone())?;
        if let (Some(old_index), Some(new_index)) = (
            sources.iter().position(|source| source.source == *old),
            sources.iter().position(|source| source.source == *new),
        ) {
            let other = sources.remove(new_index);
            let kept = &mut sources[old_index - usize::from(new_index < old_index)];
            if kept.lower > other.upper || other.lower > kept.upper {
                return Err(damage("source refresh has disjoint notice cohorts"));
            }
            if other.lower < kept.lower {
                (kept.lower, kept.prefix_hash) = (other.lower, other.prefix_hash);
            }
            kept.upper = kept.upper.max(other.upper);
            *cohort = serde_json::to_value(sources)?;
        }
    }
    if replace_source(&mut entry, from, to) {
        serde_json::to_writer(&mut *output, &entry)?;
        output.write_all(b"\n")?;
    } else {
        output.write_all(line)?;
    }
    Ok(())
}

fn remap_rotation(
    mut rotation: Rotation,
    old: &SourceId,
    new: &SourceId,
    keys: &Intent,
) -> Rotation {
    let remap = |source: &mut SourceId| {
        if source == old {
            source.clone_from(new)
        }
    };
    for link in rotation.links.values_mut() {
        remap(&mut link.source);
        link.parent.iter_mut().for_each(remap);
        link.hot_candidate
            .iter_mut()
            .for_each(|candidate| remap(&mut candidate.parent));
    }
    for successor in rotation.successors.values_mut() {
        remap(&mut successor.source);
    }
    if let Some(link) = rotation.links.remove(&keys.old) {
        rotation.links.insert(keys.new.clone(), link);
    }
    if let Some(successor) = rotation.successors.remove(&keys.old) {
        rotation.successors.insert(keys.new.clone(), successor);
    }
    let mut devices = rotation.devices.remove(&keys.old).unwrap_or_default();
    devices.extend([old.dev, new.dev]);
    devices.sort_unstable();
    devices.dedup();
    rotation.devices.insert(keys.new.clone(), devices);
    rotation
}

impl ChannelStore {
    pub(in crate::services::tui_o) fn refresh_source(
        &mut self,
        old: &SourceId,
        new: &SourceId,
    ) -> Result<(), StoreError> {
        self.mutate(|store| {
            let (old_key, new_key) = (source_key(old), source_key(new));
            let spool = store
                .sources
                .get(&old_key)
                .ok_or_else(|| damage("refresh cursor missing"))?;
            if !capture::same_file(old, new)
                || store.sources.contains_key(&new_key)
                || store.ledger.violation().is_some()
            {
                return Err(damage("invalid source refresh"));
            }
            let mut cursor = store
                .cursor(old)
                .ok_or_else(|| damage("refresh cursor missing"))?
                .clone();
            if cursor.transition_cleanup.is_some() {
                return Err(damage("cannot refresh a retired cursor"));
            }
            cursor.source = new.clone();
            let intent = Intent {
                old: old_key,
                new: new_key,
                segments: spool.segment_starts().collect(),
            };
            let dir = &store.dir;
            let _publication = durable::LockedFile::lock(File::open(dir)?)?;
            let stage = dir.join(STAGE);
            durable::ensure_dir(&stage)?;
            durable::sweep_tmp(&stage)?;
            let ledger = durable::LockedFile::try_lock(File::open(dir.join(LEDGER_FILE))?)?;
            let (from, to) = (serde_json::to_value(old)?, serde_json::to_value(new)?);
            for (index, (source, _)) in intent.paths(dir).enumerate() {
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(staged(dir, index))?;
                let mut output = BufWriter::new(file);
                match index {
                    0 => {
                        let mut init: Value = durable::read_json(&source)?
                            .ok_or_else(|| damage("refresh init missing"))?;
                        replace_source(&mut init, &from, &to);
                        serde_json::to_writer(&mut output, &init)?;
                    }
                    1 => serde_json::to_writer(
                        &mut output,
                        &remap_rotation(store.rotation()?, old, new, &intent),
                    )?,
                    2 => {
                        let mut reader = BufReader::new(&*ledger);
                        let mut line = Vec::new();
                        while reader.read_until(b'\n', &mut line)? != 0 {
                            if line.last() != Some(&b'\n') {
                                return Err(damage("unfinished ledger during refresh"));
                            }
                            project_line(&mut output, &line, (&from, &to), (old, new))?;
                            line.clear();
                        }
                    }
                    3 => serde_json::to_writer(&mut output, &cursor)?,
                    _ => {
                        let mut reader = BufReader::new(File::open(source)?);
                        let mut header = Vec::new();
                        reader.read_until(b'\n', &mut header)?;
                        let mut header: Value = serde_json::from_slice(&header)?;
                        replace_source(&mut header, &from, &to);
                        serde_json::to_writer(&mut output, &header)?;
                        output.write_all(b"\n")?;
                        io::copy(&mut reader, &mut output)?;
                    }
                }
                output.flush()?;
                output.get_ref().sync_all()?;
            }
            let init = durable::read_json(&staged(dir, 0))?
                .ok_or_else(|| damage("staged init missing"))?;
            let projected =
                ledger::read_only(&mut File::open(staged(dir, 2))?, store.init.initial_anchor)?;
            if projected.violation().is_some() {
                return Err(damage("source refresh changes ledger validity"));
            }
            let file = fs::metadata(&new.path)?;
            if capture::file_identity(&file) != (new.dev, new.ino) {
                return Err(damage("transcript moved during source refresh"));
            }
            fsync_parent_dir(&staged(dir, 0))?;
            #[cfg(any(test, feature = "test-support"))]
            durable::fault::strike(&dir.join(INTENT), durable::fault::Step::RefreshBeforeIntent)?;
            durable::create_once(&dir.join(INTENT), &serde_json::to_vec(&intent)?)?;
            finish(dir)?;
            store.sources = spool::recover_sources(dir, &init, &projected)?;
            store.ledger = projected;
            store.init = init;
            Ok(())
        })
    }
}
