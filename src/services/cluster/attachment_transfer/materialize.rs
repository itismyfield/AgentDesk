//! Files live for the actual provider invocation, not the queue-delivery tick.
use super::uploads::Upload;

#[derive(Debug)]
pub(crate) struct MaterializedUploads {
    pub records: Vec<String>,
    _directory: Option<super::temporary::Directory>,
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
            _directory: directory,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
