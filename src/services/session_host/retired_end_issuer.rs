use super::RetiredEnd;
use crate::services::session_host::legacy_return::issuer_key::ObservationKey;

pub(crate) fn issue<T>(key: ObservationKey, record: T) -> RetiredEnd<T> {
    let ObservationKey { .. } = key;
    RetiredEnd(record)
}
