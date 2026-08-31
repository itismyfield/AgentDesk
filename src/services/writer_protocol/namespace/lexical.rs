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
pub(super) struct SealedLexicalRoot(LexicalDialect, NormalizedAbsolute);

impl SealedLexicalRoot {
    fn register(dialect: LexicalDialect, bytes: &[u8]) -> Result<Self, LexicalError> {
        let normalized =
            normalize_absolute(dialect, bytes, true)?.ok_or(LexicalError::MalformedRoot)?;
        Ok(Self(dialect, normalized))
    }

    pub(super) fn normalize_candidate(
        &self,
        input: &[u8],
    ) -> Result<Option<NormalizedAbsolute>, LexicalError> {
        let candidate =
            normalize_absolute(self.0, input, false)?.filter(|value| self.contains(value));
        match candidate {
            None if two_separators(input) => Err(LexicalError::UnsupportedLexicalPrefix),
            candidate => Ok(candidate),
        }
    }

    pub(super) fn contains(&self, candidate: &NormalizedAbsolute) -> bool {
        candidate
            .0
            .strip_prefix(self.1.0.as_slice())
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(b"/"))
    }

    pub(super) fn overlaps(&self, other: &SealedLexicalRoot) -> bool {
        self.0 == other.0 && (self.contains(&other.1) || other.contains(&self.1))
    }
}

macro_rules! sealed_registrar {
    ($name:ident, $dialect:ident) => {
        pub(super) fn $name(bytes: &[u8]) -> Result<SealedLexicalRoot, LexicalError> {
            SealedLexicalRoot::register(LexicalDialect::$dialect, bytes)
        }
    };
}

sealed_registrar!(register_posix_exact_root, Posix);
sealed_registrar!(register_windows_drive_exact_root, WindowsDrive);
sealed_registrar!(register_windows_unc_exact_root, WindowsUnc);

fn normalize_absolute(
    dialect: LexicalDialect,
    input: &[u8],
    registering_root: bool,
) -> Result<Option<NormalizedAbsolute>, LexicalError> {
    let spelling = std::str::from_utf8(input).map_err(|_| LexicalError::NonUtf8)?;
    let bytes = spelling.as_bytes();
    if unsupported_namespace_prefix(bytes) || drive_relative(bytes) {
        return Err(LexicalError::UnsupportedLexicalPrefix);
    }
    let (prefix, rest, windows) = match dialect {
        LexicalDialect::Posix if spelling.starts_with('/') && !spelling.starts_with("//") => {
            ("/".to_string(), &spelling[1..], false)
        }
        LexicalDialect::WindowsDrive
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && is_windows_separator(bytes[2]) =>
        {
            (
                format!("{}:/", (bytes[0] as char).to_ascii_uppercase()),
                &spelling[3..],
                true,
            )
        }
        LexicalDialect::WindowsUnc if two_separators(bytes) => {
            ("//".to_string(), &spelling[2..], true)
        }
        LexicalDialect::Posix if spelling.starts_with("//") => {
            return Err(LexicalError::UnsupportedLexicalPrefix);
        }
        _ => return Ok(None),
    };
    let mut components = Vec::new();
    for component in rest.split(|char| char == '/' || (windows && char == '\\')) {
        match component {
            "" | "." => {}
            ".." => return Err(LexicalError::EscapesRoot),
            component => components.push(component),
        }
    }
    if dialect == LexicalDialect::WindowsUnc
        && (rest
            .as_bytes()
            .first()
            .is_some_and(|byte| is_windows_separator(*byte))
            || components.len() < 2
            || (registering_root && components.len() != 2))
    {
        let error = if registering_root {
            LexicalError::MalformedRoot
        } else {
            LexicalError::UnsupportedLexicalPrefix
        };
        return Err(error);
    }
    let normalized = format!("{prefix}{}", components.join("/")).into_bytes();
    Ok(Some(NormalizedAbsolute(normalized)))
}

fn unsupported_namespace_prefix(bytes: &[u8]) -> bool {
    [b'?', b'.'].into_iter().any(|marker| {
        bytes.len() >= 4
            && is_windows_separator(bytes[0])
            && is_windows_separator(bytes[1])
            && bytes[2] == marker
            && is_windows_separator(bytes[3])
    })
}

