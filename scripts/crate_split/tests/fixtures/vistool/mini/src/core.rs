pub(crate) mod util;

use crate::services::store::*;

pub(crate) use self::util::Shared;

pub(crate) struct Config {
    pub(crate) name: String,
    pub(crate) limit: u32,
}

impl Config {
    pub(crate) fn new(name: &str) -> Self {
        Config { name: name.to_string(), limit: COUNT }
    }

    pub(crate) fn limit(&self) -> u32 {
        self.limit
    }
}

pub(crate) struct Point {
    pub(crate) x: u32,
    pub(crate) y: u32,
}

pub(crate) struct Span {
    pub(crate) lo: u32,
}

pub(crate) fn span_lo(span: &Span) -> u32 {
    span.lo
}

pub(crate) struct Id(pub(crate) u32);

pub(crate) struct Token;

pub(crate) fn make_token() -> Token {
    Token
}

pub(crate) fn helper() -> u32 {
    3
}
