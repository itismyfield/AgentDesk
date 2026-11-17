use super::*;

#[test]
fn restart_identity_is_stable_and_separates_each_namespace_component() {
    let operation = Operation::new("claude", "token-a", 8128, "message:1").unwrap();
    assert_eq!(operation.namespace(), "claude/token-a");
    assert_eq!(operation.channel(), 8128);
    assert_eq!(
        operation,
        Operation::new("claude", "token-a", 8128, "message:1").unwrap()
    );
    for other in [
        Operation::new("codex", "token-a", 8128, "message:1").unwrap(),
        Operation::new("claude", "token-b", 8128, "message:1").unwrap(),
        Operation::new("claude", "token-a", 8129, "message:1").unwrap(),
        Operation::new("claude", "token-a", 8128, "interaction:1").unwrap(),
    ] {
        assert_ne!(operation, other);
    }
    assert!(Operation::new("claude", "token-a", 0, "message:1").is_err());
    assert!(Operation::new("claude", "token-a", 8128, "").is_err());
}

#[test]
fn restart_seed_identity_is_fixed_by_the_operation_alone_and_names_no_discord_message() {
    let operation = Operation::new("claude", "token-a", 8128, "message:1").unwrap();
    let (message, ticket) = operation.seed_identity();
    assert_eq!(
        (message, ticket.clone()),
        Operation::new("claude", "token-a", 8128, "message:1")
            .unwrap()
            .seed_identity(),
        "a replay and a new process derive the same greeting"
    );
    assert_ne!(message & (1 << 63), 0, "never a Discord message id");
    assert!(ticket.len() <= 64 && ticket.starts_with("restart-"));
    assert!(
        ticket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    );
    for other in [
        Operation::new("codex", "token-a", 8128, "message:1").unwrap(),
        Operation::new("claude", "token-b", 8128, "message:1").unwrap(),
        Operation::new("claude", "token-a", 8129, "message:1").unwrap(),
        Operation::new("claude", "token-a", 8128, "message:2").unwrap(),
        // A length prefix keeps the namespace and the id from running into each other.
        Operation::new("claude", "token-", 8128, "amessage:1").unwrap(),
    ] {
        let (other_message, other_ticket) = other.seed_identity();
        assert_ne!(ticket, other_ticket);
        assert_ne!(message, other_message);
    }
}
