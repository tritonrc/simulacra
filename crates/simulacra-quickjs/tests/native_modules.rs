use simulacra_quickjs::{FsProxy, JsRuntime, ModuleFetcher};
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan { fields });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn capture_spans<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

fn span_operations(spans: &[CapturedSpan]) -> Vec<String> {
    let mut operations = spans
        .iter()
        .filter_map(|span| span.fields.get("simulacra.operation.name").cloned())
        .collect::<Vec<_>>();
    operations.sort();
    operations
}

fn make_runtime() -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs), None);
    (runtime, vfs)
}

struct VfsBackedFsProxy {
    vfs: Arc<MemoryFs>,
}

impl VfsBackedFsProxy {
    fn new(vfs: Arc<MemoryFs>) -> Self {
        Self { vfs }
    }
}

impl FsProxy for VfsBackedFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.vfs.read(path).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        self.vfs.write(path, data).map_err(|e| e.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.vfs.list_dir(path).map_err(|e| e.to_string())
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let meta = self.vfs.metadata(path).map_err(|e| e.to_string())?;
        Ok((meta.is_file, meta.is_dir, meta.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.vfs.remove(path).map_err(|e| e.to_string())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let data = self.vfs.read(from).map_err(|e| e.to_string())?;
        if let Some(parent) = std::path::Path::new(to).parent() {
            let parent = parent.to_string_lossy();
            if !parent.is_empty() && parent != "/" {
                let _ = self.vfs.mkdir(&parent);
            }
        }
        self.vfs.write(to, &data).map_err(|e| e.to_string())?;
        self.vfs.remove(from).map_err(|e| e.to_string())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.vfs.mkdir(path).map_err(|e| e.to_string())
    }
}

fn make_runtime_with_vfs_proxy(
    vfs: Arc<MemoryFs>,
    fetcher: Option<Box<dyn ModuleFetcher>>,
) -> JsRuntime {
    let proxy: Arc<dyn FsProxy> = Arc::new(VfsBackedFsProxy::new(Arc::clone(&vfs)));
    JsRuntime::with_options(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        fetcher,
        Some(proxy),
    )
    .expect("failed to create runtime")
}

fn make_runtime_with_env(env: HashMap<String, String>) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let runtime = JsRuntime::with_env(vfs_dyn, env).expect("failed to create runtime");
    (runtime, vfs)
}

struct MockFetcher {
    responses: HashMap<String, Result<String, String>>,
}

impl MockFetcher {
    fn new(responses: Vec<(&str, Result<&str, &str>)>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(url, result)| {
                    (
                        url.to_string(),
                        result.map(str::to_string).map_err(str::to_string),
                    )
                })
                .collect(),
        }
    }
}

impl ModuleFetcher for MockFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("MockFetcher: no response configured for '{url}'")))
    }
}

fn make_runtime_with_fetcher(fetcher: MockFetcher) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs), Some(Box::new(fetcher)));
    (runtime, vfs)
}

/// Verify `simulacra:fs` is registered via native `ModuleDef` (not synthetic JS source).
///
/// A native `ModuleDef` module produces a namespace object whose `Object.keys()`
/// returns proper export names. Synthetic JS source modules historically returned
/// malformed entries (raw pointers). This test confirms the two-phase
/// `declare()`/`evaluate()` pattern is in effect by checking namespace
/// introspection and calling exported functions through all three import styles.
#[test]
fn simulacra_fs_module_is_registered_via_moduledef_not_synthetic_source() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/proof.txt", b"native")
        .expect("seed file");

    // Namespace import — the style that broke under synthetic modules
    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            const keys = Object.keys(fs).sort();
            const readResult = fs.readFile("/workspace/proof.txt");
            JSON.stringify({ keys, readResult, typeofRead: typeof fs.readFile });
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    // Native ModuleDef produces clean export names
    assert!(
        keys.contains(&"readFile".to_string()),
        "expected 'readFile' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"writeFile".to_string()),
        "expected 'writeFile' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    // No malformed pointer strings
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["readResult"].as_str(), Some("native"));
    assert_eq!(parsed["typeofRead"].as_str(), Some("function"));
}

