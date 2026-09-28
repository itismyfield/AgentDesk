pub fn sink() {}
pub fn caller() {
    sink();
}
pub fn outside() {
    caller();
}

use std::collections::HashMap;

#[cfg(feature = "workspace_lib")]
pub fn from_helper() -> u8 {
    helper::value()
}

#[cfg(feature = "workspace_proc")]
mac::item!();
