use std::collections::{HashMap, HashSet};
use std::future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use simulacra_workflow::{
    WorkflowAgentCall, WorkflowAgentResult, WorkflowError, WorkflowEvent, WorkflowRun,
    WorkflowRunOptions, WorkflowRuntime, WorkflowStatus, WorkflowStore, WorkflowWorker,
    WorkflowWorkerFuture,
};
use tokio::sync::Notify;

fn vfs() -> Arc<dyn VirtualFs> {
    Arc::new(MemoryFs::new())
}

fn runtime_with_worker(worker: Arc<dyn WorkflowWorker>) -> (WorkflowRuntime, Arc<dyn VirtualFs>) {
    let fs = vfs();
    let store = WorkflowStore::new(Arc::clone(&fs));
    (WorkflowRuntime::new(store, worker), fs)
}

fn inline_options(run_id: &str, script: &str) -> WorkflowRunOptions {
    inline_options_with_args(run_id, script, json!({}))
}

fn inline_options_with_args(run_id: &str, script: &str, args: Value) -> WorkflowRunOptions {
    WorkflowRunOptions {
        run_id: Some(run_id.to_string()),
        script: Some(script.to_string()),
        name: None,
        script_path: None,
        args,
        resume_from_run_id: None,
        concurrency_limit: 4,
    }
}

fn saved_options(run_id: &str, path: &str) -> WorkflowRunOptions {
    WorkflowRunOptions {
        run_id: Some(run_id.to_string()),
        script: None,
        name: None,
        script_path: Some(path.to_string()),
        args: json!({}),
        resume_from_run_id: None,
        concurrency_limit: 4,
    }
}

fn named_options(run_id: &str, name: &str) -> WorkflowRunOptions {
    WorkflowRunOptions {
        run_id: Some(run_id.to_string()),
        script: None,
        name: Some(name.to_string()),
        script_path: None,
        args: json!({}),
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
            Ok(run) => panic!("workflow should have failed, but finished with {run:?}"),
            Err(err) => err,
        },
        Err(err) => err,
    }
}

fn read_utf8(fs: &Arc<dyn VirtualFs>, path: &str) -> String {
    String::from_utf8(
        fs.read(path)
            .expect("VFS file should exist and be readable"),
    )
    .expect("VFS file should contain UTF-8")
}

fn read_json(fs: &Arc<dyn VirtualFs>, path: &str) -> Value {
    serde_json::from_str(&read_utf8(fs, path)).expect("VFS file should contain JSON")
}

fn valid_agent_script(name: &str, label: &str) -> String {
    format!(
        r#"
        export const meta = {{ name: "{name}", description: "exercise one worker" }};

        export default async function workflow(args) {{
          return await agent({{
            label: "{label}",
            agent: "worker",
            task: `run {label}`,
            input: args
          }});
        }}
        "#
    )
}

#[derive(Default)]
struct RecordingWorker {
    calls: Mutex<Vec<WorkflowAgentCall>>,
    responses: Mutex<HashMap<String, WorkflowAgentResult>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    delay: Mutex<Option<Duration>>,
}

impl RecordingWorker {
    fn with_delay(delay: Duration) -> Self {
        Self {
            delay: Mutex::new(Some(delay)),
            ..Self::default()
        }
    }

    fn set_response(&self, key: &str, result: WorkflowAgentResult) {
        self.responses
            .lock()
            .expect("responses lock should not be poisoned")
            .insert(key.to_string(), result);
    }

    fn calls(&self) -> Vec<WorkflowAgentCall> {
        self.calls
            .lock()
            .expect("calls lock should not be poisoned")
            .clone()
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }
}

impl WorkflowWorker for RecordingWorker {
    fn call<'a>(&'a self, call: WorkflowAgentCall) -> WorkflowWorkerFuture<'a> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            loop {
                let current = self.max_active.load(Ordering::SeqCst);
                if active <= current
                    || self
                        .max_active
                        .compare_exchange(current, active, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    break;
                }
            }

            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .push(call.clone());

            let delay = *self
                .delay
                .lock()
                .expect("delay lock should not be poisoned");
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }

            self.active.fetch_sub(1, Ordering::SeqCst);
            let response = self
                .responses
                .lock()
                .expect("responses lock should not be poisoned")
                .get(&call.key)
                .cloned()
                .unwrap_or_else(|| {
                    WorkflowAgentResult::success(
                        call.key.clone(),
                        json!({
                            "key": call.key,
                            "agent": call.agent,
                            "task": call.task,
                            "input": call.input,
                            "arguments": call.arguments,
                        }),
                    )
                });
            Ok(response)
        })
    }
}

