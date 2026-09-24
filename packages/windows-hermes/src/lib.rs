//! `@nativescript/windows-hermes`: NativeScript Windows runtime on Microsoft's prebuilt Hermes.
//!
//! Hermes ships no compilable shim (unlike QuickJS/V8/JSC): `hermes.dll` already exports the JSR
//! C API (`jsr_create_runtime`, `jsr_runtime_get_node_api_env`, …) and a full `napi_*` surface.
//! This crate holds the FFI to that API plus the engine bring-up (create runtime + `napi_env`,
//! evaluate a script, drain microtasks), shared by both the standalone host (`main.rs`) and the
//! `nativescript.dll` adapter (`abi.rs`, `host_dll` feature). Keeping env-creation in the lib is
//! what lets the DLL adapter reuse it instead of copy-pasting `main.rs`.

use std::ffi::{c_char, c_void};

use napi::sys::{napi_env, napi_value};

// The `nativescript.dll` C ABI (WinUI 3 host DLL), compiled only under the `host_dll` feature.
#[cfg(feature = "host_dll")]
pub mod abi;

/// Hermes's JSR C API (imported from hermes.lib; see build.rs). All return `napi_status` (i32).
pub mod ffi {
    use super::*;

    extern "C" {
        pub fn jsr_create_config(config: *mut *mut c_void) -> i32;
        pub fn jsr_config_set_explicit_microtasks(config: *mut c_void, value: bool) -> i32;
        pub fn jsr_create_runtime(config: *mut c_void, runtime: *mut *mut c_void) -> i32;
        pub fn jsr_runtime_get_node_api_env(runtime: *mut c_void, env: *mut napi_env) -> i32;
        pub fn jsr_open_napi_env_scope(env: napi_env, scope: *mut *mut c_void) -> i32;
        pub fn jsr_close_napi_env_scope(env: napi_env, scope: *mut c_void) -> i32;
        pub fn jsr_delete_runtime(runtime: *mut c_void) -> i32;
        pub fn jsr_run_script(
            env: napi_env,
            source: napi_value,
            source_url: *const c_char,
            result: *mut napi_value,
        ) -> i32;
        pub fn jsr_drain_microtasks(env: napi_env, max_count_hint: i32, result: *mut bool) -> i32;
    }
}

/// Bring up a Hermes runtime + its `napi_env`, with a napi_env scope open on the calling thread.
/// Returns `(runtime, env, scope)`; the caller owns their lifetime (`jsr_close_napi_env_scope`
/// then `jsr_delete_runtime` to tear down), or leaks them for a process-lifetime host. Returns
/// `None` on failure.
///
/// Explicit microtasks: without this Hermes schedules promise reactions through a host-provided
/// `setImmediate` (the React Native model), which a bare engine doesn't have: promises would
/// throw on settle. With it, reactions queue as microtasks that [`drain_microtasks`] runs.
///
/// # Safety
/// Calls into `hermes.dll`; the returned handles must be used only on the thread that created them.
pub unsafe fn create_runtime_env() -> Option<(*mut c_void, napi_env, *mut c_void)> {
    let mut config: *mut c_void = std::ptr::null_mut();
    let mut runtime: *mut c_void = std::ptr::null_mut();
    let mut env_raw: napi_env = std::ptr::null_mut();
    if ffi::jsr_create_config(&mut config) != 0
        || ffi::jsr_config_set_explicit_microtasks(config, true) != 0
        || ffi::jsr_create_runtime(config, &mut runtime) != 0
        || ffi::jsr_runtime_get_node_api_env(runtime, &mut env_raw) != 0
        || env_raw.is_null()
    {
        return None;
    }
    let mut scope: *mut c_void = std::ptr::null_mut();
    if ffi::jsr_open_napi_env_scope(env_raw, &mut scope) != 0 {
        return None;
    }
    Some((runtime, env_raw, scope))
}

