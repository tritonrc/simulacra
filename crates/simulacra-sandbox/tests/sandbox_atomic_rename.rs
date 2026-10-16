//! JS `renameSync` edge cases: renaming onto itself, and renaming a missing
//! source over an existing destination.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_vfs::OverlayFs;

fn overlay() -> Arc<OverlayFs> {
    Arc::new(OverlayFs::new(
        Box::new(MemoryFs::new()),
        Box::new(MemoryFs::new()),
    ))
}

fn js_cell_over(fs: Arc<dyn VirtualFs>) -> AgentCell {
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(
        fs,
        capability(&["/**"], &["/**"], false, true),
        unlimited_budget(),
        journal,
        http,
    )
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

fn error_of(cell: &AgentCell, code: &str) -> String {
    match cell.execute_js(code) {
        Ok(out) => panic!("{code} should have thrown, returned {:?}", out.result),
        Err(err) => err.to_string(),
    }
}

#[test]
fn js_rename_of_a_file_onto_itself_is_a_no_op_on_memory_fs() {
    let fs = Arc::new(MemoryFs::new());
    let cell = js_cell_over(fs.clone());
    fs.write("/a", b"keep").unwrap();
    cell.execute_js("fs.renameSync('/a', '/a')").unwrap();
    assert_eq!(text(&*fs, "/a"), "keep");
}

#[test]
fn js_rename_of_a_file_onto_itself_is_a_no_op_on_the_copy_fallback() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    fs.write("/a", b"keep").unwrap();
    cell.execute_js("fs.renameSync('/a', '/a')").unwrap();
    assert!(
        fs.exists("/a"),
        "the copy fallback must not delete the file"
    );
    assert_eq!(text(&*fs, "/a"), "keep");
}

#[test]
fn js_rename_of_a_missing_source_over_an_existing_file_throws_and_keeps_it() {
    let fs = Arc::new(MemoryFs::new());
    let cell = js_cell_over(fs.clone());
    fs.write("/existing", b"keep").unwrap();
    assert!(
        cell.execute_js("fs.renameSync('/missing', '/existing')")
            .is_err()
    );
    assert_eq!(text(&*fs, "/existing"), "keep");
}

#[test]
fn js_rename_of_a_missing_source_over_a_directory_reports_not_found() {
    let fs = Arc::new(MemoryFs::new());
    let cell = js_cell_over(fs.clone());
    fs.mkdir("/some-dir").unwrap();
    let message = error_of(&cell, "fs.renameSync('/missing', '/some-dir')");
    let lower = message.to_lowercase();
    assert!(
        lower.contains("not found") || message.contains("/missing"),
        "the error should name the missing source, got: {message}"
    );
    assert!(
        !lower.contains("directory"),
        "the error must not blame the destination directory, got: {message}"
    );
    assert!(fs.metadata("/some-dir").unwrap().is_dir);
}

#[test]
fn js_rename_of_a_missing_source_over_a_directory_reports_not_found_on_the_fallback() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    fs.mkdir("/some-dir").unwrap();
    let message = error_of(&cell, "fs.renameSync('/missing', '/some-dir')");
    assert!(
        !message.to_lowercase().contains("directory"),
        "the error must not blame the destination directory, got: {message}"
    );
}

#[test]
fn js_rename_of_a_missing_source_over_an_existing_file_keeps_it_on_the_fallback() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    fs.write("/existing", b"keep").unwrap();
    assert!(
        cell.execute_js("fs.renameSync('/missing', '/existing')")
            .is_err()
    );
    assert_eq!(text(&*fs, "/existing"), "keep");
}