#[derive(Default)]
struct BlockingWorker {
    started: Notify,
    dropped: AtomicUsize,
    calls: AtomicUsize,
}

impl BlockingWorker {
    async fn wait_until_started(&self) {
        loop {
            if self.calls.load(Ordering::SeqCst) > 0 {
                return;
            }
            let notified = self.started.notified();
            if self.calls.load(Ordering::SeqCst) > 0 {
                return;
            }
            notified.await;
        }
    }

    fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl WorkflowWorker for BlockingWorker {
    fn call<'a>(&'a self, _call: WorkflowAgentCall) -> WorkflowWorkerFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();

            struct DropCounter<'a>(&'a AtomicUsize);
            impl Drop for DropCounter<'_> {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }

            let _drop_counter = DropCounter(&self.dropped);
            future::pending::<()>().await;
            unreachable!("blocking worker should only finish when cancelled")
        })
    }
}

#[tokio::test]
async fn workflow_scripts_execute_as_quickjs_esm_with_standard_js_semantics() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        const suffix = ["quick", "js"].join("-");

        export const meta = {
          name: `workflow-${suffix}`,
          description: `ESM ${suffix} execution with promises and computed values`
        };

        export default async function workflow(args) {
          const normalized = await Promise.resolve(args?.seed?.toUpperCase());
          const labels = ["first", "second"];
          const calls = labels.map((label, index) => agent({
            label,
            agent: "worker",
            task: `${label}:${normalized}:${index}`,
            input: {
              normalized,
              index,
              doubled: [1, 2, 3].map((n) => n * 2)
            },
            arguments: {
              spread: { ...args, index },
              hasJson: typeof JSON.parse === "function"
            }
          }));
          return await parallel(calls);
        }
    "#;

    let run = runtime
        .start(inline_options_with_args(
            "run-quickjs-esm",
            script,
            json!({"seed": "oak", "extra": true}),
        ))
        .await
        .expect("QuickJS ESM workflow should start")
        .wait()
        .await
        .expect("QuickJS ESM workflow should complete");

    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.meta.name, "workflow-quick-js");
    assert_eq!(
        worker
            .calls()
            .iter()
            .map(|call| (call.key.as_str(), call.task.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("first", Some("first:OAK:0")),
            ("second", Some("second:OAK:1"))
        ]
    );
    assert_eq!(worker.calls()[0].input["doubled"], json!([2, 4, 6]));
    assert_eq!(worker.calls()[1].arguments["spread"]["extra"], true);
}

#[tokio::test]
async fn workflow_quickjs_profile_installs_helpers_but_not_ambient_capabilities() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        export let meta = {
          name: "workflow-profile",
          description: "workflow QuickJS profile exposes only orchestration helpers"
        };

        export default async function workflow() {
          const helperTypes = [agent, parallel, pipeline, phase, progress]
            .map((helper) => typeof helper);
          if (!helperTypes.every((kind) => kind === "function")) {
            throw new Error(`workflow helpers were not installed: ${helperTypes.join(",")}`);
          }

          const forbidden = {
            fetch: typeof globalThis.fetch,
            process: typeof globalThis.process,
            require: typeof globalThis.require,
            Date: typeof globalThis.Date,
            performance: typeof globalThis.performance,
            random: typeof Math.random
          };
          const installed = Object.entries(forbidden)
            .filter(([, kind]) => kind !== "undefined")
            .map(([name]) => name);
          if (installed.length > 0) {
            throw new Error(`workflow profile installed forbidden APIs: ${installed.join(",")}`);
          }

          progress("helpers-ready");
          return await phase("profile", async () => pipeline([
            agent({ label: "first", agent: "worker", task: "first" }),
            agent({ label: "second", agent: "worker", task: "second" })
          ]));
        }
    "#;

    let run = runtime
        .start(inline_options("run-profile", script))
        .await
        .expect("workflow should start with the workflow QuickJS profile")
        .wait()
        .await
        .expect("helpers-only workflow should complete");

    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(
        worker
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    let events = runtime
        .events("run-profile")
        .await
        .expect("workflow events should be readable");
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::Progress { message, .. } if message == "helpers-ready"
    )));
}

