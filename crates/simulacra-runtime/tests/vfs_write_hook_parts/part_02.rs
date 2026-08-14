#[tokio::test]
async fn hook_input_context_uses_v1_schema_with_tenant_path_op_and_bytes_len() {
    // Pin the v1 input ctx schema: `{tenant, path, op, bytes_len}` and nothing
    // else. Compare via parsed JSON, not exact-string equality, so trivial
    // formatting changes don't break the test but a schema drift does.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("schema", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    let _ = layer.write("/workspace/file.txt", b"abc");

    let calls = calls.lock().unwrap();
    let (_, phase, op, ctx) = calls
        .iter()
        .find(|(_, phase, op, _)| *phase == Phase::Before && *op == Operation::VfsWrite)
        .expect("expected a Before VfsWrite invocation");
    assert_eq!(*phase, Phase::Before);
    assert_eq!(*op, Operation::VfsWrite);

    let object = ctx.as_object().expect("ctx must be a JSON object");
    assert_eq!(
        object.get("tenant").and_then(Value::as_str),
        Some("tenant-a")
    );
    assert_eq!(
        object.get("path").and_then(Value::as_str),
        Some("/workspace/file.txt")
    );
    assert_eq!(object.get("op").and_then(Value::as_str), Some("write"));
    assert_eq!(object.get("bytes_len").and_then(Value::as_u64), Some(3));

    // The v1 schema MUST NOT expose `bytes` to the hook chain.
    assert!(
        !object.contains_key("bytes"),
        "v1 schema must not expose `bytes` to hooks: {ctx:?}"
    );
    // No keys beyond the four documented fields. (Adds defensiveness against
    // accidental schema drift in either direction.)
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["bytes_len", "op", "path", "tenant"],
        "unexpected keys in v1 hook ctx: {keys:?}"
    );
}

#[tokio::test]
async fn hook_input_context_op_field_is_remove_for_remove_operations() {
    // Mirror of the schema test for `remove`: the `op` field must be "remove"
    // when the layer is invoked through `VirtualFs::remove`. This is the bit
    // that disambiguates remove from a zero-byte write — both have
    // `bytes_len == 0` so a hook needs `op` to tell them apart.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "schema-remove",
        vec![],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    let _ = layer.remove("/workspace/file.txt");

    let calls = calls.lock().unwrap();
    let (_, _, _, ctx) = calls
        .iter()
        .find(|(_, phase, op, _)| *phase == Phase::Before && *op == Operation::VfsWrite)
        .expect("expected a Before VfsWrite invocation for remove");
    let object = ctx.as_object().expect("ctx must be a JSON object");
    assert_eq!(
        object.get("op").and_then(Value::as_str),
        Some("remove"),
        "remove invocation should carry op=\"remove\", got ctx={ctx:?}"
    );
    assert_eq!(
        object.get("bytes_len").and_then(Value::as_u64),
        Some(0),
        "remove should advertise bytes_len=0"
    );
}

#[tokio::test]
async fn mutate_verdict_modifying_op_produces_hook_contract_violation() {
    // `op` is immutable per the v1 mutation contract — a hook cannot upgrade
    // a `write` to a `remove` (or vice versa). Attempting to do so produces
    // `VfsError::HookContractViolation` and emits no event.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "mutate-op",
        vec![Verdict::Continue(Some(
            json!({
                "tenant": tenant().as_str(),
                "path": "/file.txt",
                "op": "remove",
                "bytes_len": 5,
            })
            .to_string(),
        ))],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    // Originating call is `write`; the hook tries to flip op to "remove".
    let err = layer.write("/file.txt", b"hello").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(err, VfsError::HookContractViolation),
        "expected HookContractViolation when hook flips op, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on op-mutation contract violation, got {received:?}"
    );
}

/// A `HookModule` that always returns a hook execution error (Internal).
/// Used to drive the "error" outcome in `simulacra.vfs.hook_outcome`.
struct ErroringHook {
    name: String,
}

impl HookModule for ErroringHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        _phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        Err(HookError::ExecutionError {
            hook: self.name.clone(),
            message: "boom".to_string(),
        })
    }
}

#[tokio::test]
async fn hook_chain_internal_error_fails_closed_with_error_outcome_not_violation() {
    // A non-Killed hook chain error (e.g., serde failure or hook execution
    // panic surfaced as `Err`) must fail closed AND map to the `error`
    // outcome, not `violation`. `violation` is reserved for `Continue`
    // returned with a mutation to an immutable field (`tenant` / `op`).
    let hook = Arc::new(ErroringHook {
        name: "boom".to_string(),
    });
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.write("/file.txt", b"hello").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    // Fail-closed: a hook error blocks the write. The error string carries
    // "hook error: ..." so an operator can distinguish a synthesized failure
    // from a hook-issued Deny.
    assert!(
        matches!(&err, VfsError::HookDenied { reason } if reason.starts_with("hook error: ")),
        "expected fail-closed HookDenied with 'hook error: ' prefix, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on hook chain error, got {received:?}"
    );
    // Note: the span attribute `simulacra.vfs.hook_outcome = error` is asserted
    // by the o11y test in s039_vfs_write_o11y.rs (Aniani path); here we pin
    // the error mapping at the API boundary.
}
