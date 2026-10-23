use super::*;

const CORPUS: &str = "한글 English 123 'plain' \"quotes\" “둥근” ‘따옴표’ … — – (괄호) [대괄호] {중괄호} 😀 🚀 👍🏻 👨‍👩‍👧‍👦 ❤️ 🇰🇷 漢字 かな カナ （全角）、。！？";

#[test]
fn common_glyphs_inject_after_full_body_or_folded_confirmation_at_any_length() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for repetitions in [1, 3, 6, 7, 20] {
        let payload = CORPUS.repeat(repetitions);
        let expected = frame("iMessage", "200", "ann", NONCE, &payload);
        // Deliberately different wrap positions: ownership depends on the full body.
        let shown = if expected.encode_utf16().count() > 800 {
            let lines = expected.matches('\n').count();
            if lines == 0 {
                "[Pasted text #1]".to_string()
            } else {
                format!("[Pasted text #1 +{lines} lines]")
            }
        } else {
            expected
                .chars()
                .collect::<Vec<_>>()
                .chunks(23)
                .map(|chunk| chunk.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let fake = Fake::new();
        fake.answers("attach", &["0,,46,37"]);
        fake.caps(&[busy_empty(), screen(&shown, "")]);
        fs::write(fake.dir.path().join("on_enter"), enqueue_record(&expected)).unwrap();
        assert_eq!(
            fake.run_text(&payload, &MULTILINE_TIMING),
            Outcome::Injected,
            "common-glyph corpus repetitions={repetitions}"
        );
        assert_eq!(fake.keys(), (1, 1, 0));
    }
}

#[test]
fn common_glyph_confirmation_never_submits_a_foreign_or_partial_body() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for payload in [CORPUS.to_string(), CORPUS.repeat(20)] {
        let expected = frame("iMessage", "200", "ann", NONCE, &payload);
        let mut foreign = vec![
            format!("{expected}human"),
            expected.replace("漢字", "漢"),
            "[Pasted text #1 +1 lines]".to_string(),
            "[Pasted text #x]".to_string(),
            format!("{expected}\n[Pasted text #1]"),
        ];
        if expected.encode_utf16().count() <= 800 {
            foreign.push("[Pasted text #1]".to_string());
        }
        for shown in foreign {
            let fake = Fake::new();
            fake.answers("attach", &["0,,46,37"]);
            fake.caps(&[busy_empty(), screen(&shown, "")]);
            let outcome = fake.run_text(&payload, &MULTILINE_TIMING);
            assert_eq!(
                fake.keys(),
                (1, 0, 0),
                "foreign body must never receive Enter: {shown}"
            );
            assert_eq!(
                outcome,
                Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
                "shown={shown}"
            );
        }
    }
}
