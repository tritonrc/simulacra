use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use simulacra_hooks::error::HookError;
use simulacra_hooks::verdict::{Operation, Phase, Verdict};
use simulacra_hooks::{HookModule, HookPipeline};

/// A test hook with configurable before/after verdicts and an invocation counter.
struct TestHook {
    name: String,
    before_verdict: Verdict,
    after_verdict: Verdict,
    invocation_count: AtomicUsize,
}

impl TestHook {
    fn new(name: &str, before_verdict: Verdict, after_verdict: Verdict) -> Self {
        Self {
            name: name.to_string(),
            before_verdict,
            after_verdict,
            invocation_count: AtomicUsize::new(0),
        }
    }

    fn invocations(&self) -> usize {
        self.invocation_count.load(Ordering::SeqCst)
    }
}

impl HookModule for TestHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        self.invocation_count.fetch_add(1, Ordering::SeqCst);
        match phase {
            Phase::Before => Ok(self.before_verdict.clone()),
            Phase::After => Ok(self.after_verdict.clone()),
        }
    }
}

/// A hook that records the order it was called.
struct OrderTrackingHook {
    name: String,
    order: Arc<std::sync::Mutex<Vec<String>>>,
}

impl HookModule for OrderTrackingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        self.order
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.name, phase));
        Ok(Verdict::continue_unchanged())
    }
}

/// A hook that modifies the context by appending its name.
struct ModifyingHook {
    name: String,
}

impl HookModule for ModifyingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        _phase: Phase,
        _operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        let modified = format!("{}->{}", context, self.name);
        Ok(Verdict::Continue(Some(modified)))
    }
}

#[test]
fn empty_chain_passes_through() {
    let pipeline = HookPipeline::new();
    let (verdict, ctx) = pipeline
        .run_before(Operation::ToolCall, "test context")
        .unwrap();
    assert_eq!(verdict, Verdict::continue_unchanged());
    assert_eq!(ctx, "test context");

    let (verdict, ctx) = pipeline
        .run_after(Operation::ToolCall, "test context")
        .unwrap();
    assert_eq!(verdict, Verdict::continue_unchanged());
    assert_eq!(ctx, "test context");
}

#[test]
fn before_hooks_run_in_config_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let hook_a: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "a".to_string(),
        order: order.clone(),
    });
    let hook_b: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "b".to_string(),
        order: order.clone(),
    });
    let hook_c: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "c".to_string(),
        order: order.clone(),
    });

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_a);
    pipeline.add(Operation::ToolCall, hook_b);
    pipeline.add(Operation::ToolCall, hook_c);

    pipeline.run_before(Operation::ToolCall, "ctx").unwrap();

    let calls = order.lock().unwrap();
    assert_eq!(*calls, vec!["a:before", "b:before", "c:before"]);
}

#[test]
fn deny_in_before_stops_chain() {
    let hook_a = Arc::new(TestHook::new(
        "a",
        Verdict::continue_unchanged(),
        Verdict::continue_unchanged(),
    ));
    let hook_deny = Arc::new(TestHook::new(
        "deny",
        Verdict::Deny("blocked".to_string()),
        Verdict::continue_unchanged(),
    ));
    let hook_c = Arc::new(TestHook::new(
        "c",
        Verdict::continue_unchanged(),
        Verdict::continue_unchanged(),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_a.clone() as Arc<dyn HookModule>);
    pipeline.add(
        Operation::ToolCall,
        hook_deny.clone() as Arc<dyn HookModule>,
    );
    pipeline.add(Operation::ToolCall, hook_c.clone() as Arc<dyn HookModule>);

    let (verdict, _) = pipeline.run_before(Operation::ToolCall, "ctx").unwrap();

    assert_eq!(verdict, Verdict::Deny("blocked".to_string()));
    assert_eq!(hook_a.invocations(), 1);
    assert_eq!(hook_deny.invocations(), 1);
    assert_eq!(hook_c.invocations(), 0); // Not reached
}

#[test]
fn kill_in_before_stops_chain() {
    let hook_kill = Arc::new(TestHook::new(
        "killer",
        Verdict::Kill("fatal".to_string()),
        Verdict::continue_unchanged(),
    ));
    let hook_after = Arc::new(TestHook::new(
        "after",
        Verdict::continue_unchanged(),
        Verdict::continue_unchanged(),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(
        Operation::ToolCall,
        hook_kill.clone() as Arc<dyn HookModule>,
    );
    pipeline.add(
        Operation::ToolCall,
        hook_after.clone() as Arc<dyn HookModule>,
    );

    let err = pipeline.run_before(Operation::ToolCall, "ctx").unwrap_err();
    assert!(matches!(err, HookError::Killed { .. }));
    assert_eq!(hook_after.invocations(), 0);
}

#[test]
fn after_hooks_run_in_reverse_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let hook_a: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "a".to_string(),
        order: order.clone(),
    });
    let hook_b: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "b".to_string(),
        order: order.clone(),
    });
    let hook_c: Arc<dyn HookModule> = Arc::new(OrderTrackingHook {
        name: "c".to_string(),
        order: order.clone(),
    });

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_a);
    pipeline.add(Operation::ToolCall, hook_b);
    pipeline.add(Operation::ToolCall, hook_c);

    pipeline.run_after(Operation::ToolCall, "ctx").unwrap();

    let calls = order.lock().unwrap();
    assert_eq!(*calls, vec!["c:after", "b:after", "a:after"]);
}

#[test]
fn deny_in_after_is_logged_not_enforced() {
    let hook_deny = Arc::new(TestHook::new(
        "deny-after",
        Verdict::continue_unchanged(),
        Verdict::Deny("too late".to_string()),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_deny as Arc<dyn HookModule>);

    // Should succeed (Deny is downgraded to Continue in after-phase)
    let result = pipeline.run_after(Operation::ToolCall, "ctx");
    assert!(result.is_ok());
    let (verdict, _) = result.unwrap();
    assert_eq!(verdict, Verdict::continue_unchanged());
}

#[test]
fn kill_in_after_is_enforced() {
    let hook_kill = Arc::new(TestHook::new(
        "killer-after",
        Verdict::continue_unchanged(),
        Verdict::Kill("must die".to_string()),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_kill as Arc<dyn HookModule>);

    let err = pipeline.run_after(Operation::ToolCall, "ctx").unwrap_err();
    assert!(matches!(err, HookError::Killed { .. }));
}

#[test]
fn modifications_chain_through_hooks() {
    let hook_a: Arc<dyn HookModule> = Arc::new(ModifyingHook {
        name: "a".to_string(),
    });
    let hook_b: Arc<dyn HookModule> = Arc::new(ModifyingHook {
        name: "b".to_string(),
    });
    let hook_c: Arc<dyn HookModule> = Arc::new(ModifyingHook {
        name: "c".to_string(),
    });

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook_a);
    pipeline.add(Operation::ToolCall, hook_b);
    pipeline.add(Operation::ToolCall, hook_c);

    let (_, ctx) = pipeline.run_before(Operation::ToolCall, "start").unwrap();
    assert_eq!(ctx, "start->a->b->c");
}

#[test]
fn no_hooks_for_operation_passes_through() {
    let hook = Arc::new(TestHook::new(
        "tool-only",
        Verdict::Deny("blocked".to_string()),
        Verdict::continue_unchanged(),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook as Arc<dyn HookModule>);

    // LLM operation has no hooks registered
    let (verdict, ctx) = pipeline.run_before(Operation::Llm, "ctx").unwrap();
    assert_eq!(verdict, Verdict::continue_unchanged());
    assert_eq!(ctx, "ctx");
}
