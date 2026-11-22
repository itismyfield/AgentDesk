use super::store::{Thing, thing};

pub(super) fn total() -> u32 {
    let made: Thing = thing();
    made.size
}
