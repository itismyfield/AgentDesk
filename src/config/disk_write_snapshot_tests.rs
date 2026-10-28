use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_source_metadata_is_excluded_from_write_roundtrip() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41}");
        let expected = crate::config::load_from_path(&fixture.path).unwrap();
        assert!(expected.runtime_source.is_some());
        let rendered = serde_yaml::to_string(&expected).unwrap();
        check_reads_back(&expected, &rendered).unwrap();
        crate::config::save_to_path(&fixture.path, &expected).unwrap();
        assert_eq!(
            crate::config::load_from_path(&fixture.path)
                .unwrap()
                .server
                .port,
            41
        );
    });
}

#[test]
fn snapshot_source_normalization_reads_back_disk_document_with_secrets() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41, auth_token: synthetic-secret}");
        let expected = crate::config::load_from_path(&fixture.path).unwrap();
        let rendered = serde_yaml::to_string(&disk_document(&expected).unwrap()).unwrap();
        check_reads_back(&expected, &rendered).unwrap();
    });
}

#[test]
fn snapshot_source_normalization_rejects_changed_values_with_secrets_kept() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41, auth_token: synthetic-secret}");
        let expected = crate::config::load_from_path(&fixture.path).unwrap();
        let rendered = serde_yaml::to_string(&disk_document(&expected).unwrap()).unwrap();
        check_reads_back(&expected, &rendered).unwrap();
        let mut changed = expected.clone();
        changed.server.port = 73;
        let rendered = serde_yaml::to_string(&disk_document(&changed).unwrap()).unwrap();
        let actual: Config = serde_yaml::from_str(&rendered).unwrap();
        assert_eq!(actual.server.auth_token, expected.server.auth_token);
        let error = check_reads_back(&expected, &rendered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("typed section server"), "{error}");
        assert!(!error.contains("synthetic-secret"));
    });
}

#[test]
fn snapshot_source_normalization_rejects_missing_secrets_with_values_unchanged() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41, auth_token: synthetic-secret}");
        let expected = crate::config::load_from_path(&fixture.path).unwrap();
        let rendered = serde_yaml::to_string(&disk_document(&expected).unwrap()).unwrap();
        check_reads_back(&expected, &rendered).unwrap();
        let mut stripped = expected.clone();
        stripped.server.auth_token = None;
        let rendered = serde_yaml::to_string(&disk_document(&stripped).unwrap()).unwrap();
        let actual: Config = serde_yaml::from_str(&rendered).unwrap();
        assert_eq!(actual.server.port, expected.server.port);
        let error = check_reads_back(&expected, &rendered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("typed section server"), "{error}");
        assert!(!error.contains("synthetic-secret"));
    });
}
