#![allow(dead_code, non_local_definitions)]
#[cfg(clippy)]
pub fn under_clippy() {}
#[cfg(not(clippy))]
pub fn under_rustc() {}
pub mod ports {
    pub trait Port { fn go(&self); fn defaulted(&self) {} }
    pub struct S;
    pub struct InConst;
}
mod glob {
    use super::ports::*;
    impl S { pub fn inherent(&self) {} }
    impl Port for S { fn go(&self) {} }
}
pub fn impl_in_fn() {
    struct Local;
    impl ports::Port for Local { fn go(&self) { let _ = 1; } }
    impl ports::InConst { pub fn from_fn(&self) {} }
}
const _: () = { impl ports::Port for ports::InConst { fn go(&self) { let _ = 2; } } };
pub fn local_trait() {
    trait Inner { fn local(&self); }
    struct LocalType;
    impl LocalType { fn local_inherent(&self) {} }
}
pub fn fold_ok() { fn folded() {} folded(); }
pub fn fold_escape() {
    fn escaped() {}
    struct Escape;
    impl Escape { fn later() { escaped(); } }
}
impl std::fmt::Display for ports::S {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}
impl dyn ports::Port { pub fn on_dyn(&self) {} }
macro_rules! two { () => { pub fn made_a() {} pub fn made_b() {} }; }
two!();
macro_rules! mixed { () => { pub fn mixed_fn() {} pub const MIXED: () = (); pub enum E { A = 1 } }; }
mixed!();
macro_rules! made_impl { () => { pub struct Made; impl ports::Port for Made { fn go(&self) {} } }; }
made_impl!();
pub const INLINE: () = const { () };
