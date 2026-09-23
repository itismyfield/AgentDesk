//! Durable, conservative receipts for AgentDesk's Memento remember writer.
//!
//! A successful receipt has no TTL. A pending receipt is deliberately *not* a
//! successful duplicate: after an interrupted/ambiguous write, a caller must
//! reconcile the result instead of submitting the same write again. Receipts
//! contain no memory text or credentials and never modify Memento's database.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const PENDING: u8 = b'P';
const CONFIRMED: u8 = b'C';

/// The held file lock spans the complete remote write. Never unlink receipt
/// files: a contender may already have opened the same inode before locking it.
#[derive(Debug)]
pub(crate) struct WriterClaim {
    file: File,
}

impl WriterClaim {
    /// `None` means a previous write was confirmed successful. In-flight and
    /// unresolved writes return errors, so callers cannot report false success.
    pub(crate) fn acquire(directory: &Path, key: &str) -> Result<Option<Self>, String> {
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("memento writer receipt requires a SHA-256 key".to_string());
        }
        fs::create_dir_all(directory)
            .map_err(|error| format!("memento writer receipt directory: {error}"))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let receipt = directory.join(format!("{}.receipt", key.to_ascii_lowercase()));
        let mut file = options
            .open(&receipt)
            .map_err(|error| format!("memento writer receipt open: {error}"))?;
        file.try_lock().map_err(|error| {
            format!("memento remember is in flight or its receipt cannot be locked: {error}")
        })?;
        let mut state = Vec::new();
        (&mut file)
            .take(2)
            .read_to_end(&mut state)
            .map_err(|error| format!("memento writer receipt read: {error}"))?;
        match state.as_slice() {
            [CONFIRMED] => return Ok(None),
            [] => {}
            _ => {
                return Err(
                    "memento remember has an unresolved prior write; reconcile its result before retrying the same payload"
                        .to_string(),
                );
            }
        }

        let mut claim = Self { file };
        claim.write_state(PENDING)?;
        // Persist the directory entry before the request is allowed to leave.
        // Also persist creation of the receipt directory itself on first use.
        #[cfg(unix)]
        {
            sync_parent_directory(&receipt)?;
            if directory
                .parent()
                .is_some_and(|path| !path.as_os_str().is_empty())
            {
                sync_parent_directory(directory)?;
            }
        }
        Ok(Some(claim))
    }

    /// Call only after an unambiguous remote success. Dropping a claim without
    /// completing it leaves a pending receipt and blocks unsafe retries.
    pub(crate) fn complete(mut self) -> Result<(), String> {
        self.write_state(CONFIRMED)
    }

    /// The request provably never left the client (for example, MCP session
    /// initialization failed). This is the only safe automatic release path.
    pub(crate) fn release_before_send(self) -> Result<(), String> {
        self.file
            .set_len(0)
            .and_then(|()| self.file.sync_all())
            .map_err(|error| format!("memento writer receipt release: {error}"))
    }

    fn write_state(&mut self, state: u8) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.write_all(&[state]))
            .and_then(|()| self.file.sync_all())
            .map_err(|error| format!("memento writer receipt persist: {error}"))
    }
}

/// Flushes the directory holding `entry` through the single cross-platform helper.
#[cfg(unix)]
fn sync_parent_directory(entry: &Path) -> Result<(), String> {
    crate::services::discord::runtime_store::fsync_parent_dir(entry)
        .map_err(|error| format!("memento writer receipt directory persist: {error}"))
}

/// Preserve every semantic/scope field, including future fields, rather than
/// relying on a fixed allowlist that could silently discard new fact metadata.
/// Only source and importance vary without changing the remembered knowledge.
/// Endpoint and credential identity isolate separate stores/tenants.
pub(crate) fn remember_fingerprint(endpoint: &str, auth_identity: &str, args: &Value) -> String {
    let mut canonical_args = canonical_value(args);
    if let Some(object) = canonical_args.as_object_mut() {
        object.remove("source");
        object.remove("importance");
    }
    // Structured serialization avoids delimiter collisions in user text.
    let identity = serde_json::json!([
        "agentdesk-memento-remember-v1",
        normalized_endpoint(endpoint),
        format!("{:x}", Sha256::digest(auth_identity.as_bytes())),
        canonical_args,
    ]);
    format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
}

fn normalized_endpoint(endpoint: &str) -> &str {
    let endpoint = endpoint.trim().trim_end_matches('/');
    endpoint.strip_suffix("/mcp").unwrap_or(endpoint)
}

fn generation_path(directory: &Path, endpoint: &str) -> std::path::PathBuf {
    directory.join(format!(
        "{:x}.generation",
        Sha256::digest(normalized_endpoint(endpoint).as_bytes())
    ))
}

