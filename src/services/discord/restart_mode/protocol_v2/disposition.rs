#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecodeDisposition<T, C, P> {
    Accepted { value: T, raw: Box<[u8]> },
    Conflict { reason: C, raw: Box<[u8]> },
    Poison { reason: P, raw: Box<[u8]> },
    Unsupported { raw: Box<[u8]> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_disposition_owns_exact_original_bytes() {
        let raw: &[u8] = b"\0raw\xff bytes";
        let dispositions: [DecodeDisposition<u8, &str, &str>; 4] = [
            DecodeDisposition::Accepted {
                value: 7,
                raw: raw.into(),
            },
            DecodeDisposition::Conflict {
                reason: "conflict",
                raw: raw.into(),
            },
            DecodeDisposition::Poison {
                reason: "poison",
                raw: raw.into(),
            },
            DecodeDisposition::Unsupported { raw: raw.into() },
        ];
        for disposition in dispositions {
            let owned = match disposition {
                DecodeDisposition::Accepted { raw, .. }
                | DecodeDisposition::Conflict { raw, .. }
                | DecodeDisposition::Poison { raw, .. }
                | DecodeDisposition::Unsupported { raw } => raw,
            };
            assert_eq!(&*owned, raw);
            assert_ne!(owned.as_ptr(), raw.as_ptr());
        }
    }
}
