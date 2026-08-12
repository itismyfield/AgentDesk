use std::fmt;
use std::str::FromStr;

/// Stable domain statuses for intake outbox rows.
///
/// The outbound status spelling returned by [`Self::as_str`] and the diagnostic
/// Rust-variant spelling returned by [`Self::variant_name`] are each defined by
/// their respective wildcard-free exhaustive match. Classifications are defined
/// only by the wildcard-free exhaustive matches in [`Self::is_open`] and
/// [`Self::operator_retry`].
///
/// VALIDATES:
/// - The compiler requires every added variant to be handled by those four
///   exhaustive matches. Renaming a variant also invalidates direct references
///   in [`Self::ALL`] and [`Self::from_str`].
/// - The `[Self; 9]` type of [`Self::ALL`] makes the initializer's cardinality a
///   compile-time check.
/// - Unit tests check [`Self::ALL`] membership against the enum declaration,
///   [`Self::from_str`] against [`Self::as_str`], and [`Self::is_open`] against
///   [`INTAKE_OUTBOX_OPEN_STATUSES_SQL`][open-statuses]. They also check
///   [`Self::ALL`] against the 0107 status CHECK body, every current
///   [`Self::operator_retry`] classification, and that rejected input is
///   preserved in [`UnknownIntakeStatus`]. The `Unknown` variant is the
///   official terminal spelling; `UnknownIntakeStatus` represents rejected,
///   unregistered spellings such as `future_status`.
///
/// DOES NOT VALIDATE:
/// - The compiler does not require [`Self::from_str`] to gain an arm for an
///   added variant because its match has an unknown-input arm. That coverage and
///   the members (rather than the cardinality) of [`Self::ALL`] are test-backed.
/// - The strong-enum `sqlx::Type` derive generates `Type`, `Encode`, and `Decode`
///   implementations, and `rename_all` determines their codec spelling.
///   Production writes bind this enum directly, and `IntakeOutboxRow` decodes
///   its status through SQLx. A pinned PostgreSQL test exercises every
///   variant's encode and decode and pins the raw spelling against
///   [`Self::as_str`].
///
/// LIMITS:
/// - No gate prevents replacing an exhaustive classification arm with `_ =>`;
///   preserving wildcard-free matches is a review convention.
/// - The source-based [`Self::ALL`] membership test reads only this file and
///   fails closed on unsupported enum syntax, but no independent gate proves
///   that its defining equality assertion remains present.
/// - The migration-source test pins the CHECK domain while the PostgreSQL codec
///   test pins wire spelling. SQL string literals and direct writers outside
///   the typed coordinates remain possible, and typed rows do not replace the
///   database CHECK. SQLx decoding does not call [`Self::from_str`].
///
/// [open-statuses]: crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum IntakeOutboxStatus {
    Pending,
    Claimed,
    Accepted,
    Spawned,
    Dispatched,
    Unknown,
    Done,
    FailedPreAccept,
    FailedPostAccept,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperatorRetryClass {
    ForceTerminate,
    AlreadyTerminal,
    Refuse,
}

impl IntakeOutboxStatus {
    pub(crate) const ALL: [Self; 9] = [
        Self::Pending,
        Self::Claimed,
        Self::Accepted,
        Self::Spawned,
        Self::Dispatched,
        Self::Unknown,
        Self::Done,
        Self::FailedPreAccept,
        Self::FailedPostAccept,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Accepted => "accepted",
            Self::Spawned => "spawned",
            Self::Dispatched => "dispatched",
            Self::Unknown => "unknown",
            Self::Done => "done",
            Self::FailedPreAccept => "failed_pre_accept",
            Self::FailedPostAccept => "failed_post_accept",
        }
    }

    pub(crate) const fn variant_name(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Claimed => "Claimed",
            Self::Accepted => "Accepted",
            Self::Spawned => "Spawned",
            Self::Dispatched => "Dispatched",
            Self::Unknown => "Unknown",
            Self::Done => "Done",
            Self::FailedPreAccept => "FailedPreAccept",
            Self::FailedPostAccept => "FailedPostAccept",
        }
    }

    pub(crate) const fn is_open(self) -> bool {
        match self {
            Self::Pending | Self::Claimed | Self::Accepted | Self::Spawned | Self::Dispatched => {
                true
            }
            Self::Unknown | Self::Done | Self::FailedPreAccept | Self::FailedPostAccept => false,
        }
    }

    pub(crate) const fn operator_retry(self) -> OperatorRetryClass {
        match self {
            Self::Accepted | Self::Spawned => OperatorRetryClass::ForceTerminate,
            Self::Unknown | Self::FailedPostAccept => OperatorRetryClass::AlreadyTerminal,
            Self::Pending
            | Self::Claimed
            | Self::Dispatched
            | Self::Done
            | Self::FailedPreAccept => OperatorRetryClass::Refuse,
        }
    }
}

impl fmt::Display for IntakeOutboxStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown intake outbox status: {0}")]
pub(crate) struct UnknownIntakeStatus(String);

