//! Files live for the actual provider invocation, not the queue-delivery tick.
use super::uploads::Upload;

#[derive(Debug)]
pub(crate) struct MaterializedUploads {
    pub records: Vec<String>,
    files: Vec<MaterializedFile>,
    _directory: Option<super::temporary::Directory>,
}

#[derive(Debug)]
struct MaterializedFile {
    record: usize,
    path: std::path::PathBuf,
    filename: String,
    len: usize,
}

impl MaterializedUploads {
    pub(crate) fn has_materialized_files(&self) -> bool {
        !self.files.is_empty()
    }

    /// Copies private bundle files into an existing upload retention policy, keeping local records.
    pub(crate) fn copy_into(self, directory: &std::path::Path) -> Result<Vec<String>, String> {
        let Self {
            mut records,
            files,
            _directory,
        } = self;
        if files.is_empty() {
            return Ok(records);
        }
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        for file in files {
            let name = file.path.file_name().ok_or("attachment filename missing")?;
            let target = directory.join(format!(
                "{}_{}",
                uuid::Uuid::new_v4().simple(),
                name.to_string_lossy()
            ));
            let copied = std::fs::copy(&file.path, &target).map_err(|error| error.to_string())?;
            if copied != file.len as u64 {
                return Err("attachment copy length mismatch".into());
            }
            records[file.record] = format!(
                "[File uploaded] {} → {} ({} bytes)",
                serde_json::json!(file.filename),
                target.display(),
                file.len
            );
        }
        Ok(records)
    }

    #[cfg(test)]
    pub(crate) fn test_bundle(filename: &str, bytes: &[u8]) -> Self {
        let directory = super::temporary::Directory::new().expect("temporary bundle directory");
        let path = directory.path().join("0-fixture.txt");
        std::fs::write(&path, bytes).expect("temporary bundle bytes");
        Self {
            records: vec![format!(
                "[File uploaded] {} → {} ({} bytes)",
                serde_json::json!(filename),
                path.display(),
                bytes.len()
            )],
            files: vec![MaterializedFile {
                record: 0,
                path,
                filename: filename.into(),
                len: bytes.len(),
            }],
            _directory: Some(directory),
        }
    }
}

pub(crate) async fn prepare(
    uploads: &[Upload],
    pool: Option<&sqlx::PgPool>,
) -> Result<MaterializedUploads, String> {
    if uploads.len() > super::store::MAX_FILES {
        return Err("too many upload references".into());
    }
    let mut bundles = Vec::new();
    let mut total = 0;
    for upload in uploads {
        if let Upload::Bundle(reference) = upload {
            let bundle =
                super::store::load(pool.ok_or("attachment storage unavailable")?, reference)
                    .await
                    .map_err(|e| e.to_string())?;
            total += bundle
                .as_bundle()
                .entries
                .iter()
                .map(|entry| entry.bytes.len())
                .sum::<usize>();
            if total > super::store::MAX_BUNDLE_BYTES {
                return Err("merged attachments exceed 24 MiB".into());
            }
            bundles.push(bundle);
        }
    }
    let uploads = uploads.to_vec();
    tokio::task::spawn_blocking(move || {
        let directory = if bundles.is_empty() {
            None
        } else {
            Some(super::temporary::Directory::new().map_err(|e| e.to_string())?)
        };
        let mut records = Vec::new();
        let mut files = Vec::new();
        let mut bundles = bundles.into_iter();
        let mut ordinal = 0;
        for upload in uploads {
            match upload {
                Upload::Local(record) => records.push(record),
                Upload::Bundle(_) => {
                    let bundle = bundles.next().ok_or("attachment preparation mismatch")?;
                    let root = directory
                        .as_ref()
                        .ok_or("attachment directory missing")?
                        .path();
                    for entry in &bundle.as_bundle().entries {
                        // No client-controlled component enters a path. Retain only
                        // a short alphanumeric extension for image/tool detection.
                        let extension = entry
                            .filename
                            .rsplit_once('.')
                            .map(|(_, s)| s)
                            .filter(|s| {
                                s.len() <= 10 && s.bytes().all(|b| b.is_ascii_alphanumeric())
                            })
                            .map(|s| format!(".{s}"))
                            .unwrap_or_default();
                        let dest = root.join(format!("{ordinal}-{}{extension}", entry.sha256));
                        std::fs::write(&dest, &entry.bytes).map_err(|e| e.to_string())?;
                        files.push(MaterializedFile {
                            record: records.len(),
                            path: dest.clone(),
                            filename: entry.filename.clone(),
                            len: entry.bytes.len(),
                        });
                        records.push(format!(
                            "[File uploaded] {} → {} ({} bytes)",
                            serde_json::json!(entry.filename),
                            dest.display(),
                            entry.bytes.len()
                        ));
                        ordinal += 1;
                    }
                }
            }
        }
        Ok(MaterializedUploads {
            records,
            files,
            _directory: directory,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_bundle_files_outlive_temporary_materialization() {
        let mut prepared = MaterializedUploads::test_bundle("../quoted\".txt", b"bundle bytes");
        let original = prepared.files[0].path.clone();
        let local = "[File uploaded] local → /tmp/existing (1 bytes)".to_string();
        prepared.records.insert(0, local.clone());
        prepared.files[0].record = 1;
        let stable = tempfile::tempdir().expect("stable uploads directory");
        let records = prepared
            .copy_into(stable.path())
            .expect("copy completed bundle");
        assert!(
            !original.exists(),
            "private temporary directory is released"
        );
        let copied = std::fs::read_dir(stable.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read(&copied).unwrap(), b"bundle bytes");
        assert_eq!(
            records[0], local,
            "local upload record stays byte identical"
        );
        assert_eq!(
            records[1],
            format!(
                "[File uploaded] {} → {} (12 bytes)",
                serde_json::json!("../quoted\".txt"),
                copied.display()
            )
        );
    }
}