/// Keep exact content bytes: normalization belongs to the caller that actually
/// sends normalized text. Direct MCP code/string facts can depend on whitespace.
/// A mutation changes the store generation so old receipts cannot suppress a
/// legitimate remember after amend/forget. Old in-flight completions remain in
/// their old generation and never restore stale suppression.
pub(crate) fn writer_fingerprint(
    directory: &Path,
    endpoint: &str,
    auth_identity: &str,
    args: &Value,
) -> Result<String, String> {
    let path = generation_path(directory, endpoint);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let generation = match options.open(path) {
        Ok(file) => {
            let mut generation = String::new();
            file.take(33)
                .read_to_string(&mut generation)
                .map_err(|e| e.to_string())?;
            if generation.len() != 32 || !generation.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid memento writer generation".into());
            }
            generation
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "initial".into(),
        Err(error) => return Err(error.to_string()),
    };
    let fingerprint = remember_fingerprint(endpoint, auth_identity, args);
    Ok(format!(
        "{:x}",
        Sha256::digest(format!("{generation}:{fingerprint}").as_bytes())
    ))
}

/// Local receipt metadata only; this never contacts or mutates Memento.
/// Atomic replacement also supports concurrent mutation hooks without torn reads.
/// Invalidate the entire endpoint because a privileged maintenance credential
/// can consolidate facts written under other client credentials. Fingerprints
/// still isolate credentials: this can permit extra writes, never merge tenants.
pub(crate) fn invalidate_writer_receipts(
    directory: &Path,
    endpoint: &str,
    _auth_identity: &str,
) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let generation = format!("{:032x}", rand::random::<u128>());
    let temporary = directory.join(format!(".generation-{generation}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
    let result = file
        .write_all(generation.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, generation_path(directory, endpoint)))
        .map_err(|e| e.to_string());
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    #[cfg(unix)]
    sync_parent_directory(&generation_path(directory, endpoint))?;
    Ok(())
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_value(&object[key])))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fingerprint(args: Value) -> String {
        remember_fingerprint("https://memento.example/mcp", "test-only-identity", &args)
    }

    fn fact() -> Value {
        json!({"content": "윤호의 생일은 3월 5일", "type": "fact", "workspace": "family", "caseId": "yunho"})
    }

    #[test]
    fn confirmed_receipt_survives_reopen_without_a_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        WriterClaim::acquire(dir.path(), &key)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        for _ in 0..3 {
            assert!(WriterClaim::acquire(dir.path(), &key).unwrap().is_none());
        }
    }

    #[test]
    fn ambiguous_write_is_an_error_not_a_successful_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        drop(WriterClaim::acquire(dir.path(), &key).unwrap().unwrap());
        let error = WriterClaim::acquire(dir.path(), &key).unwrap_err();
        assert!(error.contains("unresolved prior write"));
        // A newly learned fact remains writable even while the old one is
        // unresolved; there is no global poison flag or family-content filter.
        let new_key = fingerprint(
            json!({"content": "윤호의 생일은 3월 6일", "type": "fact", "workspace": "family", "caseId": "yunho"}),
        );
        WriterClaim::acquire(dir.path(), &new_key)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
    }

    #[test]
    fn no_send_release_allows_a_safe_retry() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        WriterClaim::acquire(dir.path(), &key)
            .unwrap()
            .unwrap()
            .release_before_send()
            .unwrap();
        WriterClaim::acquire(dir.path(), &key)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        assert!(WriterClaim::acquire(dir.path(), &key).unwrap().is_none());
    }

    #[test]
    fn competing_handle_cannot_cross_an_active_claim() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        let claim = WriterClaim::acquire(dir.path(), &key).unwrap().unwrap();
        let error = WriterClaim::acquire(dir.path(), &key).unwrap_err();
        assert!(error.contains("in flight"));
        claim.complete().unwrap();
        assert!(WriterClaim::acquire(dir.path(), &key).unwrap().is_none());
    }

    #[test]
    fn competing_process_cannot_cross_an_active_claim() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        let claim = WriterClaim::acquire(dir.path(), &key).unwrap().unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::subprocess_claim_probe"),
                "--nocapture",
            ])
            .env("ADK_WRITER_GUARD_TEST_DIRECTORY", dir.path())
            .env("ADK_WRITER_GUARD_TEST_KEY", &key)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        claim.complete().unwrap();
    }

    #[test]
    fn subprocess_claim_probe() {
        let Some(directory) = std::env::var_os("ADK_WRITER_GUARD_TEST_DIRECTORY") else {
            return;
        };
        let key = std::env::var("ADK_WRITER_GUARD_TEST_KEY").unwrap();
        let error = WriterClaim::acquire(Path::new(&directory), &key).unwrap_err();
        assert!(error.contains("in flight"), "{error}");
    }

    #[test]
    fn attribution_retries_have_the_same_fingerprint_but_content_bytes_are_preserved() {
        let first = json!({"content": "family\n  fact", "source": "turn-1", "importance": 0.2, "type": "fact"});
        let second = json!({"content": "family\n  fact", "source": "turn-2", "importance": 0.9, "type": "fact"});
        assert_eq!(fingerprint(first), fingerprint(second));
        assert_ne!(
            fingerprint(json!({"content":"a  b"})),
            fingerprint(json!({"content":"a b"}))
        );
    }

    #[test]
    fn new_knowledge_scope_and_fact_metadata_are_never_discarded() {
        let original = fact();
        let original_key = fingerprint(original.clone());
        for (field, value) in [
            ("content", "윤호의 생일은 3월 6일"),
            ("workspace", "personal"),
            ("caseId", "another-person"),
            ("agentId", "family-agent"),
            ("type", "preference"),
            ("topic", "birthday"),
            ("assertionStatus", "confirmed"),
            ("resolutionStatus", "resolved"),
            ("outcome", "new fact confirmed"),
            ("phase", "verified"),
            ("goal", "remember birthday"),
            ("contextSummary", "parent confirmed the corrected date"),
            ("futureFactMetadata", "new information"),
        ] {
            let mut changed = original.clone();
            changed[field] = json!(value);
            assert_ne!(original_key, fingerprint(changed), "lost {field}");
        }
    }

    #[test]
    fn endpoint_and_authentication_identity_isolate_receipts() {
        let args = fact();
        let key = remember_fingerprint("endpoint-a", "identity-a", &args);
        assert_ne!(key, remember_fingerprint("endpoint-b", "identity-a", &args));
        assert_ne!(key, remember_fingerprint("endpoint-a", "identity-b", &args));
    }

    #[test]
    fn mutation_generation_invalidates_completed_and_in_flight_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let key = writer_fingerprint(dir.path(), "https://memory.test", "tenant", &fact()).unwrap();
        assert_eq!(
            key,
            writer_fingerprint(dir.path(), "https://memory.test/mcp/", "tenant", &fact()).unwrap()
        );
        let old_claim = WriterClaim::acquire(dir.path(), &key).unwrap().unwrap();
        let other_tenant =
            writer_fingerprint(dir.path(), "https://memory.test", "other", &fact()).unwrap();
        invalidate_writer_receipts(dir.path(), "https://memory.test/mcp", "tenant").unwrap();
        old_claim.complete().unwrap();
        let new_key =
            writer_fingerprint(dir.path(), "https://memory.test", "tenant", &fact()).unwrap();
        assert_ne!(key, new_key);
        assert_ne!(
            other_tenant,
            writer_fingerprint(dir.path(), "https://memory.test", "other", &fact()).unwrap()
        );
        assert_ne!(
            new_key,
            writer_fingerprint(dir.path(), "https://memory.test", "other", &fact()).unwrap()
        );
        WriterClaim::acquire(dir.path(), &new_key)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        assert!(
            WriterClaim::acquire(dir.path(), &new_key)
                .unwrap()
                .is_none()
        );
        invalidate_writer_receipts(dir.path(), "https://memory.test", "tenant").unwrap();
        let after_mutation =
            writer_fingerprint(dir.path(), "https://memory.test", "tenant", &fact()).unwrap();
        assert!(
            WriterClaim::acquire(dir.path(), &after_mutation)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn fingerprint_is_canonical_and_has_no_delimiter_collisions() {
        let first: Value =
            serde_json::from_str(r#"{"content":"fact","metadata":{"a":1,"b":2}}"#).unwrap();
        let second: Value =
            serde_json::from_str(r#"{"metadata":{"b":2,"a":1},"content":"fact"}"#).unwrap();
        assert_eq!(fingerprint(first), fingerprint(second));
        assert_ne!(
            fingerprint(json!({"content":"a", "topic":"b\u{1f}c"})),
            fingerprint(json!({"content":"a\u{1f}b", "topic":"c"})),
        );
    }

    #[test]
    fn receipt_contains_no_memory_text_or_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        WriterClaim::acquire(dir.path(), &key)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        let entries = fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].file_name().to_string_lossy(),
            format!("{key}.receipt")
        );
        assert_eq!(fs::read(entries[0].path()).unwrap(), b"C");
    }

    #[test]
    fn corrupt_receipt_fails_closed_and_invalid_key_cannot_escape_directory() {
        let dir = tempfile::tempdir().unwrap();
        let key = fingerprint(fact());
        fs::write(dir.path().join(format!("{key}.receipt")), b"Cgarbage").unwrap();
        assert!(
            WriterClaim::acquire(dir.path(), &key)
                .unwrap_err()
                .contains("unresolved")
        );
        assert!(WriterClaim::acquire(dir.path(), "../escape").is_err());
    }
}
