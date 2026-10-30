use super::{PieceKey, ShadowProvider, UnitKey, UnitKind, kind_name};

#[test]
fn input_notices_never_enter_the_pg_repost_budget() {
    for provider in [ShadowProvider::Claude, ShadowProvider::Codex] {
        let unit = UnitKey {
            channel_id: 7_332_001,
            provider,
            native_key: "native-input".into(),
            kind: UnitKind::InputNotice,
        };
        assert!(PieceKey::new(unit, 0).is_none());
    }
    assert_eq!(kind_name(UnitKind::InputNotice), "input_notice");
}
