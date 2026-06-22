use simulacra_quickjs::JsRuntime;
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use std::sync::Arc;

fn make_runtime() -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let runtime = JsRuntime::new(vfs_dyn).expect("failed to create runtime");
    (runtime, vfs)
}

fn assert_console_stdout(code: &str, expected_stdout: &str) {
    let (runtime, _) = make_runtime();
    let output = runtime
        .eval(code)
        .expect("console.log evaluation should succeed");

    assert_eq!(
        output.stdout, expected_stdout,
        "unexpected console.log stdout for code:\n{code}"
    );
}

#[test]
fn console_log_formats_arrays_with_spaces() {
    assert_console_stdout(r#"console.log([1, 2, 3]);"#, "[ 1, 2, 3 ]\n");
}

#[test]
fn console_log_formats_objects_with_key_value_pairs() {
    assert_console_stdout(r#"console.log({a: 1, b: 2});"#, "{ a: 1, b: 2 }\n");
}

#[test]
fn console_log_formats_top_level_strings_without_quotes() {
    assert_console_stdout(r#"console.log("hello");"#, "hello\n");
}

#[test]
fn console_log_formats_nested_strings_in_arrays_with_single_quotes() {
    assert_console_stdout(r#"console.log(["a", "b"]);"#, "[ 'a', 'b' ]\n");
}

#[test]
fn console_log_formats_null_as_literal_null() {
    assert_console_stdout(r#"console.log(null);"#, "null\n");
}

#[test]
fn console_log_formats_undefined_as_literal_undefined() {
    assert_console_stdout(r#"console.log(undefined);"#, "undefined\n");
}

#[test]
fn console_log_formats_booleans_as_literals() {
    assert_console_stdout(r#"console.log(true);"#, "true\n");
}

#[test]
fn console_log_formats_integers_as_literals() {
    assert_console_stdout(r#"console.log(42);"#, "42\n");
}

#[test]
fn console_log_formats_floats_as_literals() {
    assert_console_stdout(r#"console.log(3.14);"#, "3.14\n");
}

#[test]
fn console_log_formats_named_functions_with_their_name() {
    assert_console_stdout(r#"console.log(function foo() {});"#, "[Function: foo]\n");
}

#[test]
fn console_log_formats_arrow_functions_as_anonymous() {
    assert_console_stdout(r#"console.log(() => {});"#, "[Function (anonymous)]\n");
}

#[test]
fn console_log_formats_circular_references_with_circular_placeholder() {
    let (runtime, _) = make_runtime();
    let output = runtime
        .eval(
            r#"
            const value = {};
            value.self = value;
            console.log(value);
            "#,
        )
        .expect("console.log with a circular reference should succeed");

    assert!(
        output.stdout.contains("[Circular]"),
        "expected circular placeholder in stdout, got {:?}",
        output.stdout
    );
}

#[test]
fn console_log_limits_deeply_nested_objects_to_object_placeholder() {
    let (runtime, _) = make_runtime();
    let output = runtime
        .eval(r#"console.log({ a: { b: { c: { d: { e: 1 } } } } });"#)
        .expect("console.log with a deep object should succeed");

    assert!(
        output.stdout.contains("[Object]"),
        "expected deep object placeholder in stdout, got {:?}",
        output.stdout
    );
}

#[test]
fn console_log_limits_deeply_nested_arrays_to_array_placeholder() {
    let (runtime, _) = make_runtime();
    let output = runtime
        .eval(r#"console.log([[[[[1]]]]]);"#)
        .expect("console.log with a deep array should succeed");

    assert!(
        output.stdout.contains("[Array]"),
        "expected deep array placeholder in stdout, got {:?}",
        output.stdout
    );
}

#[test]
fn console_log_separates_multiple_arguments_with_spaces() {
    assert_console_stdout(r#"console.log("a", "b", 1);"#, "a b 1\n");
}

#[test]
fn console_log_formats_object_keys_arrays_as_single_quoted_strings() {
    assert_console_stdout(
        r#"console.log(Object.keys({x: 1, y: 2}));"#,
        "[ 'x', 'y' ]\n",
    );
}

#[test]
fn console_log_formats_symbols_with_description() {
    assert_console_stdout(r#"console.log(Symbol("desc"));"#, "Symbol(desc)\n");
}

#[test]
fn console_log_truncates_large_arrays_after_100_items() {
    let (runtime, _) = make_runtime();
    let output = runtime
        .eval(
            r#"
            const arr = [];
            for (let i = 0; i < 105; i++) arr.push(i);
            console.log(arr);
            "#,
        )
        .expect("console.log with large array should succeed");

    assert!(
        output.stdout.contains("... 5 more items"),
        "expected truncation message in stdout, got {:?}",
        output.stdout
    );
}
