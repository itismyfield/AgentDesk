use super::*;

#[test]
fn common_glyphs_and_long_unfolded_inputs_need_no_render_prediction() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let corpus = "한글 English “quotes” … 😀 👍🏻 👨‍👩‍👧‍👦 ❤️ 🇰🇷 漢字 かな";
    for payload in [corpus.to_string(), corpus.repeat(20), "가".repeat(300)] {
        let fake = Fake::new();
        fake.answers("attach", &["4,,46,37"]);
        fake.caps(&[busy_empty()]);
        assert_eq!(fake.run_text(&payload, &FAST), Outcome::Injected);
        assert_eq!(fake.keys(), (1, 1, 0));
        assert_eq!(fake.calls("capture-pane").len(), 1);
    }
}
