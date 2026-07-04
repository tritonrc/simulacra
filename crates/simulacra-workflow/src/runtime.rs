use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rquickjs::function::Async;
use rquickjs::{Ctx, Function};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use simulacra_quickjs::{JsError, JsRuntime};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::{
    WorkflowAgentCall, WorkflowAgentResult, WorkflowError, WorkflowEvent, WorkflowRun,
    WorkflowScriptMeta, WorkflowStatus, WorkflowStore, WorkflowWorker,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunOptions {
    pub run_id: Option<String>,
    pub script: Option<String>,
    pub name: Option<String>,
    pub script_path: Option<String>,
    #[serde(default)]
    pub args: Value,
    pub resume_from_run_id: Option<String>,
    #[serde(default = "default_concurrency_limit")]
    pub concurrency_limit: usize,
}

pub(crate) fn default_concurrency_limit() -> usize {
    16
}

#[derive(Clone)]
pub struct WorkflowRuntime {
    store: WorkflowStore,
    worker: Arc<dyn WorkflowWorker>,
    runs: Arc<Mutex<HashMap<String, WorkflowRunControl>>>,
    executor: WorkflowLocalExecutor,
}

#[derive(Clone)]
struct WorkflowRunControl {
    cancellation: CancellationToken,
    events: Arc<Mutex<Vec<WorkflowEvent>>>,
}

pub struct WorkflowRunHandle {
    run_id: String,
    script_path: String,
    transcript_dir: String,
    result_rx: oneshot::Receiver<Result<WorkflowRun, WorkflowError>>,
}

impl WorkflowRunHandle {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn script_path(&self) -> &str {
        &self.script_path
    }

    pub fn transcript_dir(&self) -> &str {
        &self.transcript_dir
    }

    pub fn status(&self) -> WorkflowStatus {
        WorkflowStatus::Running
    }

    pub async fn wait(self) -> Result<WorkflowRun, WorkflowError> {
        self.result_rx
            .await
            .map_err(|_| WorkflowError::Internal("workflow local executor stopped".into()))?
    }
}

#[derive(Clone)]
struct WorkflowLocalExecutor {
    sender: mpsc::UnboundedSender<WorkflowCommand>,
}

enum WorkflowCommand {
    EvaluateMetadata {
        source: String,
        reply: oneshot::Sender<Result<WorkflowScriptMeta, WorkflowError>>,
    },
    Run {
        context: Box<ExecutionContext>,
        reply: oneshot::Sender<Result<WorkflowRun, WorkflowError>>,
    },
}

impl WorkflowLocalExecutor {
    fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        std::thread::spawn(move || run_workflow_executor(receiver));
        Self { sender }
    }

    async fn evaluate_metadata(&self, source: String) -> Result<WorkflowScriptMeta, WorkflowError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(WorkflowCommand::EvaluateMetadata { source, reply })
            .map_err(|_| WorkflowError::Internal("workflow local executor stopped".into()))?;
        result
            .await
            .map_err(|_| WorkflowError::Internal("workflow metadata task was dropped".into()))?
    }

    fn spawn_workflow(
        &self,
        context: ExecutionContext,
    ) -> Result<oneshot::Receiver<Result<WorkflowRun, WorkflowError>>, WorkflowError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(WorkflowCommand::Run {
                context: Box::new(context),
                reply,
            })
            .map_err(|_| WorkflowError::Internal("workflow local executor stopped".into()))?;
        Ok(result)
    }
}

fn run_workflow_executor(mut receiver: mpsc::UnboundedReceiver<WorkflowCommand>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "failed to create workflow local executor");
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    runtime.block_on(local.run_until(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                WorkflowCommand::EvaluateMetadata { source, reply } => {
                    tokio::task::spawn_local(async move {
                        let _ = reply.send(evaluate_metadata_local(&source).await);
                    });
                }
                WorkflowCommand::Run { context, reply } => {
                    tokio::task::spawn_local(async move {
                        let _ = reply.send(execute_workflow(*context).await);
                    });
                }
            }
        }
    }));
}