#[tokio::test]
async fn workflow_scripts_require_non_empty_const_or_let_meta() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());

    for script in [
        r#"export default async function workflow() { return await agent({ label: "missing" }); }"#,
        r#"export const meta = { name: "", description: "missing name" };
           export default async function workflow() { return await agent({ label: "empty-name" }); }"#,
        r#"export let meta = { name: "empty description", description: "" };
           export default async function workflow() { return await agent({ label: "empty-desc" }); }"#,
    ] {
        let err = start_or_wait_error(&runtime, inline_options("meta-invalid", script)).await;
        assert!(
            matches!(err, WorkflowError::InvalidMetadata(_)),
            "expected metadata error, got {err:?}"
        );
    }
    assert!(
        worker.calls().is_empty(),
        "metadata failures must happen before worker execution"
    );

    let run = runtime
        .start(inline_options(
            "meta-valid-let",
            r#"
            export let meta = { name: "let-meta", description: "let meta is accepted" };
            export default async function workflow() {
              return await agent({ label: "accepted", agent: "worker", task: "ok" });
            }
            "#,
        ))
        .await
        .expect("export let meta should be accepted")
        .wait()
        .await
        .expect("valid let-meta workflow should complete");
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.meta.name, "let-meta");
    assert_eq!(worker.calls().len(), 1);
}

#[tokio::test]
async fn restricted_modules_and_ambient_apis_fail_before_worker_execution() {
    let cases = [
        ("fetch", "await fetch('https://example.invalid')"),
        ("filesystem import", "await import('fs')"),
        ("node filesystem import", "await import('node:fs')"),
        ("shell module", "await import('shell')"),
        ("process", "process.env.SECRET"),
        ("simulacra module", "await import('simulacra:fs')"),
        ("date constructor", "new Date()"),
        ("date now", "Date.now()"),
        ("random", "Math.random()"),
        ("performance now", "performance.now()"),
    ];

    for (name, forbidden) in cases {
        let worker = Arc::new(RecordingWorker::default());
        let (runtime, _fs) = runtime_with_worker(worker.clone());
        let script = format!(
            r#"
            export const meta = {{
              name: "restricted-{name}",
              description: "reject restricted APIs in the workflow QuickJS profile"
            }};
            export default async function workflow() {{
              {forbidden};
              return await agent({{ label: "must-not-run" }});
            }}
            "#
        );

        let err = start_or_wait_error(
            &runtime,
            inline_options(&format!("restricted-{name}"), &script),
        )
        .await;
        assert!(
            matches!(err, WorkflowError::InvalidScript(_)),
            "{name} should be rejected as an invalid workflow script, got {err:?}"
        );
        assert!(
            worker.calls().is_empty(),
            "{name} restriction should be enforced before worker execution"
        );
    }
}

#[tokio::test]
async fn inline_runs_persist_script_state_and_worker_transcripts_to_vfs() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, fs) = runtime_with_worker(worker);
    let script = valid_agent_script("persisted-inline", "alpha");

    let handle = runtime
        .start(inline_options("run-persist", &script))
        .await
        .expect("workflow should start");
    assert_eq!(handle.run_id(), "run-persist");
    assert_eq!(
        handle.script_path(),
        "/var/workflows/runs/run-persist/workflow.mjs"
    );
    assert_eq!(
        handle.transcript_dir(),
        "/var/workflows/runs/run-persist/agents"
    );

    let run = handle.wait().await.expect("workflow should complete");
    assert_eq!(run.status, WorkflowStatus::Completed);

    assert_eq!(
        read_utf8(&fs, "/var/workflows/runs/run-persist/workflow.mjs"),
        script
    );
    let state = read_json(&fs, "/var/workflows/runs/run-persist/state.json");
    assert_eq!(state["run_id"], "run-persist");
    assert_eq!(state["status"], "Completed");
    assert_eq!(
        state["script_path"],
        "/var/workflows/runs/run-persist/workflow.mjs"
    );

    let agent = read_json(&fs, "/var/workflows/runs/run-persist/agents/alpha.json");
    assert_eq!(agent["key"], "alpha");
    assert_eq!(agent["is_error"], false);
    assert_eq!(agent["output"]["task"], "run alpha");
}

