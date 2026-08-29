use uuid::Uuid;

macro_rules! canonical_uuid {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(super) struct $name(String);

        impl $name {
            pub(super) fn parse(text: &str) -> Result<Self, ValueError> {
                let value = Uuid::parse_str(text).map_err(|_| ValueError)?;
                (value.hyphenated().to_string().as_bytes() == text.as_bytes())
                    .then(|| Self(text.to_owned()))
                    .ok_or(ValueError)
            }

            pub(super) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

canonical_uuid!(RequestIdV2);
canonical_uuid!(AttemptIdV2);

macro_rules! text_identity {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(super) struct $name(String);

        impl $name {
            pub(super) fn parse(text: &str) -> Result<Self, ValueError> {
                (!text.is_empty() && !text.contains('\0'))
                    .then(|| Self(text.to_owned()))
                    .ok_or(ValueError)
            }

            pub(super) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

text_identity!(ProviderIdentityV2);
text_identity!(ChannelIdentityV2);
text_identity!(NonceV2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SafeRelativeRefV2(String);

impl SafeRelativeRefV2 {
    pub(super) fn parse(text: &str) -> Result<Self, ValueError> {
        if text.is_empty()
            || text.contains(['\\', '\0'])
            || text.ends_with('/')
            || has_drive_prefix(text)
            || text.split('/').any(unsafe_component)
        {
            return Err(ValueError);
        }
        Ok(Self(text.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

fn has_drive_prefix(text: &str) -> bool {
    matches!(text.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic())
}

fn unsafe_component(component: &str) -> bool {
    if component.is_empty() || matches!(component, "." | "..") || component.ends_with(['.', ' ']) {
        return true;
    }
    let basename = component.split('.').next().unwrap_or("");
    let upper = basename.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || reserved_numbered(&upper, "COM")
        || reserved_numbered(&upper, "LPT")
}

fn reserved_numbered(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ValueError;

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = "123e4567-e89b-12d3-a456-426614174000";
    const M: &str = "123e4567-e89b-12d3-a456-426614174001";

    #[test]
    fn canonical_uuid_forms_only() {
        assert_eq!(RequestIdV2::parse(R).unwrap().as_str(), R);
        assert_eq!(AttemptIdV2::parse(M).unwrap().as_str(), M);
        for rejected in [
            "123E4567-E89B-12D3-A456-426614174000",
            "123e4567e89b12d3a456426614174000",
            "{123e4567-e89b-12d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-12d3-a456-426614174000",
        ] {
            assert!(RequestIdV2::parse(rejected).is_err(), "accepted {rejected}");
        }
    }

    #[test]
    fn identities_preserve_exact_unicode_and_reject_empty_or_nul() {
        let composed = "Cláude/β";
        let decomposed = "Cla\u{301}ude/β";
        let provider = ProviderIdentityV2::parse(composed).unwrap();
        let channel = ChannelIdentityV2::parse(decomposed).unwrap();
        let nonce = NonceV2::parse("Nonce-한글").unwrap();
        assert_eq!(provider.as_str().as_bytes(), composed.as_bytes());
        assert_eq!(channel.as_str().as_bytes(), decomposed.as_bytes());
        assert_ne!(provider.as_str().as_bytes(), channel.as_str().as_bytes());
        assert_eq!(nonce.as_str(), "Nonce-한글");
        assert!(ProviderIdentityV2::parse("").is_err());
        assert!(ChannelIdentityV2::parse("bad\0channel").is_err());
        assert!(NonceV2::parse("").is_err());
        assert!(NonceV2::parse("bad\0nonce").is_err());
    }

    #[test]
    fn safe_relative_reference_accepts_nested_portable_components() {
        assert!(SafeRelativeRefV2::parse("/absolute").is_err());
        for valid in [
            "proofs/terminal.json",
            "receipts/한글/result-01.txt",
            "com10/lpt0.txt",
            "console.txt",
        ] {
            assert_eq!(SafeRelativeRefV2::parse(valid).unwrap().as_str(), valid);
        }
    }

    #[test]
    fn safe_relative_reference_rejects_each_unsafe_class() {
        for invalid in [
            "",
            "/absolute",
            "//server/share",
            "C:/drive",
            "c:drive",
            "a\\b",
            "nul\0byte",
            "a//b",
            ".",
            "..",
            "a/./b",
            "a/../b",
            "a/",
            "name.",
            "name ",
            "a/name.",
            "CON",
            "con.txt",
            "PRN.log",
            "AUX",
            "NUL.data",
            "COM1",
            "com9.txt",
            "LPT1",
            "lpt9.log",
        ] {
            assert!(
                SafeRelativeRefV2::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }
}