impl WorkflowRuntime {
    pub fn new(store: WorkflowStore, worker: Arc<dyn WorkflowWorker>) -> Self {
        Self {
            store,
            worker,
            runs: Arc::new(Mutex::new(HashMap::new())),
            executor: WorkflowLocalExecutor::new(),
        }
    }

    pub async fn start(
        &self,
        options: WorkflowRunOptions,
    ) -> Result<WorkflowRunHandle, WorkflowError> {
        let run_id = options
            .run_id
            .clone()
            .unwrap_or_else(|| Ulid::new().to_string());
        let (script_path, source) = self.resolve_script(&run_id, &options)?;
        let meta = self.executor.evaluate_metadata(source.clone()).await?;
        let script = self
            .store
            .load_script(script_path.clone(), source.clone(), meta);
        let mut run = WorkflowRun::pending(run_id.clone(), &script);
        run.status = WorkflowStatus::Running;
        self.store.save_run(&run)?;

        let events = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CancellationToken::new();
        let control = WorkflowRunControl {
            cancellation: cancellation.clone(),
            events: Arc::clone(&events),
        };
        self.runs
            .lock()
            .map_err(|e| WorkflowError::Internal(format!("workflow run lock poisoned: {e}")))?
            .insert(run_id.clone(), control);

        push_event(
            &events,
            WorkflowEvent::RunStarted {
                run_id: run_id.clone(),
                script_path: script_path.clone(),
                name: run.meta.name.clone(),
            },
        )?;

        let context = ExecutionContext {
            run_id: run_id.clone(),
            source,
            args: options.args,
            script_path: script_path.clone(),
            meta: run.meta.clone(),
            store: self.store.clone(),
            worker: Arc::clone(&self.worker),
            events,
            cancellation,
            concurrency_limit: options.concurrency_limit.max(1),
            resume_from_run_id: options.resume_from_run_id,
        };
        let result_rx = self.executor.spawn_workflow(context)?;
        tokio::task::yield_now().await;

        Ok(WorkflowRunHandle {
            run_id: run_id.clone(),
            script_path,
            transcript_dir: WorkflowStore::transcript_dir(&run_id),
            result_rx,
        })
    }

    fn resolve_script(
        &self,
        run_id: &str,
        options: &WorkflowRunOptions,
    ) -> Result<(String, String), WorkflowError> {
        if let Some(path) = options.script_path.as_deref() {
            WorkflowStore::validate_script_path(path)?;
            return Ok((path.to_string(), self.store.read_script(path)?));
        }
        if let Some(name) = options.name.as_deref() {
            let path = WorkflowStore::script_path_for_name(name)?;
            return Ok((path.clone(), self.store.read_script(&path)?));
        }
        if let Some(script) = options.script.as_deref() {
            let path = self.store.persist_inline_script(run_id, script)?;
            return Ok((path, script.to_string()));
        }
        Err(WorkflowError::InvalidScript(
            "must provide script, name, or script_path".into(),
        ))
    }

    pub async fn cancel(&self, run_id: &str) -> Result<(), WorkflowError> {
        let control = self
            .runs
            .lock()
            .map_err(|e| WorkflowError::Internal(format!("workflow run lock poisoned: {e}")))?
            .get(run_id)
            .cloned()
            .ok_or_else(|| WorkflowError::RunNotFound {
                run_id: run_id.to_string(),
            })?;
        control.cancellation.cancel();
        Ok(())
    }

    pub async fn events(&self, run_id: &str) -> Result<Vec<WorkflowEvent>, WorkflowError> {
        let control = self
            .runs
            .lock()
            .map_err(|e| WorkflowError::Internal(format!("workflow run lock poisoned: {e}")))?
            .get(run_id)
            .cloned()
            .ok_or_else(|| WorkflowError::RunNotFound {
                run_id: run_id.to_string(),
            })?;
        Ok(control
            .events
            .lock()
            .map_err(|e| WorkflowError::Internal(format!("workflow event lock poisoned: {e}")))?
            .clone())
    }

    pub fn store(&self) -> WorkflowStore {
        self.store.clone()
    }
}

