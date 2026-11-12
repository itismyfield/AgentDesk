//! A separate print process has not proven the running TUI's workflow capability.

pub(crate) fn ultracode_available(_model: Option<&str>) -> Result<bool, String> {
    Err(
        "현재 Claude TUI에서 ultra 사용 가능 여부를 확인하지 못했습니다. 모델은 그대로 유지됩니다."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Applied {
        model: String,
        ultracode: bool,
        #[serde(rename = "ultracodeRequested")]
        requested: bool,
        #[serde(rename = "ultracodeAvailable")]
        available: bool,
    }

    fn observed(raw: &str, expected: &str) -> Result<Applied, String> {
        let response: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if response["type"] != "control_response"
            || response["response"]["subtype"] != "success"
            || response["response"]["request_id"] != "get_settings"
        {
            return Err("not the settings response".into());
        }
        let applied: Applied =
            serde_json::from_value(response["response"]["response"]["applied"].clone())
                .map_err(|e| e.to_string())?;
        if applied.model != expected
            || (applied.ultracode && !(applied.requested && applied.available))
        {
            return Err("unproven applied capability".into());
        }
        Ok(applied)
    }

    #[test]
    fn isolated_probe_reports_availability_separately_from_request_and_active() {
        for (model, requested, available, active) in [
            ("claude-opus-4-8", true, true, true),
            ("claude-opus-4-8", true, false, false),
            ("claude-sonnet-4-6", true, false, false),
            ("claude-opus-4-8", false, true, false),
        ] {
            let response = serde_json::json!({"type":"control_response", "response":{
                "subtype":"success","request_id":"get_settings","response":{
                    "applied":{"model":model,"ultracode":active,"ultracodeRequested":requested,"ultracodeAvailable":available}
                }}}).to_string();
            let applied = observed(&response, model).unwrap();
            assert_eq!(
                (applied.requested, applied.available, applied.ultracode),
                (requested, available, active)
            );
        }
    }

    #[test]
    fn standalone_probe_cannot_establish_live_tui_capability() {
        assert!(super::ultracode_available(Some("claude-opus-4-8")).is_err());
        assert!(super::ultracode_available(None).is_err());
        assert!(observed(r#"{"type":"control_response","response":{"subtype":"success","request_id":"get_settings","response":{"applied":{"model":"claude-opus-4-8","ultracode":true,"ultracodeRequested":true,"ultracodeAvailable":false}}}}"#, "claude-opus-4-8").is_err());
        assert!(observed(r#"{"type":"control_response","response":{"subtype":"success","request_id":"get_settings","response":{"applied":{"model":"claude-opus-4-8","ultracode":false,"ultracodeRequested":false}}}}"#, "claude-opus-4-8").is_err());
    }
}
