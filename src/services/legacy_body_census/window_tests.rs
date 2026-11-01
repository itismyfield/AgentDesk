use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

// Link integrity is separate from the collector's epoch and window coverage policy.
pub(super) fn valid_chain(headers: &BTreeMap<String, Value>, visits: &mut usize) -> bool {
    if headers.is_empty() {
        return false;
    }
    let mut complete = BTreeSet::new();
    for start in headers.keys() {
        let mut path = BTreeSet::new();
        let mut cursor = Some(start.as_str());
        while let Some(id) = cursor {
            *visits += 1;
            if complete.contains(id) {
                break;
            }
            if !path.insert(id) {
                return false;
            }
            let Some(header) = headers.get(id) else {
                return false;
            };
            cursor = match header.get("predecessor_boot_id") {
                Some(Value::Null) => None,
                Some(Value::String(parent)) if uuid::Uuid::parse_str(parent).is_ok() => {
                    Some(parent)
                }
                _ => return false,
            };
        }
        complete.extend(path);
    }
    true
}

#[test]
fn five_thousand_links_are_visited_at_most_twice() {
    let ids: Vec<_> = (0..5000)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let headers = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                id.clone(),
                json!({"predecessor_boot_id":i.checked_sub(1).map(|prior| &ids[prior])}),
            )
        })
        .collect();
    let mut visits = 0;
    assert!(valid_chain(&headers, &mut visits));
    assert!(visits <= 2 * ids.len(), "visited {visits}");
}
