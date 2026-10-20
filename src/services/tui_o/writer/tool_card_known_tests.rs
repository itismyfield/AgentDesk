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
        label: "Bash",
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
