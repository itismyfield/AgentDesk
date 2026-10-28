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
fn snapshot_source_normalization_still_rejects_changed_values_and_secrets() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41, auth_token: synthetic-secret}");
        let expected = crate::config::load_from_path(&fixture.path).unwrap();
        let mut changed = expected.clone();
        changed.server.port = 73;
        let error = check_reads_back(&expected, &serde_yaml::to_string(&changed).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("typed section server"), "{error}");
        assert!(!error.contains("synthetic-secret"));
        let mut stripped = expected.clone();
        stripped.server.auth_token = None;
        let error = check_reads_back(&expected, &serde_yaml::to_string(&stripped).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("typed section server"), "{error}");
    });
}
