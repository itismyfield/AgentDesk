//! The tool-image unit kind: never re-posted as text, classed as progress.

use super::{NativeTurn, image_native_key};
use crate::services::discord::outbound::notification_policy::{NoticeClass, unit_class};
use crate::services::tui_o::repost::o_piece_delivery::{PieceKey, kind_name};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};

#[test]
fn tool_image_kind_not_postable_and_progress() {
    let key = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: image_native_key(&NativeTurn::new("u1"), "ab"),
        kind: UnitKind::ToolImage,
    };
    assert!(PieceKey::new(key, 0).is_none());
    assert_eq!(kind_name(UnitKind::ToolImage), "tool_image");
    assert_eq!(unit_class(UnitKind::ToolImage), NoticeClass::Progress);
}
