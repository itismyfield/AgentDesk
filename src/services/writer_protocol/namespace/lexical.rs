//! Host-independent lexical roots for dormant writer authority.
//!
//! This module performs no filesystem, environment, home, or runtime lookup.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LexicalDialect {
    Posix,
    WindowsDrive,
    WindowsUnc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LexicalError {
    NonUtf8,
    UnsupportedLexicalPrefix,
    EscapesRoot,
    MalformedRoot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NormalizedAbsolute(Vec<u8>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SealedLexicalRoot {
    dialect: LexicalDialect,
    normalized: NormalizedAbsolute,
}

impl SealedLexicalRoot {
    fn register(dialect: LexicalDialect, bytes: &[u8]) -> Result<Self, LexicalError> {
        std::str::from_utf8(bytes).map_err(|_| LexicalError::NonUtf8)?;
        Ok(Self {
            dialect,
            normalized: NormalizedAbsolute(bytes.to_vec()),
        })
    }

    pub(super) fn normalize_candidate(
        &self,
        input: &[u8],
    ) -> Result<Option<NormalizedAbsolute>, LexicalError> {
        std::str::from_utf8(input).map_err(|_| LexicalError::NonUtf8)?;
        Ok(Some(NormalizedAbsolute(input.to_vec())))
    }

    pub(super) fn contains(&self, candidate: &NormalizedAbsolute) -> bool {
        contains_bytes(&self.normalized.0, &candidate.0)
    }

    pub(super) fn overlaps(&self, other: &SealedLexicalRoot) -> bool {
        self.dialect == other.dialect
            && (contains_bytes(&self.normalized.0, &other.normalized.0)
                || contains_bytes(&other.normalized.0, &self.normalized.0))
    }
}

pub(super) fn register_posix_exact_root(
    bytes: &[u8],
) -> Result<SealedLexicalRoot, LexicalError> {
    SealedLexicalRoot::register(LexicalDialect::Posix, bytes)
}

pub(super) fn register_windows_drive_exact_root(
    bytes: &[u8],
) -> Result<SealedLexicalRoot, LexicalError> {
    SealedLexicalRoot::register(LexicalDialect::WindowsDrive, bytes)
}

pub(super) fn register_windows_unc_exact_root(
    bytes: &[u8],
) -> Result<SealedLexicalRoot, LexicalError> {
    SealedLexicalRoot::register(LexicalDialect::WindowsUnc, bytes)
}

fn contains_bytes(root: &[u8], candidate: &[u8]) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(bytes: &[u8]) -> Option<NormalizedAbsolute> {
        Some(NormalizedAbsolute(bytes.to_vec()))
    }

    #[test]
    fn sealed_portable_roots_normalize_exactly() {
        let cases = [
            (
                register_posix_exact_root(b"/runtime/sessions").unwrap(),
                b"/runtime//sessions/./relay".as_slice(),
                b"/runtime/sessions/relay".as_slice(),
            ),
            (
                register_windows_drive_exact_root(br"C:\runtime\sessions").unwrap(),
                br"c:/runtime\sessions\.\relay".as_slice(),
                b"C:/runtime/sessions/relay".as_slice(),
            ),
            (
                register_windows_unc_exact_root(br"\\server\share").unwrap(),
                br"\\server\share\\.\relay".as_slice(),
                b"//server/share/relay".as_slice(),
            ),
        ];
        for (root, input, normalized) in cases {
            assert_eq!(root.normalize_candidate(input), Ok(expected(normalized)));
        }
    }
}
