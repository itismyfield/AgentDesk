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
pub(super) struct NormalizedAbsolute(LexicalDialect, Vec<u8>);

impl NormalizedAbsolute {
    fn contains(&self, candidate: &Self) -> bool {
        self.0 == candidate.0
            && candidate.1.strip_prefix(&self.1[..]).is_some_and(|suffix| {
                suffix.is_empty() || self.1.ends_with(b"/") || suffix.starts_with(b"/")
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SealedLexicalRoot(NormalizedAbsolute);

impl SealedLexicalRoot {
    fn register(dialect: LexicalDialect, bytes: &[u8]) -> Result<Self, LexicalError> {
        let normalized =
            normalize_absolute(dialect, bytes, true)?.ok_or(LexicalError::MalformedRoot)?;
        Ok(Self(normalized))
    }

    pub(super) fn normalize_candidate(
        &self,
        input: &[u8],
    ) -> Result<Option<NormalizedAbsolute>, LexicalError> {
        let candidate =
            normalize_absolute(self.0.0, input, false)?.filter(|value| self.contains(value));
        match candidate {
            None if two_separators(input) => Err(LexicalError::UnsupportedLexicalPrefix),
            candidate => Ok(candidate),
        }
    }

    pub(super) fn contains(&self, candidate: &NormalizedAbsolute) -> bool {
        self.0.contains(candidate)
    }

    pub(super) fn overlaps(&self, other: &SealedLexicalRoot) -> bool {
        self.0.contains(&other.0) || other.0.contains(&self.0)
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
    if unsupported_namespace_prefix(input) || drive_relative(input) {
        return Err(LexicalError::UnsupportedLexicalPrefix);
    }
    let (prefix, rest, windows) = match dialect {
        LexicalDialect::Posix if spelling.starts_with('/') && !spelling.starts_with("//") => {
            ("/".to_string(), &spelling[1..], false)
        }
        LexicalDialect::WindowsDrive
            if input.len() >= 3
                && input[0].is_ascii_alphabetic()
                && input[1] == b':'
                && is_windows_separator(input[2]) =>
        {
            (
                format!("{}:/", (input[0] as char).to_ascii_uppercase()),
                &spelling[3..],
                true,
            )
        }
        LexicalDialect::WindowsUnc if two_separators(input) => {
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
        let error = registering_root
            .then_some(LexicalError::MalformedRoot)
            .unwrap_or(LexicalError::UnsupportedLexicalPrefix);
        return Err(error);
    }
    let normalized = format!("{prefix}{}", components.join("/")).into_bytes();
    Ok(Some(NormalizedAbsolute(dialect, normalized)))
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
    use LexicalDialect::{Posix as P, WindowsDrive as D, WindowsUnc as U};

    struct LexicalFixtures([SealedLexicalRoot; 5]);
    fn named_roots() -> LexicalFixtures {
        LexicalFixtures([
            register_posix_exact_root(b"/Runtime").unwrap(),
            register_windows_drive_exact_root(br"C:\Runtime").unwrap(),
            register_windows_unc_exact_root(br"\\server\share").unwrap(),
            register_posix_exact_root(b"/").unwrap(),
            register_windows_drive_exact_root(b"C:/").unwrap(),
        ])
    }
    fn v(dialect: LexicalDialect, bytes: &[u8]) -> NormalizedAbsolute {
        NormalizedAbsolute(dialect, bytes.to_vec())
    }
    fn assert_n(root: &SealedLexicalRoot, input: &[u8], expected: NormalizedAbsolute) {
        assert_eq!(root.normalize_candidate(input), Ok(Some(expected)));
    }

    fn assert_e(root: &SealedLexicalRoot, input: &[u8], error: LexicalError) {
        assert_eq!(root.normalize_candidate(input), Err(error), "{input:?}");
    }

    #[test]
    fn sealed_portable_roots_normalize_exactly() {
        let [p, d, u, ..] = named_roots().0;
        assert_n(&p, b"/Runtime//./x", v(P, b"/Runtime/x"));
        assert_n(&d, br"c:/Runtime\.\x", v(D, b"C:/Runtime/x"));
        assert_n(&u, br"\\server\share\\.\x", v(U, b"//server/share/x"));
    }

    #[test]
    fn unsupported_prefixes_and_escape_components_fail_closed() {
        let [p, d, u, ..] = named_roots().0;
        let unsupported = LexicalError::UnsupportedLexicalPrefix;
        for mask in 0_usize..8 {
            let separator = |bit: usize| [b'/', b'\\'][(mask >> bit) & 1];
            for marker in [b'?', b'.'] {
                let input = [separator(0), separator(1), marker, separator(2)];
                assert_e(&d, &input, unsupported);
            }
        }
        assert_e(&d, b"C:../x", unsupported);
        assert_e(&p, b"//Runtime/x", unsupported);
        assert_e(&p, b"/Runtime/../x", LexicalError::EscapesRoot);
        assert_e(&p, b"//?/Runtime/\xff", LexicalError::NonUtf8);
        assert_e(&u, br"\\server", unsupported);
        assert_e(&u, br"\\\share\x", unsupported);
        assert_e(&u, br"\\other\share\x", unsupported);
        assert_e(&d, br"\\other\share\x", unsupported);
        let malformed = register_windows_unc_exact_root(br"\\server\share\extra");
        assert_eq!(malformed, Err(LexicalError::MalformedRoot));
    }

    #[test]
    fn normalized_candidates_preserve_case_separators_and_root_boundaries() {
        let [p, d, u, pf, df] = named_roots().0;
        assert_n(&p, b"/Runtime", v(P, b"/Runtime"));
        assert!(p.contains(&v(P, b"/Runtime")));
        assert_n(&p, br"/Runtime/a\b", v(P, br"/Runtime/a\b"));
        assert_n(&d, br"c:/Runtime\a", v(D, b"C:/Runtime/a"));
        assert_eq!(d.normalize_candidate(br"c:\runtime\a"), Ok(None));
        assert_eq!(p.normalize_candidate(b"relative"), Ok(None));
        assert_n(&p, b"/Runtime/~/x", v(P, b"/Runtime/~/x"));
        assert!(p.overlaps(&register_posix_exact_root(b"/Runtime/sessions").unwrap()));
        assert!(!p.overlaps(&register_posix_exact_root(b"/RuntimeElse").unwrap()));
        assert!(!p.contains(&v(P, b"/RuntimeElse/x")));
        assert_n(&pf, b"/", v(P, b"/"));
        assert!(pf.contains(&v(P, b"/child")));
        assert!(pf.overlaps(&register_posix_exact_root(b"/child").unwrap()));
        assert_n(&df, b"c:/", v(D, b"C:/"));
        assert!(df.contains(&v(D, b"C:/child")));
        assert!(df.overlaps(&register_windows_drive_exact_root(b"C:/child").unwrap()));
        assert!(!pf.contains(&v(U, b"/")));
        assert!(!pf.contains(&v(U, b"//server/share")));
        assert!(!pf.overlaps(&u));
    }
}
