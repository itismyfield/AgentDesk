use super::{FD_HEADROOM_WARN_PERCENT, FD_HEADROOM_WARN_REMAINING};

pub(super) fn fd_usage_percent(open_files: u64, soft_limit: u64) -> u64 {
    if soft_limit == 0 {
        return 0;
    }
    open_files.saturating_mul(100) / soft_limit
}

pub(super) fn fd_usage_near_limit(open_files: u64, soft_limit: u64) -> bool {
    if soft_limit == 0 {
        return false;
    }
    let remaining = soft_limit.saturating_sub(open_files);
    fd_usage_percent(open_files, soft_limit) >= FD_HEADROOM_WARN_PERCENT
        || remaining <= FD_HEADROOM_WARN_REMAINING
}