struct ExecutionContext {
    run_id: String,
    source: String,
    args: Value,
    script_path: String,
    meta: WorkflowScriptMeta,
    store: WorkflowStore,
    worker: Arc<dyn WorkflowWorker>,
    events: Arc<Mutex<Vec<WorkflowEvent>>>,
    cancellation: CancellationToken,
    concurrency_limit: usize,
    resume_from_run_id: Option<String>,
}

async fn evaluate_metadata_local(source: &str) -> Result<WorkflowScriptMeta, WorkflowError> {
    let result = eval_workflow_module(
        &format!(
            "{source}\nglobalThis.__simulacraWorkflowResult__ = JSON.stringify(typeof meta === 'undefined' ? null : meta);"
        ),
        |_| Ok(()),
    )
    .await?;
    let value: Value = serde_json::from_str(&result)?;
    if value.is_null() {
        return Err(WorkflowError::InvalidMetadata(
            "workflow meta must be exported as a non-empty object".into(),
        ));
    }
    let meta: WorkflowScriptMeta = serde_json::from_value(value)
        .map_err(|e| WorkflowError::InvalidMetadata(format!("workflow meta is invalid: {e}")))?;
    if meta.name.trim().is_empty() || meta.description.trim().is_empty() {
        return Err(WorkflowError::InvalidMetadata(
            "workflow meta name and description must be non-empty".into(),
        ));
    }
    Ok(meta)
}

async fn execute_workflow(context: ExecutionContext) -> Result<WorkflowRun, WorkflowError> {
    let results: Arc<Mutex<Vec<(usize, WorkflowAgentResult)>>> = Arc::new(Mutex::new(Vec::new()));
    let next_index = Arc::new(AtomicUsize::new(0));
    let semaphore = Arc::new(Semaphore::new(context.concurrency_limit));
    let cached = load_resume_cache(&context);

    let setup_context = JsWorkflowHost {
        run_id: context.run_id.clone(),
        store: context.store.clone(),
        worker: Arc::clone(&context.worker),
        events: Arc::clone(&context.events),
        cancellation: context.cancellation.clone(),
        semaphore,
        results: Arc::clone(&results),
        next_index,
        cached,
    };

    let args_json = serde_json::to_string(&context.args)?;
    let source = format!(
        "{}\nglobalThis.__simulacraWorkflowArgsJson = {};\nglobalThis.__simulacraWorkflowResult__ = JSON.stringify(await workflow(JSON.parse(globalThis.__simulacraWorkflowArgsJson)));",
        context.source,
        serde_json::to_string(&args_json)?,
    );

    let eval_result = eval_workflow_module(&source, move |ctx| {
        setup_workflow_globals(ctx, setup_context)
    })
    .await;

    let mut ordered = results
        .lock()
        .map_err(|e| WorkflowError::Internal(format!("workflow result lock poisoned: {e}")))?
        .clone();
    ordered.sort_by_key(|(idx, _)| *idx);
    let run_results = ordered
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();

    let mut run = WorkflowRun {
        run_id: context.run_id.clone(),
        script_path: context.script_path,
        meta: context.meta,
        status: WorkflowStatus::Completed,
        results: run_results,
        error: None,
    };

    let mut terminal_error = None;

    if context.cancellation.is_cancelled() {
        run.status = WorkflowStatus::Cancelled;
        push_event(
            &context.events,
            WorkflowEvent::RunCancelled {
                run_id: context.run_id.clone(),
            },
        )?;
    } else if let Err(err) = eval_result {
        run.status = WorkflowStatus::Failed;
        run.error = Some(err.to_string());
        push_event(
            &context.events,
            WorkflowEvent::RunFailed {
                run_id: context.run_id.clone(),
                error: err.to_string(),
            },
        )?;
        terminal_error = Some(err);
    } else if run.results.iter().any(|result| result.is_error) {
        run.status = WorkflowStatus::Failed;
        run.error = Some("one or more workflow workers failed".into());
        push_event(
            &context.events,
            WorkflowEvent::RunFailed {
                run_id: context.run_id.clone(),
                error: "one or more workflow workers failed".into(),
            },
        )?;
    } else {
        push_event(
            &context.events,
            WorkflowEvent::RunCompleted {
                run_id: context.run_id.clone(),
            },
        )?;
    }

    context.store.save_run(&run)?;
    if let Some(err) = terminal_error {
        return Err(err);
    }
    Ok(run)
}

