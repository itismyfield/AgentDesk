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