impl FromStr for IntakeOutboxStatus {
    type Err = UnknownIntakeStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "accepted" => Ok(Self::Accepted),
            "spawned" => Ok(Self::Spawned),
            "dispatched" => Ok(Self::Dispatched),
            "unknown" => Ok(Self::Unknown),
            "done" => Ok(Self::Done),
            "failed_pre_accept" => Ok(Self::FailedPreAccept),
            "failed_post_accept" => Ok(Self::FailedPostAccept),
            unknown => Err(UnknownIntakeStatus(unknown.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{IntakeOutboxStatus, OperatorRetryClass, UnknownIntakeStatus};
    use crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const MODULE_SOURCE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", file!()));

    fn repo_source(path: &str) -> String {
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"))
    }

    fn quoted_sql_values(sql_list: &str) -> BTreeSet<&str> {
        sql_list
            .split(',')
            .map(|value| value.trim().trim_matches('\''))
            .filter(|value| !value.is_empty())
            .collect()
    }

    fn declared_variant_names(source: &str) -> BTreeSet<&str> {
        let body = source
            .split_once("enum IntakeOutboxStatus {")
            .expect("source must declare IntakeOutboxStatus")
            .1
            .split_once("\n}")
            .expect("IntakeOutboxStatus must have a closing brace")
            .0;

        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty()
                    || line.starts_with("//")
                    || line.starts_with("/*")
                    || line.starts_with('*')
                    || line.starts_with("#[")
                {
                    return None;
                }
                line.strip_suffix(',')
                    .filter(|name| name.chars().all(|ch| ch == '_' || ch.is_alphanumeric()))
            })
            .collect()
    }

    #[test]
    fn all_exactly_matches_declared_variants() {
        let declared = declared_variant_names(MODULE_SOURCE);
        let all: BTreeSet<_> = IntakeOutboxStatus::ALL
            .map(IntakeOutboxStatus::variant_name)
            .into_iter()
            .collect();
        assert_eq!(all, declared);

        let decorated = MODULE_SOURCE.replacen(
            "    Pending,",
            "    /// Parser-resilience probe.\n    #[doc = \"pending\"]\n    Pending,",
            1,
        );
        assert_eq!(declared_variant_names(&decorated), declared);
    }

    #[test]
    fn open_classification_matches_shared_sql_list() {
        let classified: BTreeSet<_> = IntakeOutboxStatus::ALL
            .into_iter()
            .filter(|status| status.is_open())
            .map(IntakeOutboxStatus::as_str)
            .collect();
        assert_eq!(
            classified,
            quoted_sql_values(INTAKE_OUTBOX_OPEN_STATUSES_SQL)
        );
    }

    #[test]
    fn all_exactly_matches_migration_status_check() {
        let migration = repo_source("migrations/postgres/0107_intake_outbox_dispatched_clock.sql");
        let check_values = migration
            .split_once("ADD CONSTRAINT intake_outbox_status_check CHECK (status IN (")
            .expect("migration must add intake_outbox_status_check")
            .1
            .split_once(")) NOT VALID;")
            .expect("status CHECK must remain NOT VALID")
            .0;
        let all: BTreeSet<_> = IntakeOutboxStatus::ALL
            .map(IntakeOutboxStatus::as_str)
            .into_iter()
            .collect();
        assert_eq!(all, quoted_sql_values(check_values));
    }

    #[test]
    fn status_strings_round_trip() {
        for status in IntakeOutboxStatus::ALL {
            assert_eq!(status.as_str().parse(), Ok(status));
            assert_eq!(format!("{status:>20}"), format!("{:>20}", status.as_str()));
        }
    }

    #[test]
    fn unknown_status_preserves_rejected_spelling() {
        let unknown = "waiting_for_operator";
        assert_eq!(
            unknown.parse::<IntakeOutboxStatus>(),
            Err(UnknownIntakeStatus(unknown.to_owned()))
        );
    }

    #[test]
    fn operator_retry_classifies_every_status() {
        let expected = [
            (IntakeOutboxStatus::Pending, OperatorRetryClass::Refuse),
            (IntakeOutboxStatus::Claimed, OperatorRetryClass::Refuse),
            (
                IntakeOutboxStatus::Accepted,
                OperatorRetryClass::ForceTerminate,
            ),
            (
                IntakeOutboxStatus::Spawned,
                OperatorRetryClass::ForceTerminate,
            ),
            (IntakeOutboxStatus::Dispatched, OperatorRetryClass::Refuse),
            (
                IntakeOutboxStatus::Unknown,
                OperatorRetryClass::AlreadyTerminal,
            ),
            (IntakeOutboxStatus::Done, OperatorRetryClass::Refuse),
            (
                IntakeOutboxStatus::FailedPreAccept,
                OperatorRetryClass::Refuse,
            ),
            (
                IntakeOutboxStatus::FailedPostAccept,
                OperatorRetryClass::AlreadyTerminal,
            ),
        ];
        assert_eq!(expected.map(|(status, _)| status), IntakeOutboxStatus::ALL);
        for (status, class) in expected {
            assert_eq!(status.operator_retry(), class);
        }
    }
}