fn load_resume_cache(context: &ExecutionContext) -> Arc<HashMap<String, WorkflowAgentResult>> {
    let mut cache = HashMap::new();
    if let Some(source_run_id) = context.resume_from_run_id.as_deref()
        && let Ok(previous) = context.store.read_run(source_run_id)
    {
        for result in previous.results {
            if !result.is_error
                && let Ok(saved) = context.store.read_agent_result(source_run_id, &result.key)
            {
                cache.insert(saved.key.clone(), saved);
            }
        }
    }
    Arc::new(cache)
}

struct JsWorkflowHost {
    run_id: String,
    store: WorkflowStore,
    worker: Arc<dyn WorkflowWorker>,
    events: Arc<Mutex<Vec<WorkflowEvent>>>,
    cancellation: CancellationToken,
    semaphore: Arc<Semaphore>,
    results: Arc<Mutex<Vec<(usize, WorkflowAgentResult)>>>,
    next_index: Arc<AtomicUsize>,
    cached: Arc<HashMap<String, WorkflowAgentResult>>,
}

impl Clone for JsWorkflowHost {
    fn clone(&self) -> Self {
        Self {
            run_id: self.run_id.clone(),
            store: self.store.clone(),
            worker: Arc::clone(&self.worker),
            events: Arc::clone(&self.events),
            cancellation: self.cancellation.clone(),
            semaphore: Arc::clone(&self.semaphore),
            results: Arc::clone(&self.results),
            next_index: Arc::clone(&self.next_index),
            cached: Arc::clone(&self.cached),
        }
    }
}

fn setup_workflow_globals(ctx: &Ctx<'_>, host: JsWorkflowHost) -> Result<(), WorkflowError> {
    let globals = ctx.globals();

    let agent_host = host.clone();
    let agent_fn = Function::new(
        ctx.clone(),
        Async(move |call_json: String| {
            let host = agent_host.clone();
            async move { host_agent_call(host, call_json).await }
        }),
    )
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;
    globals
        .set("__simulacra_agent_json", agent_fn)
        .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;

    let progress_host = host.clone();
    let progress_fn = Function::new(
        ctx.clone(),
        move |message: String| -> rquickjs::Result<()> {
            push_event(
                &progress_host.events,
                WorkflowEvent::Progress {
                    run_id: progress_host.run_id.clone(),
                    message,
                },
            )
            .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e.to_string()))
        },
    )
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;
    globals
        .set("__simulacra_progress", progress_fn)
        .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;

    let phase_start_host = host.clone();
    let phase_start = Function::new(ctx.clone(), move |name: String| -> rquickjs::Result<()> {
        push_event(
            &phase_start_host.events,
            WorkflowEvent::PhaseStarted {
                run_id: phase_start_host.run_id.clone(),
                name,
            },
        )
        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e.to_string()))
    })
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;
    globals
        .set("__simulacra_phase_start", phase_start)
        .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;

    let phase_end_host = host;
    let phase_end = Function::new(ctx.clone(), move |name: String| -> rquickjs::Result<()> {
        push_event(
            &phase_end_host.events,
            WorkflowEvent::PhaseCompleted {
                run_id: phase_end_host.run_id.clone(),
                name,
            },
        )
        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e.to_string()))
    })
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;
    globals
        .set("__simulacra_phase_end", phase_end)
        .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;

    ctx.eval::<(), _>(
        r#"
        globalThis.agent = async function(call) {
            return JSON.parse(await globalThis.__simulacra_agent_json(JSON.stringify(call ?? {})));
        };
        globalThis.parallel = async function(items) {
            return await Promise.all(items);
        };
        globalThis.pipeline = async function(items) {
            let last = null;
            for (const item of items) {
                last = await item;
            }
            return last;
        };
        globalThis.phase = async function(name, fn) {
            globalThis.__simulacra_phase_start(String(name));
            try {
                return await fn();
            } finally {
                globalThis.__simulacra_phase_end(String(name));
            }
        };
        globalThis.progress = function(message) {
            globalThis.__simulacra_progress(String(message));
        };
        "#,
    )
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))?;
    Ok(())
}

