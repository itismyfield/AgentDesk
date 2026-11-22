use crate::core::{Config, Id, Point, Span};

// rustc runs the field-privacy pass (E0451) only once nothing else failed, so this crate keeps no residue.
fn span_width() -> u32 {
    let span = Span { lo: 8 };
    crate::core::span_lo(&span)
}

pub(crate) fn start() -> u32 {
    let config = Config::new("mini");
    let point = Point { x: 1, y: 2 };
    let Id(id) = Id(4);
    let shared = crate::core::Shared(5);
    let _token = crate::core::make_token();
    config.name.len() as u32
        + config.limit
        + config.limit()
        + point.x
        + point.y
        + id
        + span_width()
        + shared.0
        + crate::core::helper()
        + crate::core::util::tidy(1)
        + crate::services::web_total()
}

#[cfg(test)]
mod tests {
    #[test]
    fn start_adds_up() {
        assert!(super::start() > 0);
    }
}