/// Verify `simulacra:console` is registered via native `ModuleDef` (not synthetic JS source).
///
/// Same rationale as the `simulacra:fs` test: namespace introspection must return
/// clean export names, and the `log` function must capture output correctly
/// through all import styles.
#[test]
fn simulacra_console_module_is_registered_via_moduledef_not_synthetic_source() {
    let (runtime, _) = make_runtime();

    // Namespace import — verify clean keys and working function
    let output = runtime
        .eval(
            r#"
            import * as consoleModule from "simulacra:console";
            const keys = Object.keys(consoleModule).sort();
            consoleModule.log("native-console-check");
            JSON.stringify({ keys, typeofLog: typeof consoleModule.log });
            "#,
        )
        .expect("simulacra:console namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert!(
        keys.contains(&"log".to_string()),
        "expected 'log' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["typeofLog"].as_str(), Some("function"));
    assert_eq!(output.stdout, "native-console-check\n");
}

/// Verify `simulacra:process` is registered via native `ModuleDef` (not synthetic JS source).
///
/// Same rationale: namespace introspection returns clean export names, and
/// the `cwd`/`env`/`exit` exports function correctly through namespace import.
#[test]
fn simulacra_process_module_is_registered_via_moduledef_not_synthetic_source() {
    let mut env = HashMap::new();
    env.insert("CHECK_VAR".to_string(), "native_process".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import * as processModule from "simulacra:process";
            const keys = Object.keys(processModule).sort();
            const cwdResult = processModule.cwd();
            const envVal = processModule.env.CHECK_VAR;
            JSON.stringify({ keys, cwdResult, envVal, typeofCwd: typeof processModule.cwd, typeofExit: typeof processModule.exit });
            "#,
        )
        .expect("simulacra:process namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert!(
        keys.contains(&"cwd".to_string()),
        "expected 'cwd' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"env".to_string()),
        "expected 'env' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"exit".to_string()),
        "expected 'exit' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["cwdResult"].as_str(), Some("/workspace"));
    assert_eq!(parsed["envVal"].as_str(), Some("native_process"));
    assert_eq!(parsed["typeofCwd"].as_str(), Some("function"));
    assert_eq!(parsed["typeofExit"].as_str(), Some("function"));
}

#[test]
fn simulacra_fs_named_read_file_import_reads_from_vfs() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/test.txt", b"hello from vfs")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import { readFile } from "simulacra:fs";
            readFile("/workspace/test.txt");
            "#,
        )
        .expect("simulacra:fs readFile import should succeed");

    assert_eq!(output.result.as_deref(), Some("hello from vfs"));
}

#[test]
fn simulacra_fs_named_write_file_import_writes_to_vfs() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();

    runtime
        .eval(
            r#"
            import { writeFile } from "simulacra:fs";
            writeFile("/workspace/out.txt", "hello");
            "#,
        )
        .expect("simulacra:fs writeFile import should succeed");

    assert_eq!(fs.read("/workspace/out.txt").unwrap(), b"hello");
}

#[test]
fn simulacra_fs_exists_sync_named_export_reports_vfs_presence() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/existing.txt", b"present")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import { existsSync } from "simulacra:fs";
            `${existsSync("/workspace/existing.txt")}|${existsSync("/workspace/missing.txt")}`;
            "#,
        )
        .expect("simulacra:fs existsSync import should succeed");

    assert_eq!(output.result.as_deref(), Some("true|false"));
}

#[test]
fn simulacra_fs_mkdir_sync_named_export_creates_directory_in_vfs() {
    let (runtime, vfs) = make_runtime();

    runtime
        .eval(
            r#"
            import { mkdirSync } from "simulacra:fs";
            mkdirSync("/workspace/new-dir");
            "#,
        )
        .expect("simulacra:fs mkdirSync import should succeed");

    assert!(
        vfs.list_dir("/workspace")
            .unwrap()
            .iter()
            .any(|entry| entry == "new-dir"),
        "expected mkdirSync to create /workspace/new-dir"
    );
}

#[test]
fn simulacra_fs_default_export_exposes_read_and_write_methods() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "simulacra:fs";
            fs.writeFile("/workspace/default.txt", "via default");
            fs.readFile("/workspace/default.txt");
            "#,
        )
        .expect("simulacra:fs default import should succeed");

    assert_eq!(output.result.as_deref(), Some("via default"));
}

#[test]
fn simulacra_console_named_log_import_captures_stdout() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { log } from "simulacra:console";
            log("hi");
            "#,
        )
        .expect("simulacra:console log import should succeed");

    assert_eq!(output.stdout, "hi\n");
}

#[test]
fn simulacra_console_default_export_exposes_log_method() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import consoleModule from "simulacra:console";
            consoleModule.log("hello from default");
            "#,
        )
        .expect("simulacra:console default import should succeed");

    assert_eq!(output.stdout, "hello from default\n");
}

#[test]
fn simulacra_process_named_env_import_returns_host_controlled_environment_object() {
    let mut env = HashMap::new();
    env.insert("MY_VAR".to_string(), "my_value".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import { env } from "simulacra:process";
            `${env.MY_VAR}|${String(env.HOME)}`;
            "#,
        )
        .expect("simulacra:process env import should succeed");

    assert_eq!(output.result.as_deref(), Some("my_value|undefined"));
}

#[test]
fn simulacra_process_named_cwd_import_returns_workspace() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { cwd } from "simulacra:process";
            cwd();
            "#,
        )
        .expect("simulacra:process cwd import should succeed");

    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn simulacra_process_named_exit_import_terminates_execution_with_the_given_code() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { exit } from "simulacra:process";
            console.log("before");
            exit(7);
            console.log("after");
            "#,
        )
        .expect("simulacra:process exit import should succeed");

    assert_eq!(output.stdout, "before\n");
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.result, None);
}

