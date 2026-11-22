//! A thread follows its parent's policy for the next execution, while the execution it runs now
//! is read only from what this process judged.

use super::super::surface_policy::{Relation, force_relation_for_test};
use super::*;
use crate::config::session_hosts::force_for_test;

const HERDR_PARENT: u64 = 8_118_100;
const TMUX_PARENT: u64 = 8_118_101;

fn forget(channel: u64) {
    JUDGED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&channel);
}

#[test]
fn a_thread_reads_its_parents_policy_and_an_unresolved_one_reads_neither_host() {
    let (child, tmux_child, held, plain) = (8_118_110, 8_118_111, 8_118_112, 8_118_113);
    let _hosts = force_for_test(Some("mac-mini"), &[(HERDR_PARENT, "mac-mini")]);
    let parent = Relation::Child {
        parent: HERDR_PARENT,
    };
    let _child = force_relation_for_test(child, parent);
    let tmux = Relation::Child {
        parent: TMUX_PARENT,
    };
    let _tmux_child = force_relation_for_test(tmux_child, tmux);
    let _held = force_relation_for_test(held, Relation::Unresolved);
    let endpoint = session_hosts::herdr_endpoint(HERDR_PARENT).unwrap();

    let own = PolicyHost::Herdr {
        endpoint: endpoint.clone(),
        via_parent: None,
    };
    assert_eq!(parent_policy(HERDR_PARENT), own);
    let inherited = PolicyHost::Herdr {
        endpoint,
        via_parent: Some(HERDR_PARENT),
    };
    assert_eq!(parent_policy(child), inherited);
    assert_eq!(parent_policy(tmux_child), PolicyHost::Tmux);
    assert_eq!(parent_policy(plain), PolicyHost::Tmux);
    assert!(matches!(parent_policy(held), PolicyHost::Unknown(_)));
    // The raw boot read still names only the configured channel itself.
    assert!(session_hosts::herdr_endpoint(child).is_none());
    assert!(!session_hosts::has_herdr_endpoint(child));

    assert!(!left_herdr(HERDR_PARENT));
    assert!(!left_herdr(child), "a thread under a Herdr parent");
    assert!(!left_herdr(held), "an unknown policy is no departure");
    assert!(left_herdr(tmux_child) && left_herdr(plain));
}

#[test]
fn a_channels_own_entry_is_its_execution_with_nothing_judged_and_a_thread_needs_a_judgement() {
    let (child, tmux_child, held) = (8_118_120, 8_118_121, 8_118_122);
    let _hosts = force_for_test(Some("mac-mini"), &[(HERDR_PARENT, "mac-mini")]);
    let _child = force_relation_for_test(
        child,
        Relation::Child {
            parent: HERDR_PARENT,
        },
    );
    let _tmux_child = force_relation_for_test(
        tmux_child,
        Relation::Child {
            parent: TMUX_PARENT,
        },
    );
    let _held = force_relation_for_test(held, Relation::Unresolved);
    let endpoint = session_hosts::herdr_endpoint(HERDR_PARENT).unwrap();
    forget(child);

    // Right after a restart nothing is judged: the boot entry alone answers for its own channel.
    let herdr = Execution::Herdr(endpoint.clone());
    assert_eq!(current_execution_cached(HERDR_PARENT), herdr);
    assert_eq!(current_execution_cached(tmux_child), Execution::Tmux);
    assert_eq!(current_execution_cached(TMUX_PARENT), Execution::Tmux);
    assert_eq!(current_execution_cached(held), Execution::Unknown);
    assert_eq!(current_execution_cached(child), Execution::Unknown);
    assert!(!runs_herdr(child) && !next_is_herdr(child));
    assert_eq!(next_host_cached(child), NextHost::Tmux);
    assert_eq!(next_host_cached(HERDR_PARENT), NextHost::Herdr);

    record_judged(child, Some(false));
    assert_eq!(current_execution_cached(child), Execution::Tmux);
    assert_eq!(next_host_cached(child), NextHost::Tmux);
    record_judged(child, Some(true));
    assert_eq!(current_execution_cached(child), herdr);
    assert_eq!(current_execution_cached(child).endpoint(), Some(endpoint));
    assert_eq!(next_host_cached(child), NextHost::Herdr);
    // A refused or returning judgement forgets the execution instead of calling it tmux.
    record_judged(child, None);
    assert_eq!(current_execution_cached(child), Execution::Unknown);

    // Nothing is recorded for a channel that is no resolved thread.
    record_judged(held, Some(true));
    record_judged(TMUX_PARENT, Some(true));
    assert_eq!(current_execution_cached(held), Execution::Unknown);
    assert_eq!(current_execution_cached(TMUX_PARENT), Execution::Tmux);
    record_judged(tmux_child, Some(true));
    assert_eq!(current_execution_cached(tmux_child), Execution::Tmux);
    assert_eq!(next_host_cached(tmux_child), NextHost::Tmux);
    forget(tmux_child);
}

#[test]
fn the_next_host_of_a_thread_is_herdr_only_on_its_parents_own_endpoint() {
    let child = 8_118_130;
    let hosts = [(HERDR_PARENT, "mac-mini"), (TMUX_PARENT + 50, "mac-book")];
    let _hosts = force_for_test(Some("mac-mini"), &hosts);
    let _child = force_relation_for_test(
        child,
        Relation::Child {
            parent: HERDR_PARENT,
        },
    );
    let own = session_hosts::herdr_endpoint(HERDR_PARENT).unwrap();
    let other = session_hosts::herdr_endpoint(TMUX_PARENT + 50).unwrap();
    assert_eq!(next_host(child, &Execution::Herdr(own)), NextHost::Herdr);
    assert_eq!(next_host(child, &Execution::Herdr(other)), NextHost::Tmux);
    assert_eq!(next_host(child, &Execution::Tmux), NextHost::Tmux);
    assert_eq!(next_host(child, &Execution::Unknown), NextHost::Tmux);
    assert_eq!(
        next_host(HERDR_PARENT, &Execution::Unknown),
        NextHost::Herdr
    );
}
