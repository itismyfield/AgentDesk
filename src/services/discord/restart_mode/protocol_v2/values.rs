use uuid::Uuid;

macro_rules! canonical_uuid {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(super) struct $name(String);

        impl $name {
            pub(super) fn mint_v4() -> Self {
                Self(Uuid::new_v4().hyphenated().to_string())
            }

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
        if text.is_empty() || text.ends_with('/') || text.split('/').any(unsafe_component) {
            return Err(ValueError);
        }
        Ok(Self(text.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

fn unsafe_component(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.ends_with(['.', ' '])
        || component.chars().any(windows_forbidden_character)
    {
        return true;
    }
    let basename = component.split('.').next().unwrap_or("");
    let upper = basename.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || reserved_numbered(&upper, "COM")
        || reserved_numbered(&upper, "LPT")
}

fn windows_forbidden_character(character: char) -> bool {
    matches!(
        character,
        '\0'..='\u{1f}' | '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'
    )
}

fn reserved_numbered(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ValueError;

#[cfg(test)]
mod high_risk_recovery {
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
            "unicode/＜＞：＂／＼｜？＊.txt",
            "unicode/trailing-nbsp\u{a0}",
        ] {
            assert_eq!(
                SafeRelativeRefV2::parse(valid).unwrap().as_str().as_bytes(),
                valid.as_bytes()
            );
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

    #[test]
    fn safe_relative_reference_rejects_windows_forbidden_characters() {
        for forbidden in [
            '<', '>', ':', '"', '|', '?', '*', '\u{1}', '\u{10}', '\u{1f}',
        ] {
            for invalid in [
                format!("bad{forbidden}name"),
                format!("safe/bad{forbidden}name.ext"),
            ] {
                assert!(
                    SafeRelativeRefV2::parse(&invalid).is_err(),
                    "accepted {invalid:?}"
                );
            }
        }
    }

    #[test]
    fn safe_relative_reference_rejects_superscript_reserved_basenames() {
        for reserved in ["COM¹", "com²", "CoM³", "LPT¹", "lpt²", "LpT³"] {
            for invalid in [reserved.to_owned(), format!("safe/{reserved}.json")] {
                assert!(
                    SafeRelativeRefV2::parse(&invalid).is_err(),
                    "accepted {invalid:?}"
                );
            }
        }
    }
}