fn drive_relative(bytes: &[u8]) -> bool {
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !bytes.get(2).is_some_and(|byte| is_windows_separator(*byte))
}

fn is_windows_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

fn two_separators(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && is_windows_separator(bytes[0]) && is_windows_separator(bytes[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_normalizes(root: &SealedLexicalRoot, input: &[u8], expected: &[u8]) {
        assert_eq!(
            root.normalize_candidate(input),
            Ok(Some(NormalizedAbsolute(expected.to_vec())))
        );
    }

    fn assert_error(root: &SealedLexicalRoot, input: &[u8], error: LexicalError) {
        assert_eq!(root.normalize_candidate(input), Err(error), "{input:?}");
    }

    #[test]
    fn sealed_portable_roots_normalize_exactly() {
        assert_normalizes(
            &register_posix_exact_root(b"/runtime/sessions").unwrap(),
            b"/runtime//sessions/./relay",
            b"/runtime/sessions/relay",
        );
        assert_normalizes(
            &register_windows_drive_exact_root(br"C:\runtime\sessions").unwrap(),
            br"c:/runtime\sessions\.\relay",
            b"C:/runtime/sessions/relay",
        );
        assert_normalizes(
            &register_windows_unc_exact_root(br"\\server\share").unwrap(),
            br"\\server\share\\.\relay",
            b"//server/share/relay",
        );
    }

    #[test]
    fn unsupported_prefixes_and_escape_components_fail_closed() {
        let posix = register_posix_exact_root(b"/runtime").unwrap();
        let drive = register_windows_drive_exact_root(br"C:\runtime").unwrap();
        let unc = register_windows_unc_exact_root(br"\\server\share").unwrap();
        let unsupported = LexicalError::UnsupportedLexicalPrefix;
        for mask in 0_usize..8 {
            let separator = |bit: usize| [b'/', b'\\'][(mask >> bit) & 1];
            for marker in [b'?', b'.'] {
                let input = [separator(0), separator(1), marker, separator(2)];
                assert_error(&drive, &input, unsupported);
            }
        }
        assert_error(&drive, b"C:../x", unsupported);
        assert_error(&posix, b"//runtime/x", unsupported);
        assert_error(&posix, b"/runtime/../x", LexicalError::EscapesRoot);
        assert_error(&posix, b"//?/runtime/\xff", LexicalError::NonUtf8);
        assert_error(&unc, br"\\server", unsupported);
        assert_error(&unc, br"\\\share\x", unsupported);
        assert_error(&unc, br"\\other\share\x", unsupported);
        assert_error(&drive, br"\\other\share\x", unsupported);
        assert_eq!(
            register_windows_unc_exact_root(br"\\server\share\extra"),
            Err(LexicalError::MalformedRoot)
        );
    }

    #[test]
    fn normalized_candidates_preserve_case_separators_and_root_boundaries() {
        let posix = register_posix_exact_root(b"/Runtime").unwrap();
        let drive = register_windows_drive_exact_root(br"C:\Runtime").unwrap();
        let child = register_posix_exact_root(b"/Runtime/sessions").unwrap();
        let sibling = register_posix_exact_root(b"/RuntimeElse").unwrap();
        let posix_value = NormalizedAbsolute(b"/Runtime".to_vec());
        assert_normalizes(&posix, b"/Runtime", b"/Runtime");
        assert!(posix.contains(&posix_value));
        assert_normalizes(&posix, br"/Runtime/a\b", br"/Runtime/a\b");
        assert_normalizes(&drive, br"c:/Runtime\a", b"C:/Runtime/a");
        assert_eq!(drive.normalize_candidate(br"c:\runtime\a"), Ok(None));
        assert_eq!(posix.normalize_candidate(b"relative"), Ok(None));
        assert_normalizes(&posix, b"/Runtime/~/x", b"/Runtime/~/x");
        assert!(posix.overlaps(&child));
        assert!(!posix.overlaps(&sibling));
        assert!(!posix.contains(&NormalizedAbsolute(b"/RuntimeElse/x".to_vec())));
        let unc = register_windows_unc_exact_root(br"\\server\share").unwrap();
        assert!(!register_posix_exact_root(b"/").unwrap().overlaps(&unc));
    }
}
