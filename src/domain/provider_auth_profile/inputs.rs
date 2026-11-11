//! Normalized overrides retain an explicit `default` and keep each source separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProfileOverrides<'a> {
    pub channel: Option<&'a str>,
    pub agent: Option<&'a str>,
}

impl<'a> ProfileOverrides<'a> {
    pub(crate) fn new(channel: Option<&'a str>, agent: Option<&'a str>) -> Self {
        let normalize =
            |value: Option<&'a str>| value.map(str::trim).filter(|value| !value.is_empty());
        Self {
            channel: normalize(channel),
            agent: normalize(agent),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileOverrides;

    #[test]
    fn normalization_preserves_sources_and_explicit_default() {
        let input = ProfileOverrides::new(Some(" default "), Some(" work "));
        assert_eq!(input.channel, Some("default"));
        assert_eq!(input.agent, Some("work"));
        let input = ProfileOverrides::new(Some(" \t"), Some(" work "));
        assert_eq!(input.channel, None);
        assert_eq!(input.agent, Some("work"));
        assert_eq!(ProfileOverrides::new(None, Some(" ")).agent, None);
    }
}
