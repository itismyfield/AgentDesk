//! The rules an ask's options must keep before anything is stored.
use super::*;
use serde_json::json;

fn rule(options: serde_json::Value) -> Result<usize, &'static str> {
    let body = json!({"agent_id": "a", "target": {"channel_id": "7"}, "ref": "r", "title": "t",
        "options": options});
    let ask: Ask = serde_json::from_value(body).unwrap();
    ask.stored_options().map(|options| options.len())
}

/// Two to five options, names that cannot be confused with each other or with a reply's option
/// number, and a recommendation for every option or for none.
#[test]
fn decision_ask_options_are_distinct_words_with_a_whole_recommendation() {
    let label = |name: &str| json!({"label": name});
    let percent =
        |name: &str, percent: i64| json!({"label": name, "recommendation_percent": percent});
    assert_eq!(rule(json!([label(" 진행 "), label("축소")])), Ok(2));
    assert_eq!(rule(json!([percent("a", 70), percent("b", 30)])), Ok(2));
    let names = "option_names_must_be_distinct_words";
    let whole = "recommendation_must_cover_every_option_and_sum_to_100";
    let refused = [
        (json!([label("a")]), "options_must_be_two_to_five"),
        (
            json!([
                label("a"),
                label("b"),
                label("c"),
                label("d"),
                label("e"),
                label("f")
            ]),
            "options_must_be_two_to_five",
        ),
        (json!([label("a"), label(" a")]), names),
        (json!([label("a"), {"label": "b", "key": "a"}]), names),
        (json!([label("2"), label("b")]), names),
        (json!([label("2번"), label("b")]), names),
        (json!([label(" "), label("b")]), names),
        (json!([percent("a", 70), label("b")]), whole),
        (json!([percent("a", 70), percent("b", 20)]), whole),
        (json!([percent("a", 130), percent("b", -30)]), whole),
    ];
    for (options, expected) in refused {
        assert_eq!(rule(options.clone()), Err(expected), "{options}");
    }
}

/// The card is measured as Discord measures it, in UTF-16 units: one of exactly 2,000 units is
/// posted, one more is refused, and a character outside the BMP counts twice.
#[test]
fn decision_card_is_refused_past_two_thousand_utf16_units() {
    let card = |context: &str| {
        let body = json!({"agent_id": "a", "target": {"channel_id": "7"}, "ref": "r",
            "title": "t", "context": context, "options": [{"label": "x"}, {"label": "y"}]});
        let ask: Ask = serde_json::from_value(body).unwrap();
        let options = ask.stored_options().unwrap();
        card::render(&ask, &options, Some(7), Uuid::nil()).map(|card| card.content)
    };
    let empty = card("").unwrap().encode_utf16().count();
    // A context adds its own line: its text and one newline.
    let room = card::CONTENT_LIMIT - empty - 1;
    let fill = format!("{}{}", "a".repeat(room % 2), "😀".repeat(room / 2));
    assert_eq!(
        card(&fill).map(|text| text.encode_utf16().count()),
        Ok(2000)
    );
    assert_eq!(card(&format!("{fill}😀")).err(), Some(2002));
}
