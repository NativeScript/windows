//! `@nativescript/windows-jsc`: NativeScript Windows runtime on JavaScriptCore.
//!
//! The napi provider is napi-android's JSC shim (`vendor/shim/jsc-api.cpp` + `jsr.cpp`), which is
//! implemented purely over JavaScriptCore's **public C API** (`<JavaScriptCore/JavaScript.h>`), so
//! it compiles against just the vendored public headers: no WebKit internals or WTF. `build.rs`
//! compiles it (this verifies the MSVC port with no engine binary present). The runnable bin needs
//! a real `JavaScriptCore.dll`/`.lib` in `vendor/x64` (feature `jsc_link`); see the package README.

/// FFI to the shim's JSR bring-up (available once the engine is linked via `jsc_link`).
#[cfg(feature = "jsc_link")]
pub mod ffi {
    use napi::sys::{napi_env, napi_value};
    use std::os::raw::c_char;

    /// Opaque `napi_runtime__*` from the shim.
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
        /// Drain hook for the event loop. A no-op in the JSC shim: JSC drains its microtask
        /// queue itself whenever the VM returns to the host.
        pub fn js_execute_pending_jobs(env: napi_env) -> i32;
    }
}

/// Evaluate `code` as a script named `filename` for the module runner, returning its completion
/// value. On failure JSC's exception is left pending.
#[cfg(feature = "jsc_link")]
pub fn eval_named(env: &napi::Env, code: &str, filename: &str) -> Result<napi::sys::napi_value, ()> {
    use napi::NapiRaw;
    let (Ok(source), Ok(file)) = (env.create_string(code), std::ffi::CString::new(filename)) else {
        return Err(());
    };
    let mut result: napi::sys::napi_value = std::ptr::null_mut();
    let status = unsafe { ffi::js_execute_script(env.raw(), source.raw(), file.as_ptr(), &mut result) };
    if status != 0 || result.is_null() {
        return Err(());
    }
    Ok(result)
}

// The `nativescript.dll` C ABI (WinUI 3 host DLL), compiled only under the `host_dll` feature
// (which enables `jsc_link`, so a real JavaScriptCore.{lib,dll} must be present in vendor/x64).
#[cfg(feature = "host_dll")]
pub mod abi;

/// The three engine-specific pieces the `nativescript.dll` adapter needs: create the JSC engine +
/// its `napi_env`, evaluate a script string, and drain microtasks. Mirrors the QuickJS package's
/// `shim` surface (`shared_env_ptr` / `run_script_checked` / `drain_microtasks`) so `abi.rs` reads
/// the same across engines. Gated behind `host_dll`: the standalone bin (`main.rs`) keeps its own
/// copies so the validated host path is untouched.
#[cfg(feature = "host_dll")]
pub mod host {
    use std::cell::Cell;
    use std::ffi::{c_void, CString};

    use napi::{Env, JsUnknown, NapiRaw, NapiValue};

    use crate::ffi;

    /// The process-lifetime JSC `napi_env` as a raw pointer. Creates the VM + env on first call
    /// (both process-lifetime, as a real host keeps them), then caches the env thread-local.
    /// Returns null on failure.
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
                    // Prefer `.stack` over a bare toString() when available.
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

    /// A no-op in the JSC shim (JSC drains its microtask queue when the VM returns to the host);
    /// kept so all four engines share the same drain contract.
    pub fn drain_microtasks(raw: *mut c_void) {
        unsafe {
            ffi::js_execute_pending_jobs(raw as napi::sys::napi_env);
        }
    }
}
