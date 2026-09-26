// Canary for tools/modmap-driver: `mod shared;` and `wrapped::passed` are clean; flagged are `spliced`, `probe`'s mount
// and the macro-made `wrapped`, `named` and `keyword`, whose macros write only the `mod` item, the name or the keyword.
macro_rules! mount {
    ($path:literal) => {
        #[path = $path]
        mod injected;
    };
}
macro_rules! wrap { ($name:ident $body:tt) => { pub mod $name $body }; }
macro_rules! name { ($kw:tt $body:tt) => { $kw named $body }; }
macro_rules! kw { ($vis:tt $name:ident $body:tt) => { $vis mod $name $body }; }

mod shared;

wrap!(wrapped { pub mod passed; });
name!(mod { pub fn kept() {} });
kw!(pub keyword { pub fn kept() {} });

pub mod spliced {
    include!("shared.rs");
}

pub fn probe() {
    mount!("shared.rs");
}