/// Evaluate `code` as a script named `filename` for the module runner, returning its completion
/// value. On failure Hermes's exception is left pending.
pub fn eval_named(env: &napi::Env, code: &str, filename: &str) -> Result<napi_value, ()> {
    use napi::NapiRaw;
    let (Ok(source), Ok(url)) = (env.create_string(code), std::ffi::CString::new(filename)) else {
        return Err(());
    };
    let mut result: napi_value = std::ptr::null_mut();
    let status = unsafe { ffi::jsr_run_script(env.raw(), source.raw(), url.as_ptr(), &mut result) };
    if status != napi::sys::Status::napi_ok || result.is_null() {
        return Err(());
    }
    Ok(result)
}

/// Evaluate `code` and return the completion value coerced to a string, or the thrown exception's
/// message. Shared by the standalone host and the `nativescript.dll` adapter.
pub fn run_script(env: &napi::Env, code: &str) -> Result<String, String> {
    use napi::{JsUnknown, NapiRaw, NapiValue};
    let source = env.create_string(code).map_err(|e| e.to_string())?;
    let url = std::ffi::CString::new("<whermes-host>").unwrap();
    let mut result: napi_value = std::ptr::null_mut();
    let status = unsafe { ffi::jsr_run_script(env.raw(), source.raw(), url.as_ptr(), &mut result) };
    if status != napi::sys::Status::napi_ok || result.is_null() {
        let mut is_pending = false;
        unsafe { napi::sys::napi_is_exception_pending(env.raw(), &mut is_pending) };
        if is_pending {
            let mut err: napi_value = std::ptr::null_mut();
            unsafe { napi::sys::napi_get_and_clear_last_exception(env.raw(), &mut err) };
            let msg = unsafe { JsUnknown::from_raw_unchecked(env.raw(), err) }
                .coerce_to_string()
                .and_then(|s| s.into_utf8())
                .and_then(|s| Ok(s.as_str()?.to_owned()))
                .unwrap_or_else(|_| "<unprintable exception>".into());
            return Err(format!("JS exception: {msg}"));
        }
        return Err(format!("jsr_run_script status {status}"));
    }
    let val = unsafe { JsUnknown::from_raw_unchecked(env.raw(), result) };
    val.coerce_to_string()
        .and_then(|s| s.into_utf8())
        .and_then(|s| Ok(s.as_str()?.to_owned()))
        .map_err(|e| e.to_string())
}

/// Run Hermes's microtask queue to exhaustion (promise reactions): the event loop's drain hook.
pub fn drain_microtasks(env_raw: napi_env) {
    unsafe {
        let mut more = false;
        ffi::jsr_drain_microtasks(env_raw, -1, &mut more);
    }
}

/// The three engine-specific pieces the `nativescript.dll` adapter needs, in the same shape as the
/// QuickJS package's `shim` (`shared_env_ptr` / `run_script_checked` / `drain_microtasks`) so
/// `abi.rs` reads the same across engines. Gated behind `host_dll`.
#[cfg(feature = "host_dll")]
pub mod host {
    use std::cell::Cell;
    use std::ffi::c_void;

    use napi::Env;

    /// The process-lifetime Hermes `napi_env` as a raw pointer. Brings up the runtime + env + scope
    /// on first call and leaks them (as a real host keeps them for the process), caching the env
    /// thread-local. Returns null on failure.
    pub fn shared_env_ptr() -> *mut c_void {
        thread_local! {
            static ENV: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
        }
        ENV.with(|e| {
            let cur = e.get();
            if !cur.is_null() {
                return cur;
            }
            // runtime + scope are intentionally leaked (process-lifetime); only env is kept.
            let (_runtime, env_raw, _scope) = match unsafe { super::create_runtime_env() } {
                Some(t) => t,
                None => return std::ptr::null_mut(),
            };
            e.set(env_raw as *mut c_void);
            env_raw as *mut c_void
        })
    }

    /// Evaluate `code`, returning its string coercion or the thrown exception's message.
    pub fn run_script_checked(raw: *mut c_void, code: &str) -> Result<String, String> {
        let env = unsafe { Env::from_raw(raw as napi::sys::napi_env) };
        super::run_script(&env, code)
    }

    /// Drain Hermes's microtask queue: the event loop's drain hook.
    pub fn drain_microtasks(raw: *mut c_void) {
        super::drain_microtasks(raw as napi::sys::napi_env);
    }
}
