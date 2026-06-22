//! WHATWG-compliant `AbortController` and `AbortSignal` implementation.
//!
//! `AbortSignal.timeout(ms)` does **not** auto-abort after the given duration.
//! Instead it stores the timeout as metadata that `fetch()` extracts and passes
//! to the HTTP client. The signal's `aborted` stays `false`.

use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::Ctx;

/// Default reason string used when `abort()` is called without an explicit reason.
const DEFAULT_ABORT_REASON: &str = "The operation was aborted";

/// A WHATWG-compliant `AbortSignal`.
///
/// Tracks whether an operation has been aborted, an optional reason string,
/// and optional timeout metadata (milliseconds) for `fetch()` integration.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    aborted: bool,
    reason: Option<String>,
    timeout_ms: Option<u64>,
}

impl AbortSignal {
    /// Create a new non-aborted signal with no timeout.
    pub fn new() -> Self {
        Self {
            aborted: false,
            reason: None,
            timeout_ms: None,
        }
    }

    /// Whether the signal has been aborted.
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// The abort reason, if any.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Timeout metadata in milliseconds, if set via `AbortSignal.timeout()`.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    /// Mark this signal as aborted with the given reason.
    ///
    /// If `reason` is `None`, the default message
    /// `"The operation was aborted"` is used.
    pub fn abort(&mut self, reason: Option<String>) {
        self.aborted = true;
        self.reason = Some(reason.unwrap_or_else(|| DEFAULT_ABORT_REASON.to_string()));
    }

    /// Return `Err(reason)` if aborted, otherwise `Ok(())`.
    pub fn throw_if_aborted(&self) -> Result<(), String> {
        if self.aborted {
            Err(self
                .reason
                .clone()
                .unwrap_or_else(|| DEFAULT_ABORT_REASON.to_string()))
        } else {
            Ok(())
        }
    }

    /// Create a pre-aborted signal.
    ///
    /// Equivalent to the static `AbortSignal.abort(reason)` in the WHATWG spec.
    pub fn new_aborted(reason: Option<String>) -> Self {
        let mut signal = Self::new();
        signal.abort(reason);
        signal
    }

    /// Create a non-aborted signal carrying timeout metadata.
    ///
    /// **Important:** the signal is *not* automatically aborted after `ms`.
    /// The timeout value is stored as metadata for `fetch()` to extract.
    pub fn new_timeout(ms: u64) -> Self {
        Self {
            aborted: false,
            reason: None,
            timeout_ms: Some(ms),
        }
    }
}

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// A WHATWG-compliant `AbortController`.
///
/// Owns a shared [`AbortSignal`] that can be passed to consumers (e.g.
/// `fetch()`) and later aborted by calling [`AbortController::abort`].
#[derive(Debug, Clone)]
pub struct AbortController {
    signal: Rc<RefCell<AbortSignal>>,
}

impl AbortController {
    /// Create a new controller with a fresh, non-aborted signal.
    pub fn new() -> Self {
        Self {
            signal: Rc::new(RefCell::new(AbortSignal::new())),
        }
    }

    /// Return a shared reference to the underlying signal.
    pub fn signal(&self) -> Rc<RefCell<AbortSignal>> {
        Rc::clone(&self.signal)
    }

