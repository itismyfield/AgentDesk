pub(crate) struct Config {
    pub(crate) limit: u32,
}

impl Config {
    pub(crate) fn new() -> Self {
        Config { limit: 3 }
    }
}

pub(crate) fn helper() -> u32 {
    2
}

#[cfg(test)]
pub(crate) fn fixture_value() -> u32 {
    7
}
