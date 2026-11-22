//! vistool real-mode fixture: the upper member names lower items through `lower::core`.
pub fn total() -> u32 {
    let config = lower::core::Config::new();
    config.limit + lower::core::helper()
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_up() {
        assert_eq!(super::total(), 5);
    }
}
