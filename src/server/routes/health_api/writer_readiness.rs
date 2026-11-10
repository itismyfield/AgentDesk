use serde_json::{Value, json};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::cutover::{
    self,
    intake_route::{self, IntakeRoute},
};

pub(super) fn attach(body: &mut Value, detailed: bool) {
    if !detailed {
        return;
    }
    body["tui_o_writer_readiness"] = cutover::boot_ownership()
        .into_iter()
        .filter_map(|(channel, kind, _)| {
            let provider = match kind? {
                RuntimeHandoffKind::ClaudeTui => "claude",
                RuntimeHandoffKind::CodexTui => "codex",
                _ => return None,
            };
            let route = intake_route::route(provider, channel);
            let accepts = route == IntakeRoute::Gateway;
            let reason = match route {
                IntakeRoute::Gateway => None,
                IntakeRoute::Hold(reason) => Some(reason),
                IntakeRoute::Unselected => Some("channel is not owned by O".into()),
            };
            Some(
                json!({"provider": provider, "channel_id": channel.to_string(),
                    "accepts": accepts, "hold_reason": reason}),
            )
        })
        .collect::<Vec<_>>()
        .into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::cutover::test_override;

    #[test]
    fn writer_readiness_projects_the_admission_result_per_boot_channel() {
        let channels = [
            (805601, RuntimeHandoffKind::ClaudeTui),
            (805602, RuntimeHandoffKind::CodexTui),
        ];
        let boot = test_override::force_channels(&channels);
        let mut body = json!({});
        // Both the ready and unready answers must come through the existing admission guard.
        let probe = intake_route::test_probe::answer_with(|channel| channel == 805601);
        attach(&mut body, true);
        drop((probe, boot));
        let rows = body["tui_o_writer_readiness"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            json!({"provider":"claude", "channel_id":"805601", "accepts":true, "hold_reason":null})
        );
        assert_eq!(rows[1]["provider"], "codex");
        assert_eq!(rows[1]["channel_id"], "805602");
        assert_eq!(rows[1]["accepts"], false);
        assert!(
            rows[1]["hold_reason"]
                .as_str()
                .unwrap()
                .contains("writer ready")
        );
    }

    #[test]
    fn writer_readiness_omits_unconfigured_channels_and_public_projection() {
        let boot = test_override::force_channels(&[]);
        let mut body = json!({});
        attach(&mut body, true);
        assert_eq!(body["tui_o_writer_readiness"], json!([]));
        let mut public = json!({});
        attach(&mut public, false);
        assert!(public.get("tui_o_writer_readiness").is_none());
        drop(boot);
    }

    #[test]
    fn writer_readiness_a_boot_channel_without_a_host_is_unready() {
        let boot = test_override::force_channels(&[(805603, RuntimeHandoffKind::ClaudeTui)]);
        let mut body = json!({});
        attach(&mut body, true);
        assert_eq!(body["tui_o_writer_readiness"][0]["accepts"], false);
        drop(boot);
    }
}
