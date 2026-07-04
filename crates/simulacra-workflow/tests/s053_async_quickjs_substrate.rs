use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use simulacra_workflow::{
    WorkflowAgentCall, WorkflowAgentResult, WorkflowError, WorkflowEvent, WorkflowRunOptions,
    WorkflowRuntime, WorkflowStatus, WorkflowStore, WorkflowWorker, WorkflowWorkerFuture,
};

fn runtime_with_worker(worker: Arc<dyn WorkflowWorker>) -> (WorkflowRuntime, Arc<dyn VirtualFs>) {
    let fs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = WorkflowStore::new(Arc::clone(&fs));
    (WorkflowRuntime::new(store, worker), fs)
}

fn inline_options(run_id: &str, script: &str) -> WorkflowRunOptions {
    WorkflowRunOptions {
        run_id: Some(run_id.to_string()),
        script: Some(script.to_string()),
        name: None,
        script_path: None,
        args: json!({ "seed": "s053" }),
        resume_from_run_id: None,
        concurrency_limit: 4,
    }
}

async fn start_or_wait_error(
    runtime: &WorkflowRuntime,
    options: WorkflowRunOptions,
) -> WorkflowError {
    match runtime.start(options).await {
        Ok(handle) => match handle.wait().await {
            Ok(run) => panic!("workflow should have failed, but completed with {run:?}"),
            Err(error) => error,
        },
        Err(error) => error,
    }
}

#[derive(Default)]
struct RecordingWorker {
    calls: Mutex<Vec<WorkflowAgentCall>>,
}

impl RecordingWorker {
    fn calls(&self) -> Vec<WorkflowAgentCall> {
        self.calls
            .lock()
            .expect("calls lock should not be poisoned")
            .clone()
    }
}

impl WorkflowWorker for RecordingWorker {
    fn call<'a>(&'a self, call: WorkflowAgentCall) -> WorkflowWorkerFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .push(call.clone());
            Ok(WorkflowAgentResult::success(
                call.key.clone(),
                json!({
                    "items": [
                        {
                            "label": call.key,
                            "count": 2,
                            "nested": { "ok": true }
                        }
                    ],
                    "input": call.input
                }),
            ))
        })
    }
}

struct DelayedWorker {
    delay: Duration,
}

impl WorkflowWorker for DelayedWorker {
    fn call<'a>(&'a self, call: WorkflowAgentCall) -> WorkflowWorkerFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(WorkflowAgentResult::success(
                call.key,
                json!({ "after_delay": true }),
            ))
        })
    }
}

fn assert_error_contains(error: WorkflowError, expected: &[&str]) {
    let message = error.to_string();
    for needle in expected {
        assert!(
            message.contains(needle),
            "expected workflow error {message:?} to contain {needle:?}"
        );
    }
}

#[tokio::test]
async fn workflow_agent_calls_are_not_limited_by_the_default_quickjs_eval_timeout() {
    let (runtime, _fs) = runtime_with_worker(Arc::new(DelayedWorker {
        delay: Duration::from_millis(5500),
    }));
    let script = r#"
        export const meta = {
          name: "s053-long-agent-call",
          description: "agent calls can exceed the ordinary quickjs eval timeout"
        };

        export default async function workflow() {
          return await agent({ label: "slow-agent", task: "wait" });
        }
    "#;

    let run = runtime
        .start(inline_options("s053-long-agent-call", script))
        .await
        .expect("workflow should start")
        .wait()
        .await
        .expect("workflow should not fail at the ordinary five second JS timeout");

    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.results.len(), 1);
}

#[tokio::test]
async fn workflow_agent_host_function_returns_a_promise_resolving_to_typed_json() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        export const meta = {
          name: "s053-agent-promise",
          description: "agent host function is promise-compatible"
        };

        export default async function workflow(args) {
          const pending = agent({
            label: "typed-json",
            agent: "worker",
            task: "return structured data",
            input: args
          });
          if (typeof pending.then !== "function") {
            throw new Error("agent() did not return a Promise-compatible value");
          }
          const result = await pending;
          if (typeof result !== "object" || typeof result.output !== "object") {
            throw new Error(`agent result was not a typed object: ${typeof result}`);
          }
          if (!Array.isArray(result.output.items) || result.output.items[0].count !== 2) {
            throw new Error("agent result did not preserve nested JSON values");
          }
          return result;
        }
    "#;

    let run = runtime
        .start(inline_options("s053-agent-promise", script))
        .await
        .expect("workflow should start")
        .wait()
        .await
        .expect("workflow should complete");

    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(worker.calls().len(), 1);
    assert_eq!(
        run.results[0].output["items"][0]["nested"]["ok"],
        Value::Bool(true)
    );
    assert_eq!(run.results[0].output["input"]["seed"], "s053");
}

#[tokio::test]
async fn workflow_async_rejections_surface_the_javascript_rejection_message() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        export const meta = {
          name: "s053-rejection",
          description: "promise rejection propagation"
        };

        export default async function workflow() {
          await Promise.resolve();
          throw new Error("s053 workflow rejected promise");
        }
    "#;

    let error = start_or_wait_error(&runtime, inline_options("s053-rejection", script)).await;

    assert!(matches!(error, WorkflowError::InvalidScript(_)));
    assert_error_contains(error, &["s053 workflow rejected promise"]);
    assert!(
        worker.calls().is_empty(),
        "rejected workflow should not call workers after throwing"
    );
}

#[tokio::test]
async fn workflow_shared_profile_keeps_restricted_apis_unavailable_during_execution() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker);
    let script = r#"
        export const meta = {
          name: "s053-workflow-profile",
          description: "workflow uses restricted shared QuickJS profile"
        };

        export default async function workflow() {
          const forbidden = {
            fs: typeof globalThis.fs,
            fetch: typeof globalThis.fetch,
            process: typeof globalThis.process,
            Date: typeof globalThis.Date,
            random: typeof Math.random,
            performance: typeof globalThis.performance,
            require: typeof globalThis.require
          };
          const installed = Object.entries(forbidden)
            .filter(([, kind]) => kind !== "undefined")
            .map(([name]) => name);
          if (installed.length > 0) {
            throw new Error(`restricted APIs were installed: ${installed.join(",")}`);
          }
          const value = await Promise.resolve("shared-async-substrate");
          progress(value);
          return { value };
        }
    "#;

    let run = runtime
        .start(inline_options("s053-workflow-profile", script))
        .await
        .expect("workflow should start")
        .wait()
        .await
        .expect("workflow should complete");

    assert_eq!(run.status, WorkflowStatus::Completed);
    let events = runtime
        .events("s053-workflow-profile")
        .await
        .expect("workflow events should be readable");
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::Progress { message, .. } if message == "shared-async-substrate"
    )));
}

#[tokio::test]
async fn workflow_static_simulacra_imports_are_rejected_before_worker_execution() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        import { readFile } from "simulacra:fs";

        export const meta = {
          name: "s053-static-restricted-import",
          description: "static restricted imports are blocked by workflow profile"
        };

        export default async function workflow() {
          readFile("/workspace/secret.txt");
          return await agent({ label: "must-not-run" });
        }
    "#;

    let error = start_or_wait_error(
        &runtime,
        inline_options("s053-static-restricted-import", script),
    )
    .await;

    assert!(matches!(error, WorkflowError::InvalidScript(_)));
    assert_error_contains(error, &["simulacra:fs"]);
    assert!(
        worker.calls().is_empty(),
        "restricted static imports must fail before worker execution"
    );
}
