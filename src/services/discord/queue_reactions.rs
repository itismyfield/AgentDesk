pub(in crate::services::discord) const QUEUE_STANDALONE_PENDING_REACTION: char = '📬';
pub(in crate::services::discord) const QUEUE_MERGED_PENDING_REACTION: char = '➕';
pub(in crate::services::discord) const QUEUE_RECONCILE_PENDING_REACTION: char = '🔄';

pub(in crate::services::discord) const QUEUE_PENDING_REACTION_EMOJIS: [char; 3] = [
    QUEUE_STANDALONE_PENDING_REACTION,
    QUEUE_MERGED_PENDING_REACTION,
    QUEUE_RECONCILE_PENDING_REACTION,
];

pub(in crate::services::discord) const QUEUE_STANDALONE_DRAIN_REACTION_EMOJIS: [char; 2] = [
    QUEUE_STANDALONE_PENDING_REACTION,
    QUEUE_RECONCILE_PENDING_REACTION,
];

pub(in crate::services::discord) fn drain_reactions_for_queue_exit(
    is_standalone: bool,
) -> &'static [char] {
    if is_standalone {
        &QUEUE_STANDALONE_DRAIN_REACTION_EMOJIS
    } else {
        &QUEUE_PENDING_REACTION_EMOJIS
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn drain_sets_include_only_queue_markers() {
        assert!(super::QUEUE_PENDING_REACTION_EMOJIS.contains(&'🔄'));
        assert!(super::QUEUE_STANDALONE_DRAIN_REACTION_EMOJIS.contains(&'🔄'));
        assert!(super::drain_reactions_for_queue_exit(true).contains(&'🔄'));
        assert!(super::drain_reactions_for_queue_exit(false).contains(&'🔄'));
        assert!(!super::drain_reactions_for_queue_exit(true).contains(&'⏳'));
        assert!(!super::drain_reactions_for_queue_exit(false).contains(&'⏳'));
    }
}