    /// Abort the associated signal.
    ///
    /// If `reason` is `None`, the default message
    /// `"The operation was aborted"` is used.
    pub fn abort(&self, reason: Option<String>) {
        self.signal.borrow_mut().abort(reason);
    }
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

/// Register `AbortController` and `AbortSignal` as JS globals.
///
/// After calling this, JS code can use:
/// - `new AbortController()` with `.signal` and `.abort(reason?)`
/// - `signal.aborted`, `signal.reason`, `signal.throwIfAborted()`
/// - `AbortSignal.abort(reason?)` — static, returns pre-aborted signal
/// - `AbortSignal.timeout(ms)` — static, returns signal with timeout metadata
pub fn register_abort(ctx: &Ctx<'_>) -> Result<(), rquickjs::Error> {
    ctx.eval::<(), _>(
        r#"
(function() {
    var ABORTED = Symbol("__abort_aborted__");
    var REASON = Symbol("__abort_reason__");
    var TIMEOUT_MS = Symbol("__abort_timeout_ms__");

    function AbortSignal() {
        if (!(this instanceof AbortSignal)) {
            throw new TypeError("AbortSignal constructor requires 'new'");
        }
        this[ABORTED] = false;
        this[REASON] = undefined;
        this[TIMEOUT_MS] = undefined;
    }

    Object.defineProperty(AbortSignal.prototype, "aborted", {
        get: function() {
            return this[ABORTED];
        },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(AbortSignal.prototype, "reason", {
        get: function() {
            return this[REASON];
        },
        enumerable: true,
        configurable: true
    });

    // Expose timeout_ms as a regular property name so fetch() and Request
    // can read it from outside this IIFE (Symbols are scoped to the closure).
    Object.defineProperty(AbortSignal.prototype, "timeout_ms", {
        get: function() {
            return this[TIMEOUT_MS];
        },
        enumerable: false,
        configurable: true
    });

    AbortSignal.prototype.throwIfAborted = function() {
        if (this[ABORTED]) {
            throw new DOMException(
                this[REASON] || "The operation was aborted",
                "AbortError"
            );
        }
    };

    // Internal helper used by AbortController to mutate the signal
    AbortSignal.prototype.__abort__ = function(reason) {
        this[ABORTED] = true;
        this[REASON] = (reason !== undefined && reason !== null) ? reason : "The operation was aborted";
    };

    // Static method: AbortSignal.abort(reason?)
    AbortSignal.abort = function(reason) {
        var signal = new AbortSignal();
        signal.__abort__(reason);
        return signal;
    };

    // Static method: AbortSignal.timeout(ms)
    // Does NOT auto-abort — stores timeout as metadata for fetch().
    AbortSignal.timeout = function(ms) {
        var signal = new AbortSignal();
        signal[TIMEOUT_MS] = Number(ms);
        return signal;
    };

    var SIGNAL = Symbol("__abort_controller_signal__");

    function AbortController() {
        if (!(this instanceof AbortController)) {
            throw new TypeError("AbortController constructor requires 'new'");
        }
        this[SIGNAL] = new AbortSignal();
    }

    Object.defineProperty(AbortController.prototype, "signal", {
        get: function() {
            return this[SIGNAL];
        },
        enumerable: true,
        configurable: true
    });

    AbortController.prototype.abort = function(reason) {
        this[SIGNAL].__abort__(reason);
    };

    globalThis.AbortController = AbortController;
    globalThis.AbortSignal = AbortSignal;
})();
"#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::Value;

    // -----------------------------------------------------------------------
    // Rust-level unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn new_controller_has_non_aborted_signal() {
        let controller = AbortController::new();
        let signal = controller.signal();
        assert!(!signal.borrow().aborted());
        assert_eq!(signal.borrow().reason(), None);
    }

    #[test]
    fn abort_sets_aborted_and_default_reason() {
        let controller = AbortController::new();
        controller.abort(None);
        let signal = controller.signal();
        assert!(signal.borrow().aborted());
        assert_eq!(signal.borrow().reason(), Some("The operation was aborted"));
    }

    #[test]
    fn abort_with_custom_reason() {
        let controller = AbortController::new();
        controller.abort(Some("user cancelled".to_string()));
        let signal = controller.signal();
        assert!(signal.borrow().aborted());
        assert_eq!(signal.borrow().reason(), Some("user cancelled"));
    }

    #[test]
    fn throw_if_aborted_throws_when_aborted() {
        let mut signal = AbortSignal::new();
        signal.abort(Some("test error".to_string()));
        let result = signal.throw_if_aborted();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "test error");
    }

    #[test]
    fn throw_if_aborted_noop_when_not_aborted() {
        let signal = AbortSignal::new();
        assert!(signal.throw_if_aborted().is_ok());
    }

    #[test]
    fn signal_abort_static_returns_pre_aborted() {
        let signal = AbortSignal::new_aborted(None);
        assert!(signal.aborted());
        assert_eq!(signal.reason(), Some("The operation was aborted"));

        let signal2 = AbortSignal::new_aborted(Some("custom".to_string()));
        assert!(signal2.aborted());
        assert_eq!(signal2.reason(), Some("custom"));
    }

    #[test]
    fn signal_timeout_carries_ms_metadata() {
        let signal = AbortSignal::new_timeout(5000);
        assert!(!signal.aborted());
        assert_eq!(signal.timeout_ms(), Some(5000));
        assert_eq!(signal.reason(), None);
    }

    // -----------------------------------------------------------------------
    // JS integration tests
    // -----------------------------------------------------------------------

    fn with_js_context<F: FnOnce(&Ctx<'_>)>(f: F) {
        let rt = rquickjs::Runtime::new().expect("failed to create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("failed to create context");
        ctx.with(|ctx| {
            register_abort(&ctx).expect("failed to register AbortController/AbortSignal");
            f(&ctx);
        });
    }

    #[test]
    fn js_new_abort_controller_has_non_aborted_signal() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var ac = new AbortController();
                        return ac.signal.aborted === false;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_controller_abort_sets_signal_aborted() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var ac = new AbortController();
                        ac.abort();
                        return ac.signal.aborted === true;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_controller_abort_with_custom_reason() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var ac = new AbortController();
                        ac.abort("custom reason");
                        return [ac.signal.aborted, ac.signal.reason];
                    })()
                    "#,
                )
                .expect("eval failed");

            let aborted: bool = result[0].as_bool().unwrap();
            assert!(aborted);
            let reason: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(reason, "custom reason");
        });
    }

    #[test]
    fn js_signal_throw_if_aborted_throws_when_aborted() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var ac = new AbortController();
                        ac.abort();
                        try {
                            ac.signal.throwIfAborted();
                            return false;
                        } catch(e) {
                            return true;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_signal_abort_static_returns_pre_aborted() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var signal = AbortSignal.abort();
                        return [signal.aborted, signal.reason];
                    })()
                    "#,
                )
                .expect("eval failed");

            let aborted: bool = result[0].as_bool().unwrap();
            assert!(aborted);
            let reason: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(reason, "The operation was aborted");
        });
    }

    #[test]
    fn js_signal_timeout_returns_non_aborted_signal() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var signal = AbortSignal.timeout(5000);
                        return signal.aborted === false;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }
}
