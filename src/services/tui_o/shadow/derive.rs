#[cfg(test)]
mod tests {
    use super::super::identity::UnitContent;
    use super::super::unit_plan::{UnitPlan, plan};

    /// Long bodies split exactly like Legacy, counting UTF-16 units; an over-limit piece blocks.
    #[test]
    fn split_pieces_follow_legacy_split_in_utf16_units() {
        let text = "한글 본문과 이모지 🎉 섞인 문장. ".repeat(160);
        let Ok(UnitPlan::Pieces(pieces)) = plan(&UnitContent::Payload(text.clone())) else {
            panic!("long body must split into pieces");
        };
        let legacy = crate::services::discord::formatting::split_for_shadow(text.trim());
        assert!(pieces.len() >= 2 && pieces.len() == legacy.len());
        for (piece, (legacy_text, _)) in pieces.iter().zip(&legacy) {
            assert_eq!(piece.units as usize, legacy_text.encode_utf16().count());
            assert!(piece.units <= 2000);
            let sha256 = <sha2::Sha256 as sha2::Digest>::digest(legacy_text);
            assert_eq!(piece.sha256, hex::encode(sha256));
        }
        let over = super::super::unit_plan::digest_pieces(vec![("x".repeat(2001), 2001)]);
        assert!(over.is_err());
    }
}
