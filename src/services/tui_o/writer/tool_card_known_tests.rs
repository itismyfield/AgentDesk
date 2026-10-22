use super::*;
use crate::services::tui_o::shadow::UnitKind;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn panel_disposition_enters_live_known_before_another_tool() {
    let on = Arc::new(AtomicBool::new(false));
    let switched = Arc::clone(&on);
    let mut run = ToolCardRun::new(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(move || switched.load(Ordering::SeqCst)),
    });
    let key = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "off:0".into(),
        kind: UnitKind::Tool,
    };
    let meta = || ToolMeta {
        id: "off".into(),
        label: "Bash".into(),
        summary: None,
    };
    assert!(matches!(run.tool(key.clone(), meta()).as_slice(),
        [Derived::Excluded { unit_key, reason }]
            if *unit_key == key && reason == TOOL_CALL_PANEL));
    assert!(
        run.inputs.known.contains(&key),
        "panel choice must enter live known immediately"
    );
    on.store(true, Ordering::SeqCst);
    assert!(matches!(run.tool(key.clone(), meta()).as_slice(),
        [Derived::Excluded { unit_key, reason }]
            if *unit_key == key && reason == TOOL_CALL_PANEL));
    assert!(run.close().is_empty());
}

#[test]
fn hidden_summaries_group_by_label_and_status() {
    for status in ['✓', '·'] {
        let rows = ["first", "second"].map(|summary| Row {
            key: UnitKey {
                channel_id: 7,
                provider: ShadowProvider::Claude,
                native_key: summary.into(),
                kind: UnitKind::Tool,
            },
            meta: ToolMeta {
                id: summary.into(),
                label: "Bash".into(),
                summary: Some(summary.into()),
            },
            status,
        });
        let visible = render(rows.iter(), true);
        assert_eq!(visible.lines().count(), 2, "{visible}");
        assert!(!visible.contains('×'), "{visible}");
        assert!(visible.contains("first") && visible.contains("second"));
        let hidden = render(rows.iter(), false);
        assert_eq!(hidden, format!("-# ▸ Bash ×2  {status}"));
    }
}
