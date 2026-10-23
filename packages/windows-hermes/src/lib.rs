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
        pub fn jsr_config_set_script_cache(
            config: *mut c_void,
            script_cache_data: *mut c_void,
            load: Option<ScriptCacheLoad>,
            store: Option<ScriptCacheStore>,
            data_delete: Option<DataDelete>,
            deleter_data: *mut c_void,
        ) -> i32;
    }

    pub type DataDelete = unsafe extern "C" fn(data: *mut c_void, deleter_data: *mut c_void);

    pub type ScriptCacheLoad = unsafe extern "C" fn(
        script_cache_data: *mut c_void,
        source_url: *const c_char,
        source_hash: u64,
        runtime_name: *const c_char,
        runtime_version: u64,
        cache_tag: *const c_char,
        buffer: *mut *const u8,
        buffer_size: *mut usize,
        buffer_delete_cb: *mut Option<DataDelete>,
        deleter_data: *mut *mut c_void,
    );

    pub type ScriptCacheStore = unsafe extern "C" fn(
        script_cache_data: *mut c_void,
        source_url: *const c_char,
        source_hash: u64,
        runtime_name: *const c_char,
        runtime_version: u64,
        cache_tag: *const c_char,
        buffer: *const u8,
        buffer_size: usize,
        buffer_delete_cb: Option<DataDelete>,
        deleter_data: *mut c_void,
    );
}

/// Hermes's script cache, in the app's local folder. Files are keyed by script name, source hash,
/// Hermes version and cache tag, so an edited script or a new Hermes compiles again.
mod script_cache {
    use std::ffi::{c_char, c_void, CStr};
    use std::path::PathBuf;

    use super::ffi::DataDelete;

    unsafe fn text(s: *const c_char) -> String {
        if s.is_null() { String::new() } else { CStr::from_ptr(s).to_string_lossy().into_owned() }
    }

    unsafe fn cache_path(url: *const c_char, hash: u64, version: u64, tag: *const c_char) -> Option<PathBuf> {
        let dir = runtime::local_folder()?;
        let url = text(url);
        let name: String = url
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("script")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
            .take(64)
            .collect();
        let tag: String = text(tag).chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
        Some(PathBuf::from(dir).join("hermes-cache").join(format!("{name}.{hash:016x}.{version:x}.{tag}.hbc")))
    }

    unsafe extern "C" fn free_bytes(data: *mut c_void, deleter_data: *mut c_void) {
        let _ = data;
        drop(Box::from_raw(deleter_data as *mut Vec<u8>));
    }

    pub unsafe extern "C" fn load(
        _data: *mut c_void,
        source_url: *const c_char,
        source_hash: u64,
        _runtime_name: *const c_char,
        runtime_version: u64,
        cache_tag: *const c_char,
        buffer: *mut *const u8,
        buffer_size: *mut usize,
        buffer_delete_cb: *mut Option<DataDelete>,
        deleter_data: *mut *mut c_void,
    ) {
        *buffer = std::ptr::null();
        *buffer_size = 0;
        let Some(path) = cache_path(source_url, source_hash, runtime_version, cache_tag) else { return };
        let Ok(bytes) = std::fs::read(&path) else { return };
        if bytes.is_empty() {
            return;
        }
        let boxed = Box::new(bytes);
        *buffer = boxed.as_ptr();
        *buffer_size = boxed.len();
        *buffer_delete_cb = Some(free_bytes);
        *deleter_data = Box::into_raw(boxed) as *mut c_void;
    }

    pub unsafe extern "C" fn store(
        _data: *mut c_void,
        source_url: *const c_char,
        source_hash: u64,
        _runtime_name: *const c_char,
        runtime_version: u64,
        cache_tag: *const c_char,
        buffer: *const u8,
        buffer_size: usize,
        buffer_delete_cb: Option<DataDelete>,
        deleter_data: *mut c_void,
    ) {
        if let Some(path) = cache_path(source_url, source_hash, runtime_version, cache_tag) {
            if !buffer.is_null() && buffer_size > 0 {
                let bytes = std::slice::from_raw_parts(buffer, buffer_size);
                // Written beside the final name first, so a crash never leaves a partial cache file.
                let tmp = path.with_extension("tmp");
                let written = path.parent().map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&tmp, bytes))
                    .and_then(|_| std::fs::rename(&tmp, &path));
                if written.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
        if let Some(delete) = buffer_delete_cb {
            delete(buffer as *mut c_void, deleter_data);
        }
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
        || ffi::jsr_config_set_script_cache(
            config,
            std::ptr::null_mut(),
            Some(script_cache::load),
            Some(script_cache::store),
            None,
            std::ptr::null_mut(),
        ) != 0
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
