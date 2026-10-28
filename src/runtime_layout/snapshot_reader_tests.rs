use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

const INITIAL: &str = "memory: {backend: memento, file: {sak_path: snapshot/sak}}";
const DISK: &str = "memory: {backend: file, file: {sak_path: disk/sak}}";

#[test]
fn memory_backend_uses_same_root_snapshot_until_reload() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write(INITIAL);
        assert_eq!(load_memory_backend(fixture.root()).backend, "memento");
        fixture.install(INITIAL, DISK);
        let current = load_memory_backend(fixture.root());
        assert_eq!(current.backend, "memento");
        assert_eq!(current.file.sak_path, "snapshot/sak");
        fixture.reload();
        let reloaded = load_memory_backend(fixture.root());
        assert_eq!(reloaded.backend, "file");
        assert_eq!(reloaded.file.sak_path, "disk/sak");
    });
}

#[test]
fn memory_backend_snapshot_without_memory_keeps_json_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install("server: {}", INITIAL);
        let canonical = memory_backend_path(fixture.root());
        std::fs::write(
            &canonical,
            r#"{"backend":"file","file":{"sak_path":"canonical/sak"}}"#,
        )
        .unwrap();
        let legacy = fixture.root().join("memory-backend.json");
        std::fs::write(&legacy, r#"{"backend":"memento","sak_path":"legacy/sak"}"#).unwrap();
        let current = load_memory_backend(fixture.root());
        assert_eq!(current.backend, "file");
        assert_eq!(current.file.sak_path, "canonical/sak");
        std::fs::write(&canonical, "not JSON").unwrap();
        let legacy_config = load_memory_backend(fixture.root());
        assert_eq!(legacy_config.backend, "memento");
        assert_eq!(legacy_config.file.sak_path, "legacy/sak");
        std::fs::remove_file(&canonical).unwrap();
        std::fs::remove_file(&legacy).unwrap();
        assert_eq!(
            load_memory_backend(fixture.root()),
            MemoryBackendConfig::default()
        );
    });
}

#[test]
fn memory_backend_other_root_keeps_independent_yaml_and_json() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(INITIAL, DISK);
        let other = tempfile::tempdir().unwrap();
        let path = config_file_path(other.path());
        fixture.write_at(&path, DISK);
        let independent = load_memory_backend(other.path());
        assert_eq!(independent.backend, "file");
        assert_eq!(independent.file.sak_path, "disk/sak");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            memory_backend_path(other.path()),
            r#"{"backend":"memento","file":{"sak_path":"other/sak"}}"#,
        )
        .unwrap();
        let fallback = load_memory_backend(other.path());
        assert_eq!(fallback.backend, "memento");
        assert_eq!(fallback.file.sak_path, "other/sak");
    });
}

#[test]
fn memory_backend_without_snapshot_preserves_invalid_yaml_json_and_defaults() {
    isolated(|| {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.path.parent().unwrap()).unwrap();
        std::fs::write(&fixture.path, "server: [").unwrap();
        let fallback = memory_backend_path(fixture.root());
        std::fs::write(
            &fallback,
            r#"{"backend":"local","file":{"sak_path":"json/sak"}}"#,
        )
        .unwrap();
        let loaded = load_memory_backend(fixture.root());
        assert_eq!(loaded.backend, "file");
        assert_eq!(loaded.file.sak_path, "json/sak");
        std::fs::write(&fallback, "not JSON").unwrap();
        assert_eq!(
            load_memory_backend(fixture.root()),
            MemoryBackendConfig::default()
        );
    });
}
