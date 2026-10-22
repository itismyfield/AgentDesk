use super::*;

/// A query error fails every channel; a row that cannot decode fails only its own channel, and
/// an unreadable channel column fails every channel with no decoded row.
#[test]
fn bulk_session_rows_isolate_decode_failures() {
    let rows = vec![("201", false), ("202", true)];
    let read = |row: &(&str, bool), field: &str| match (field, row.1) {
        ("thread_channel_id", _) => Ok(Some(row.0.to_string())),
        ("status", true) => Err(sqlx::Error::ColumnDecode {
            index: field.into(),
            source: std::io::Error::other("injected decode failure").into(),
        }),
        ("status", false) => Ok(Some("idle".to_string())),
        _ => Ok(None),
    };
    let lookups = map_session_rows(&[201, 202, 203], Ok(rows.clone()), read);
    assert!(
        matches!(&lookups[&201], Ok(Some(session)) if session.status.as_deref() == Some("idle"))
    );
    assert!(
        lookups[&202]
            .as_ref()
            .unwrap_err()
            .contains("decode session status")
    );
    assert!(matches!(lookups[&203], Ok(None)));

    let failed = map_session_rows(
        &[201, 203],
        Err::<Vec<(&str, bool)>, _>("query channel sessions: boom".into()),
        read,
    );
    assert!(
        failed
            .values()
            .all(|lookup| lookup.as_ref().is_err_and(|e| e.contains("boom")))
    );

    let keyless = |row: &(&str, bool), field: &str| match field {
        "thread_channel_id" if row.1 => Err(sqlx::Error::ColumnDecode {
            index: field.into(),
            source: std::io::Error::other("injected key failure").into(),
        }),
        _ => read(row, field),
    };
    let unattributed = map_session_rows(&[201, 202, 203], Ok(rows), keyless);
    assert!(unattributed[&201].is_ok());
    for channel in [202, 203] {
        assert!(
            unattributed[&channel]
                .as_ref()
                .unwrap_err()
                .contains("thread_channel_id")
        );
    }
}