#[tokio::test]
async fn saved_workflow_paths_validate_and_script_path_takes_precedence() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, fs) = runtime_with_worker(worker.clone());
    let saved_script = valid_agent_script("saved", "saved-label");
    fs.write("/workflows/saved.mjs", saved_script.as_bytes())
        .expect("saved workflow should be written to MemoryFs");

    let mut options = saved_options("run-saved", "/workflows/saved.mjs");
    options.script = Some(
        r#"export const meta = { name: "ignored", description: "ignored inline script" };
           export default async function workflow() {
             return await agent({ label: "inline-should-not-run" });
           }"#
        .to_string(),
    );
    let run = runtime
        .start(options)
        .await
        .expect("valid saved workflow path should start")
        .wait()
        .await
        .expect("saved workflow should complete");
    assert_eq!(run.script_path, "/workflows/saved.mjs");
    assert_eq!(
        worker
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["saved-label"]
    );

    for invalid in [
        "../escape.mjs",
        "/tmp/host.mjs",
        "/workflows/../secret.mjs",
        "/workflows/not-js.txt",
        "/workflows/nul\0byte.mjs",
    ] {
        let err = start_or_wait_error(&runtime, saved_options("run-invalid-path", invalid)).await;
        assert!(
            matches!(err, WorkflowError::InvalidScriptPath { .. }),
            "{invalid:?} should be an invalid script path, got {err:?}"
        );
    }
}

#[tokio::test]
async fn saved_workflow_name_resolves_from_workflows_dir_and_script_source_is_required() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, fs) = runtime_with_worker(worker.clone());
    let saved_script = valid_agent_script("named", "named-label");
    fs.write("/workflows/named.mjs", saved_script.as_bytes())
        .expect("named workflow should be written to MemoryFs");

    let run = runtime
        .start(named_options("run-named", "named"))
        .await
        .expect("workflow name should resolve to /workflows/<name>.mjs")
        .wait()
        .await
        .expect("named workflow should complete");
    assert_eq!(run.script_path, "/workflows/named.mjs");
    assert_eq!(
        worker
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["named-label"]
    );

    let err = start_or_wait_error(
        &runtime,
        WorkflowRunOptions {
            run_id: Some("run-no-source".to_string()),
            script: None,
            name: None,
            script_path: None,
            args: json!({}),
            resume_from_run_id: None,
            concurrency_limit: 4,
        },
    )
    .await;
    assert!(
        matches!(err, WorkflowError::InvalidScript(_)),
        "workflow start should require script, name, or script_path; got {err:?}"
    );
}

#[tokio::test]
async fn orchestration_helpers_emit_events_without_changing_result_semantics() {
    let worker = Arc::new(RecordingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        export const meta = { name: "helpers", description: "exercise orchestration helpers" };

        export default async function workflow(args) {
          progress(`queued:${args.batch}`);
          const planned = await phase("plan", async () =>
            agent({ label: "plan", agent: "planner", task: `plan ${args.batch}`, input: args })
          );
          const built = await parallel([
            agent({ label: "build-a", agent: "builder", task: "build a", input: planned.output }),
            agent({ label: "build-b", agent: "builder", task: "build b", input: planned.output })
          ]);
          progress("built");
          return await pipeline([
            agent({ label: "review", agent: "reviewer", task: "review", input: built }),
            agent({ label: "final", agent: "writer", task: "final" })
          ]);
        }
    "#;

    let run = runtime
        .start(inline_options_with_args(
            "run-helpers",
            script,
            json!({"batch": "b17"}),
        ))
        .await
        .expect("workflow should start")
        .wait()
        .await
        .expect("workflow should complete");
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(
        worker
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["plan", "build-a", "build-b", "review", "final"]
    );
    assert_eq!(
        run.results
            .iter()
            .map(|result| result.key.as_str())
            .collect::<Vec<_>>(),
        ["plan", "build-a", "build-b", "review", "final"]
    );

    let events = runtime
        .events("run-helpers")
        .await
        .expect("workflow events should be readable");
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::Progress { message, .. } if message == "queued:b17"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::PhaseStarted { name, .. } if name == "plan"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::PhaseCompleted { name, .. } if name == "plan"
    )));
}

