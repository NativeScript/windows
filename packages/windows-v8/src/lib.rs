//! `@nativescript/windows-v8`: NativeScript Windows runtime on V8.
//!
//! Reuses napi-android's `v8-api.cpp` (napi over V8's C++ API) compiled against the `v8` crate's
//! bundled rusty_v8 (V8 14.7). Our v8 crate uses the default config (no pointer compression / no
//! sandbox), so the shim needs no ABI-matching defines. The Android bring-up (`jsr.cpp`) is
//! replaced by `csrc/win_jsr.cpp`.

// The C++ shim (v8-api.cpp, win_jsr.cpp) calls into V8. Keeps the v8 crate, and with it rusty_v8's
// static lib, in the link even where no Rust code here names it (the runtime crate is built without
// its V8-based classic engine).
extern crate v8;

pub mod ffi {
    use napi::sys::{napi_env, napi_value};
    use std::os::raw::c_char;

    /// Opaque `napi_runtime__*` from `win_jsr.cpp`.
    pub type NapiRuntime = *mut std::ffi::c_void;

    extern "C" {
        pub fn js_create_runtime(runtime: *mut NapiRuntime) -> i32;
        pub fn js_create_napi_env(env: *mut napi_env, runtime: NapiRuntime) -> i32;
        pub fn js_execute_script(
            env: napi_env,
            script: napi_value,
            file: *const c_char,
            result: *mut napi_value,
        ) -> i32;
        /// Microtask checkpoint (PerformMicrotaskCheckpoint): the event loop's drain hook.
        pub fn js_execute_pending_jobs(env: napi_env) -> i32;
        /// Run an entry ES module; errors are reported through `esm::ns_esm_report_error`.
        pub fn js_run_module(env: napi_env, source: *const c_char, key: *const c_char) -> i32;
        pub fn js_esm_evict(key: *const c_char) -> i32;
        pub fn js_esm_for_each_key(
            visit: extern "C" fn(key: *const c_char, user: *mut std::ffi::c_void),
            user: *mut std::ffi::c_void,
        );
    }
}

/// The module loader's Rust callbacks (see `csrc/win_jsr.cpp`).
pub mod esm;

/// V8's fatal/OOM error handler (registered by `win_jsr.cpp`): logs to the trace log before the
/// process goes down, since a packaged app has no visible stdout/stderr for V8's own CHECK-failure
/// aborts (which bypass Rust's panic machinery entirely).
#[no_mangle]
pub extern "C" fn ns_v8_fatal_error(
    location: *const std::os::raw::c_char,
    message: *const std::os::raw::c_char,
) {
    let to_str = |p: *const std::os::raw::c_char| -> String {
        if p.is_null() {
            return "<none>".to_string();
        }
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned()
    };
    runtime::debug_output(&format!(
        "[ERROR] [V8_FATAL] {} : {}\n",
        to_str(location),
        to_str(message)
    ));
}

// The `nativescript.dll` C ABI (WinUI 3 host DLL), compiled only under the `host_dll` feature.
#[cfg(feature = "host_dll")]
pub mod abi;

/// The three engine-specific pieces the `nativescript.dll` adapter needs: create the V8 engine +
/// its `napi_env`, evaluate a script string, and drain the microtask queue. Mirrors the QuickJS
/// package's `shim` surface (`shared_env_ptr` / `run_script_checked` / `drain_microtasks`) so
/// `abi.rs` reads the same across engines. Gated behind `host_dll`: the standalone bin (`main.rs`)
/// keeps its own copies so the validated host path is untouched.
#[cfg(feature = "host_dll")]
pub mod host {
    use std::cell::Cell;
    use std::ffi::{c_void, CString};

    use napi::{Env, JsUnknown, NapiRaw, NapiValue};

    use crate::ffi;

