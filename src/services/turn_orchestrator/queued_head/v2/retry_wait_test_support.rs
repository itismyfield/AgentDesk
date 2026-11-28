use super::{Front, Owner, same};

/// The waiting original keeps its slot, body and ticket, and nothing was retained or refused.
pub fn assert_original_and_ticket_kept(owner: &Owner, front: &Front) {
    let now = owner.front().expect("the original still leads");
    assert!(now.0.is_some() && now.0 == front.0, "ticket {:?}", now.0);
    assert!(same(&now.1, &front.1), "original changed");
    assert!(owner.snapshot.retained_inputs.is_empty());
    assert!(owner.snapshot.refused_fronts.is_empty());
}