async fn host_agent_call(host: JsWorkflowHost, call_json: String) -> String {
    match host_agent_call_inner(host, call_json).await {
        Ok(value) => value,
        Err(err) => serde_json::to_string(&json!({
            "key": "workflow-error",
            "output": null,
            "is_error": true,
            "error": err.to_string(),
        }))
        .unwrap_or_else(|_| "{\"is_error\":true}".to_string()),
    }
}

async fn host_agent_call_inner(
    host: JsWorkflowHost,
    call_json: String,
) -> Result<String, WorkflowError> {
    if host.cancellation.is_cancelled() {
        return Err(WorkflowError::Cancelled);
    }

    let index = host.next_index.fetch_add(1, Ordering::SeqCst);
    let arguments: Value = serde_json::from_str(&call_json)
        .map_err(|e| WorkflowError::InvalidScript(format!("agent() argument is not JSON: {e}")))?;
    let key = arguments
        .get("label")
        .or_else(|| arguments.get("key"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("agent-{index}"));

    if let Some(cached) = host.cached.get(&key) {
        push_event(
            &host.events,
            WorkflowEvent::AgentCallCompleted {
                run_id: host.run_id.clone(),
                key: key.clone(),
                cached: true,
                is_error: cached.is_error,
            },
        )?;
        host.results
            .lock()
            .map_err(|e| WorkflowError::Internal(format!("workflow result lock poisoned: {e}")))?
            .push((index, cached.clone()));
        return Ok(serde_json::to_string(cached)?);
    }

    let call = WorkflowAgentCall {
        key: key.clone(),
        index,
        phase: None,
        agent: arguments
            .get("agent")
            .or_else(|| arguments.get("agent_type"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        task: arguments
            .get("task")
            .or_else(|| arguments.get("prompt"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        input: arguments.get("input").cloned().unwrap_or(Value::Null),
        arguments: arguments.get("arguments").cloned().unwrap_or(Value::Null),
    };

    push_event(
        &host.events,
        WorkflowEvent::AgentCallStarted {
            run_id: host.run_id.clone(),
            key: key.clone(),
            agent: call.agent.clone(),
            task: call.task.clone(),
        },
    )?;

    let permit = tokio::select! {
        _ = host.cancellation.cancelled() => return Err(WorkflowError::Cancelled),
        permit = host.semaphore.clone().acquire_owned() => permit
            .map_err(|e| WorkflowError::Internal(format!("workflow semaphore closed: {e}")))?,
    };

    let worker = Arc::clone(&host.worker);
    let mut worker_call = worker.call(call);
    let result = tokio::select! {
        _ = host.cancellation.cancelled() => {
            return Err(WorkflowError::Cancelled);
        }
        result = &mut worker_call => result?,
    };
    drop(permit);

    host.store.save_agent_result(&host.run_id, &result)?;
    push_event(
        &host.events,
        WorkflowEvent::AgentCallCompleted {
            run_id: host.run_id.clone(),
            key: key.clone(),
            cached: false,
            is_error: result.is_error,
        },
    )?;
    host.results
        .lock()
        .map_err(|e| WorkflowError::Internal(format!("workflow result lock poisoned: {e}")))?
        .push((index, result.clone()));
    Ok(serde_json::to_string(&result)?)
}

async fn eval_workflow_module<F>(source: &str, setup: F) -> Result<String, WorkflowError>
where
    F: for<'js> FnOnce(&Ctx<'js>) -> Result<(), WorkflowError> + Send + 'static,
{
    JsRuntime::eval_workflow_module_with_setup(source, move |ctx| {
        setup(ctx).map_err(|e| JsError::Execution(e.to_string()))
    })
    .await
    .map_err(|e| WorkflowError::InvalidScript(e.to_string()))
}

fn push_event(
    events: &Arc<Mutex<Vec<WorkflowEvent>>>,
    event: WorkflowEvent,
) -> Result<(), WorkflowError> {
    events
        .lock()
        .map_err(|e| WorkflowError::Internal(format!("workflow event lock poisoned: {e}")))?
        .push(event);
    Ok(())
}
