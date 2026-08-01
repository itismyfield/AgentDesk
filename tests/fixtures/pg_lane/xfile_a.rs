#[cfg(test)]
mod tests {
    use crate::xfile_b::helpers::middle;

    fn bridge() {
        middle();
    }

    #[test]
    fn transitive_case() {
        bridge();
    }
}