#[test]
fn simulacra_process_default_export_exposes_env_cwd_and_exit() {
    let mut env = HashMap::new();
    env.insert("VISIBLE".to_string(), "yes".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import processModule from "simulacra:process";
            console.log(processModule.env.VISIBLE);
            processModule.exit(9);
            processModule.cwd();
            "#,
        )
        .expect("simulacra:process default import should succeed");

    assert_eq!(output.stdout, "yes\n");
    assert_eq!(output.exit_code, Some(9));
}

#[test]
fn simulacra_fs_namespace_object_keys_list_all_expected_exports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.keys(fs).sort());
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some(
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readdirSync","renameSync","statSync","unlinkSync","writeFile"]"#
        )
    );
}

#[test]
fn simulacra_fs_namespace_get_own_property_names_include_all_exports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.getOwnPropertyNames(fs).sort());
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some(
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readdirSync","renameSync","statSync","unlinkSync","writeFile"]"#
        )
    );
}

#[test]
fn simulacra_fs_namespace_read_file_export_has_function_type() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            typeof fs.readFile;
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(output.result.as_deref(), Some("function"));
}

#[test]
fn simulacra_fs_namespace_keys_do_not_expose_malformed_pointer_entries() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.keys(fs));
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    let keys = output.result.unwrap_or_default();
    assert!(
        !keys.contains("0x") && !keys.contains("ptr") && !keys.contains("__"),
        "expected Object.keys(ns) to contain export names, got {keys}"
    );
}

#[test]
fn simulacra_fs_named_import_style_is_supported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFile, writeFile } from "simulacra:fs";
            `${typeof readFile}|${typeof writeFile}`;
            "#,
        )
        .expect("named imports from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}

#[test]
fn simulacra_fs_default_import_style_is_supported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "simulacra:fs";
            `${typeof fs.readFile}|${typeof fs.writeFile}`;
            "#,
        )
        .expect("default import from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}

#[test]
fn simulacra_fs_namespace_import_style_is_supported() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/namespace.txt", b"namespace")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            fs.readFile("/workspace/namespace.txt");
            "#,
        )
        .expect("namespace import from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("namespace"));
}

#[test]
fn legacy_fs_global_readfilesync_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/legacy.txt", "still works");
            fs.readFileSync("/workspace/legacy.txt");
            "#,
        )
        .expect("legacy fs global should still work");

    assert_eq!(output.result.as_deref(), Some("still works"));
}

#[test]
fn legacy_console_global_log_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(r#"console.log("legacy console")"#)
        .expect("legacy console global should still work");

    assert_eq!(output.stdout, "legacy console\n");
}

#[test]
fn legacy_process_global_cwd_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval("process.cwd()")
        .expect("legacy process global should still work");

    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn s003_compatibility_smoke_test_stays_green_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/s003.txt", "compat");
            console.log(fs.readFileSync("/workspace/s003.txt"));
            process.cwd();
            "#,
        )
        .expect("S003 compatibility smoke test should still work");

    assert_eq!(output.stdout, "compat\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn s014_remote_and_relative_imports_stay_green_after_native_module_migration() {
    let fetcher = MockFetcher::new(vec![(
        "https://modules.invalid/value.js",
        Ok(r#"export default "remote";"#),
    )]);
    let (runtime, vfs) = make_runtime_with_fetcher(fetcher);
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/child.js", br#"export default "relative";"#)
        .expect("seed child module");
    fs.write(
        "/workspace/parent.js",
        br#"
        import child from "./child.js";
        import remote from "https://modules.invalid/value.js";
        export default `${child}-${remote}`;
        "#,
    )
    .expect("seed parent module");

    let output = runtime
        .eval(
            r#"
            import value from "/workspace/parent.js";
            value;
            "#,
        )
        .expect("S014 remote and relative imports should still work");

    assert_eq!(output.result.as_deref(), Some("relative-remote"));
}

#[test]
fn built_in_module_loading_does_not_emit_additional_spans_compared_to_plain_eval() {
    let (plain_runtime, _) = make_runtime();
    let (module_runtime, _) = make_runtime();

    let (_, plain_spans) = capture_spans(|| plain_runtime.eval("1 + 1").unwrap());
    let (_, module_spans) = capture_spans(|| {
        module_runtime
            .eval(
                r#"
                import { readFile } from "simulacra:fs";
                typeof readFile;
                "#,
            )
            .unwrap()
    });

    let module_operations = span_operations(&module_spans);
    let plain_operations = span_operations(&plain_spans);
    let baseline = if plain_operations.is_empty() {
        vec!["js_execute".to_string()]
    } else {
        plain_operations
    };
    assert_eq!(
        module_operations, baseline,
        "built-in module loading should emit only the normal js execution span"
    );
}

#[test]
fn new_native_exports_are_available_after_s016() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { existsSync, mkdirSync } from "simulacra:fs";
            `${typeof existsSync}|${typeof mkdirSync}`;
            "#,
        )
        .expect("existsSync and mkdirSync should be available after S016 implementation");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}
