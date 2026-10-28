//! Logging a huge string or a huge object under a small cap must not copy it
//! into Rust first. The script marks a point with `tools.mark()`; the largest
//! single Rust allocation after that mark is what the console cost.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use simulacra_quickjs::{BoxFuture, JsHost, JsRuntime, JsRuntimeOptions};
use simulacra_vfs::MemoryFs;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

struct Mark;

impl JsHost for Mark {
    fn names(&self) -> Vec<String> {
        vec!["mark".into()]
    }
    fn call(&self, _: String, _: String) -> BoxFuture<'static, Result<String, String>> {
        LARGEST.store(0, Ordering::Relaxed);
        Box::pin(async { Ok("null".into()) })
    }
}

fn log_after_mark(setup: &str, log: &str) -> (String, usize) {
    let rt = JsRuntime::new(Arc::new(MemoryFs::new()))
        .unwrap()
        .with_runtime_options(JsRuntimeOptions {
            host: Some(Arc::new(Mark)),
            console_cap: Some(256),
            ..Default::default()
        });
    let out = rt
        .eval(&format!(
            "(async () => {{ {setup}; await tools.mark(); {log}; }})()"
        ))
        .unwrap();
    (out.stdout, LARGEST.load(Ordering::Relaxed))
}

#[test]
fn huge_values_are_bounded_before_they_reach_rust() {
    const LIMIT: usize = 4 << 20;

    let (stdout, largest) = log_after_mark("const s = 'x'.repeat(1 << 24)", "console.log(s)");
    assert!(stdout.len() <= 256 + 32 && stdout.ends_with("[output truncated]\n"));
    assert!(largest < LIMIT, "string copy of {largest} bytes");

    let (stdout, largest) = log_after_mark(
        "const o = {}; for (let i = 0; i < 200000; i++) o['k' + i] = i",
        "console.log(o)",
    );
    assert!(stdout.len() <= 256 + 32 && stdout.ends_with("[output truncated]\n"));
    assert!(largest < LIMIT, "key copy of {largest} bytes");

    let (stdout, largest) = log_after_mark(
        "const s = 'x'.repeat(1 << 24)",
        "String.prototype.slice = () => 'y'.repeat(1 << 24); \
         Function.prototype.call = () => 'z'.repeat(1 << 24); console.log(s)",
    );
    assert!(stdout.ends_with("[output truncated]\n"));
    assert!(largest < LIMIT, "patched slice copied {largest} bytes");

    let (stdout, largest) =
        log_after_mark("const o = {['x'.repeat(1 << 24)]: 1}", "console.log(o)");
    assert!(stdout.len() <= 256 + 32 && stdout.ends_with("[output truncated]\n"));
    assert!(largest < LIMIT, "key copy of {largest} bytes");
}
