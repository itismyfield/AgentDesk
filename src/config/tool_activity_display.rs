//! Display detail for tool activity in the existing Discord status surface.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivityDisplay {
    Class,
    #[default]
    Summary,
}

#[cfg(test)]
mod tests {
    use super::ToolActivityDisplay;
    use crate::config::{Config, PlaceholderConfig};

    #[test]
    fn omitted_tool_activity_detail_defaults_to_summary() {
        let parsed: PlaceholderConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(parsed.tool_activity_display, ToolActivityDisplay::Summary);
        assert_eq!(parsed, PlaceholderConfig::default());
        let saved = serde_yaml::to_string(&Config::default()).unwrap();
        assert!(!saved.contains("placeholder:"), "{saved}");
    }

    #[test]
    fn tool_activity_modes_round_trip_without_accepting_unknown_detail() {
        for (text, expected) in [
            ("class", ToolActivityDisplay::Class),
            ("summary", ToolActivityDisplay::Summary),
        ] {
            let yaml = format!("tool_activity_display: {text}\n");
            let parsed: PlaceholderConfig = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed.tool_activity_display, expected);
            let saved = serde_yaml::to_string(&parsed).unwrap();
            let reparsed: PlaceholderConfig = serde_yaml::from_str(&saved).unwrap();
            assert_eq!(reparsed, parsed);
            assert!(saved.contains(&yaml), "{saved}");
        }
        assert!(serde_yaml::from_str::<PlaceholderConfig>("tool_activity_display: raw\n").is_err());
    }
}