#[tokio::test]
async fn parallel_respects_configured_concurrency_limit_and_preserves_input_order() {
    let worker = Arc::new(RecordingWorker::with_delay(Duration::from_millis(25)));
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = r#"
        export const meta = { name: "fanout", description: "bounded parallelism" };

        export default async function workflow() {
          return await parallel([
            agent({ label: "one", agent: "worker", task: "1" }),
            agent({ label: "two", agent: "worker", task: "2" }),
            agent({ label: "three", agent: "worker", task: "3" }),
            agent({ label: "four", agent: "worker", task: "4" })
          ]);
        }
    "#;
    let mut options = inline_options("run-fanout", script);
    options.concurrency_limit = 2;

    let run = runtime
        .start(options)
        .await
        .expect("workflow should start")
        .wait()
        .await
        .expect("workflow should complete");
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert!(
        worker.max_active() <= 2,
        "parallel should not exceed the configured concurrency limit; saw {}",
        worker.max_active()
    );
    assert_eq!(
        run.results
            .iter()
            .map(|result| result.key.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "three", "four"],
        "parallel results must remain in input order even when workers complete concurrently"
    );
}

#[tokio::test]
async fn cancellation_marks_run_cancelled_and_does_not_fabricate_successes() {
    let worker = Arc::new(BlockingWorker::default());
    let (runtime, fs) = runtime_with_worker(worker.clone());
    let script = valid_agent_script("cancel", "blocked");

    let handle = runtime
        .start(inline_options("run-cancel", &script))
        .await
        .expect("workflow should start promptly");
    worker.wait_until_started().await;

    runtime
        .cancel("run-cancel")
        .await
        .expect("workflow cancellation should be accepted");
    let run = handle
        .wait()
        .await
        .expect("cancelled workflow should resolve to a run state");
    assert_eq!(run.status, WorkflowStatus::Cancelled);
    assert!(
        run.results.is_empty(),
        "cancelled workers must not be reported as successful"
    );
    assert!(
        worker.dropped_count() > 0,
        "active worker future should be cancelled through the worker boundary"
    );

    let state = read_json(&fs, "/var/workflows/runs/run-cancel/state.json");
    assert_eq!(state["status"], "Cancelled");
    assert!(
        !fs.exists("/var/workflows/runs/run-cancel/agents/blocked.json"),
        "cancelled active workers must not get fabricated transcript files"
    );
}

#[tokio::test]
async fn resume_reuses_completed_calls_and_reruns_changed_missing_or_failed_calls() {
    let first_worker = Arc::new(RecordingWorker::default());
    first_worker.set_response(
        "failed",
        WorkflowAgentResult::failure("failed", "previous failure"),
    );
    let (runtime, fs) = runtime_with_worker(first_worker.clone());
    let first_script = r#"
        export const meta = { name: "resume", description: "first run has mixed results" };
        export default async function workflow() {
          return await parallel([
            agent({ label: "stable", agent: "worker", task: "same" }),
            agent({ label: "failed", agent: "worker", task: "will fail" }),
            agent({ label: "changed-old", agent: "worker", task: "old" })
          ]);
        }
    "#;

    let first = runtime
        .start(inline_options("resume-source", first_script))
        .await
        .expect("first workflow should start")
        .wait()
        .await
        .expect("first workflow should finish with persisted state");
    assert_eq!(first.status, WorkflowStatus::Failed);

    fs.remove("/var/workflows/runs/resume-source/agents/changed-old.json")
        .expect("test setup removes one transcript to simulate a missing cached call");

    let second_worker = Arc::new(RecordingWorker::default());
    let second_runtime =
        WorkflowRuntime::new(WorkflowStore::new(Arc::clone(&fs)), second_worker.clone());
    let second_script = r#"
        export const meta = { name: "resume", description: "second run resumes cached work" };
        export default async function workflow() {
          return await parallel([
            agent({ label: "stable", agent: "worker", task: "same" }),
            agent({ label: "failed", agent: "worker", task: "will fail" }),
            agent({ label: "changed-new", agent: "worker", task: "new" }),
            agent({ label: "missing", agent: "worker", task: "not present before" })
          ]);
        }
    "#;
    let mut options = inline_options("resume-target", second_script);
    options.resume_from_run_id = Some("resume-source".to_string());

    let resumed = second_runtime
        .start(options)
        .await
        .expect("resume workflow should start")
        .wait()
        .await
        .expect("resume workflow should complete");
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    assert!(
        resumed.results.iter().any(|result| result.key == "stable"),
        "completed stable result should be present in resumed run"
    );

    let rerun_keys: HashSet<String> = second_worker
        .calls()
        .into_iter()
        .map(|call| call.key)
        .collect();
    assert!(
        !rerun_keys.contains("stable"),
        "matching completed calls should be reused from the previous run"
    );
    assert_eq!(
        rerun_keys,
        HashSet::from([
            "failed".to_string(),
            "changed-new".to_string(),
            "missing".to_string()
        ]),
        "changed, missing, and previously failed calls must rerun"
    );

    let events = second_runtime
        .events("resume-target")
        .await
        .expect("resume events should be available");
    assert!(events.iter().any(|event| matches!(
        event,
        WorkflowEvent::AgentCallCompleted { key, cached: true, .. } if key == "stable"
    )));
}

