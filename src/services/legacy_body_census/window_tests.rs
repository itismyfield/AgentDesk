use serde_json::json;

pub(super) use super::super::window::valid_chain;

#[test]
fn five_thousand_links_are_visited_at_most_twice() {
    let ids: Vec<_> = (0..5000)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let anchor = json!({"boot_id":ids[0],"started_at_ms":0});
    let headers = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                id.clone(),
                json!({"boot_id":id,"started_at_ms":i as i64,"coverage_start":anchor,"predecessor_boot_id":i.checked_sub(1).map(|prior| &ids[prior])}),
            )
        })
        .collect();
    let mut visits = 0;
    assert!(valid_chain(&headers, &anchor, 4999, None, &mut visits));
    assert!(visits <= 2 * ids.len(), "visited {visits}");
}
