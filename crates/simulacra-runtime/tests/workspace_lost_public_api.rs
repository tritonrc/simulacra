use simulacra_runtime::{RuntimeError, WorkspaceLostCause};

#[test]
fn downstream_consumers_can_match_every_workspace_lost_cause() {
    let cases = [
        WorkspaceLostCause::Gone,
        WorkspaceLostCause::Closed,
        WorkspaceLostCause::Reaped,
    ];

    for expected in cases {
        let error = RuntimeError::WorkspaceLost { cause: expected };

        match error {
            RuntimeError::WorkspaceLost { cause } => assert_eq!(cause, expected),
            other => panic!("expected WorkspaceLost, got {other:?}"),
        }
    }
}
