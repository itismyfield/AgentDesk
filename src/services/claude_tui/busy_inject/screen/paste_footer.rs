/// Measured on Claude Code 2.1.309: a folded paste temporarily replaces the bypass footer.
pub(super) fn recognized(row: &str) -> bool {
    row == "  paste again to expand"
}