    /// The process-lifetime V8 `napi_env` as a raw pointer. Initializes the V8 platform and creates
    /// the isolate + env on first call (both process-lifetime, as a real host keeps them), then
    /// caches the env thread-local. Returns null on failure.
    pub fn shared_env_ptr() -> *mut c_void {
        thread_local! {
            static ENV: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
        }
        ENV.with(|e| {
            let cur = e.get();
            if !cur.is_null() {
                return cur;
            }
            unsafe {
                // The shim's C++ can't link NewDefaultPlatform's libc++ unique_ptr, so drive the
                // V8 platform init from rusty_v8: before js_create_runtime creates an isolate.
                let platform = v8::new_default_platform(0, false).make_shared();
                v8::V8::initialize_platform(platform);
                v8::V8::initialize();

                let mut runtime: ffi::NapiRuntime = std::ptr::null_mut();
                let mut env_raw: napi::sys::napi_env = std::ptr::null_mut();
                if ffi::js_create_runtime(&mut runtime) != 0
                    || ffi::js_create_napi_env(&mut env_raw, runtime) != 0
                    || env_raw.is_null()
                {
                    return std::ptr::null_mut();
                }
                e.set(env_raw as *mut c_void);
                env_raw as *mut c_void
            }
        })
    }

    /// Evaluate `code`, returning its string coercion or the thrown exception's message.
    pub fn run_script_checked(raw: *mut c_void, code: &str) -> Result<String, String> {
        unsafe {
            let env = Env::from_raw(raw as napi::sys::napi_env);
            let source = env.create_string(code).map_err(|e| e.to_string())?;
            let file = CString::new("<script>").unwrap();
            let mut result: napi::sys::napi_value = std::ptr::null_mut();
            let status =
                ffi::js_execute_script(env.raw(), source.raw(), file.as_ptr(), &mut result);
            if status != 0 || result.is_null() {
                let mut pending = false;
                napi::sys::napi_is_exception_pending(env.raw(), &mut pending);
                if pending {
                    let mut err: napi::sys::napi_value = std::ptr::null_mut();
                    napi::sys::napi_get_and_clear_last_exception(env.raw(), &mut err);
                    // Prefer `.stack` over a bare toString() when available. Rebuild the JsUnknown
                    // per attempt since each coercion consumes its receiver.
                    let stack = JsUnknown::from_raw_unchecked(env.raw(), err)
                        .coerce_to_object()
                        .and_then(|o| o.get_named_property::<JsUnknown>("stack"))
                        .and_then(|s| s.coerce_to_string())
                        .and_then(|s| s.into_utf8())
                        .and_then(|s| Ok(s.as_str()?.to_owned()))
                        .ok()
                        .filter(|s| !s.is_empty());
                    let msg = stack.unwrap_or_else(|| {
                        JsUnknown::from_raw_unchecked(env.raw(), err)
                            .coerce_to_string()
                            .and_then(|s| s.into_utf8())
                            .and_then(|s| Ok(s.as_str()?.to_owned()))
                            .unwrap_or_else(|_| "<unprintable exception>".into())
                    });
                    return Err(format!("JS exception: {msg}"));
                }
                return Err(format!("js_execute_script status {status}"));
            }
            let val = JsUnknown::from_raw_unchecked(env.raw(), result);
            val.coerce_to_string()
                .and_then(|s| s.into_utf8())
                .and_then(|s| Ok(s.as_str()?.to_owned()))
                .map_err(|e| e.to_string())
        }
    }

    /// Run `code` as the entry ES module registered under `key`. Errors are already reported
    /// (trace log + last JS error) by the loader; `false` only says the graph didn't start.
    pub fn run_module(raw: *mut c_void, code: &str, key: &str) -> bool {
        let (Ok(code), Ok(key)) = (CString::new(code), CString::new(key)) else {
            return false;
        };
        unsafe { ffi::js_run_module(raw as napi::sys::napi_env, code.as_ptr(), key.as_ptr()) == 0 }
    }

    /// Run V8's microtask checkpoint (promise reactions): the event loop's drain hook.
    pub fn drain_microtasks(raw: *mut c_void) {
        unsafe {
            ffi::js_execute_pending_jobs(raw as napi::sys::napi_env);
        }
    }
}
