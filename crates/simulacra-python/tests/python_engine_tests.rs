//! Thin integration tests verifying that simulacra-python re-exports from simulacra-python-runtime
//! work correctly. The full runtime test suite lives in simulacra-python-runtime/tests/.

use simulacra_python::{ExternalDispatcher, PythonResourceLimits, PythonRuntime};

fn make_runtime() -> PythonRuntime {
    PythonRuntime::new(PythonResourceLimits::default())
}

/// Verify the re-export path works for basic execution.
#[test]
fn reexport_execute_simple() {
    let rt = make_runtime();
    let out = rt.execute_simple("print('hello from re-export')").unwrap();
    assert_eq!(out.stdout, "hello from re-export\n");
}

/// Verify ExternalDispatcher trait is accessible through simulacra-python.
struct MinimalDispatcher;

impl ExternalDispatcher for MinimalDispatcher {
    fn read_file(&self, _path: &str) -> Result<String, String> {
        Ok("content".into())
    }
    fn write_file(&self, _path: &str, _content: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    fn http_get(&self, _url: &str) -> Result<String, String> {
        Ok("ok".into())
    }
    fn http_post(&self, _url: &str, _body: &str) -> Result<String, String> {
        Ok("ok".into())
    }
    fn env_get(&self, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[test]
fn reexport_execute_with_dispatch() {
    let rt = make_runtime();
    let out = rt
        .execute(
            "result = read_file('/test')\nprint(result)",
            &MinimalDispatcher,
        )
        .unwrap();
    assert_eq!(out.stdout, "content\n");
}