#[tokio::test]
async fn workflow_events_serialize_and_map_to_server_sse_payloads() {
    let event = WorkflowEvent::AgentCallStarted {
        run_id: "run-events".to_string(),
        key: "research".to_string(),
        agent: Some("researcher".to_string()),
        task: Some("collect evidence".to_string()),
    };
    let serialized = serde_json::to_value(&event).expect("workflow event should serialize");
    assert_eq!(serialized["type"], "agent_call_started");
    assert_eq!(serialized["run_id"], "run-events");
    assert_eq!(serialized["key"], "research");

    let sse = event.to_sse_json(42);
    assert_eq!(sse["event"], "workflow.agent_start");
    assert_eq!(sse["run_id"], "run-events");
    assert_eq!(sse["seq"], 42);
    assert_eq!(sse["agent_label"], "research");
    assert_eq!(sse["status"], "running");
}

#[tokio::test]
async fn workflow_start_returns_promptly_with_metadata_while_workers_continue() {
    let worker = Arc::new(BlockingWorker::default());
    let (runtime, _fs) = runtime_with_worker(worker.clone());
    let script = valid_agent_script("prompt", "slow-worker");

    let handle = runtime
        .start(inline_options("run-prompt", &script))
        .await
        .expect("start should return once the workflow run is accepted");
    assert_eq!(handle.run_id(), "run-prompt");
    assert_eq!(handle.status(), WorkflowStatus::Running);
    assert_eq!(
        handle.script_path(),
        "/var/workflows/runs/run-prompt/workflow.mjs"
    );
    assert_eq!(
        handle.transcript_dir(),
        "/var/workflows/runs/run-prompt/agents"
    );

    worker.wait_until_started().await;
    runtime
        .cancel("run-prompt")
        .await
        .expect("cleanup cancellation should be accepted");
    let final_run = handle
        .wait()
        .await
        .expect("cancelled prompt run should join");
    assert_eq!(final_run.status, WorkflowStatus::Cancelled);
}

#[test]
fn workflow_run_state_and_events_are_serializable_contract_types() {
    fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
    assert_serde::<WorkflowAgentCall>();
    assert_serde::<WorkflowAgentResult>();
    assert_serde::<WorkflowEvent>();
    assert_serde::<WorkflowRun>();
    assert_serde::<WorkflowStatus>();

    let run = WorkflowRun {
        run_id: "run-serde".to_string(),
        script_path: "/workflows/serde.mjs".to_string(),
        meta: simulacra_workflow::WorkflowScriptMeta {
            name: "serde".to_string(),
            description: "serializable run state".to_string(),
        },
        status: WorkflowStatus::Completed,
        results: vec![WorkflowAgentResult::success("agent", json!({"ok": true}))],
        error: None,
    };
    let encoded = serde_json::to_string(&run).expect("run state should serialize");
    let decoded: WorkflowRun = serde_json::from_str(&encoded).expect("run state should roundtrip");
    assert_eq!(decoded.run_id, "run-serde");
    assert_eq!(decoded.results[0].key, "agent");
}
