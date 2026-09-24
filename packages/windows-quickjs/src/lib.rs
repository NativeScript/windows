//! QuickJS engine bindings: quickjs-ng embedded and run from Rust on Windows/MSVC.
//!
//! quickjs-ng is compiled via `cc` and evaluated through the FFI, with the napi-ios/android
//! `node_api.h` shim layered over it to expose a `napi_env`. The standalone host builds on top
//! of that; the `runtime::napi_engine` interop layer is engine-neutral and runs unchanged.

use std::ffi::{c_char, CStr, CString};

// The `nativescript.dll` C ABI (WinUI 3 host DLL), compiled only under the `host_dll` feature.
#[cfg(feature = "host_dll")]
pub mod abi;

extern "C" {
    fn qjs_eval_int(code: *const c_char, err: i32) -> i32;
    fn qjs_eval_str(code: *const c_char) -> *mut c_char;
    fn qjs_free(p: *mut c_char);
}

/// Evaluate `code`, returning its Int32 coercion (or `None` on exception / bad input).
pub fn eval_int(code: &str) -> Option<i32> {
    let c = CString::new(code).ok()?;
    const SENTINEL: i32 = i32::MIN;
    let v = unsafe { qjs_eval_int(c.as_ptr(), SENTINEL) };
    if v == SENTINEL {
        None
    } else {
        Some(v)
    }
}

/// Evaluate `code`, returning its string coercion (or `None` on exception).
pub fn eval_string(code: &str) -> Option<String> {
    let c = CString::new(code).ok()?;
    unsafe {
        let p = qjs_eval_str(c.as_ptr());
        if p.is_null() {
            return None;
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        qjs_free(p);
        Some(s)
    }
}

/// The napi-android node_api provider over quickjs-ng (quickjs-api.c + jsr.cpp), exercised
/// through the standard `napi_*` C ABI: proving the standalone Node-API provider works on
/// Windows. This is what a standalone host + the engine-neutral `runtime::napi_engine` layer
/// will run against (no Node required).
#[cfg(feature = "napi_shim")]
pub mod shim {
    use std::ffi::{c_char, c_void, CString};

    type NapiStatus = i32; // napi_ok == 0
    type NapiRuntime = *mut c_void;
    type NapiEnv = *mut c_void;
    type NapiValue = *mut c_void;
    const NAPI_AUTO_LENGTH: usize = usize::MAX;

    extern "C" {
        fn qjs_create_runtime(rt: *mut NapiRuntime) -> NapiStatus;
        fn qjs_create_napi_env(env: *mut NapiEnv, rt: NapiRuntime) -> NapiStatus;
        fn qjs_execute_script(
            env: NapiEnv,
            script: NapiValue,
            file: *const c_char,
            result: *mut NapiValue,
        ) -> NapiStatus;
        fn qjs_free_napi_env(env: NapiEnv) -> NapiStatus;
        fn qjs_free_runtime(rt: NapiRuntime) -> NapiStatus;
        fn qjs_execute_pending_jobs(env: NapiEnv) -> NapiStatus;
        fn napi_create_string_utf8(
            env: NapiEnv,
            s: *const c_char,
            len: usize,
            result: *mut NapiValue,
        ) -> NapiStatus;
        fn napi_get_value_int32(env: NapiEnv, v: NapiValue, result: *mut i32) -> NapiStatus;
        fn napi_get_value_string_utf8(
            env: NapiEnv,
            v: NapiValue,
            buf: *mut c_char,
            bufsize: usize,
            result: *mut usize,
        ) -> NapiStatus;
        fn napi_open_handle_scope(env: NapiEnv, result: *mut *mut c_void) -> NapiStatus;
        fn napi_close_handle_scope(env: NapiEnv, scope: *mut c_void) -> NapiStatus;
        fn napi_coerce_to_string(env: NapiEnv, v: NapiValue, result: *mut NapiValue) -> NapiStatus;
        fn napi_get_and_clear_last_exception(env: NapiEnv, result: *mut NapiValue) -> NapiStatus;
    }

    unsafe fn read_string(env: NapiEnv, v: NapiValue) -> String {
        let mut s_val: NapiValue = std::ptr::null_mut();
        let st1 = napi_coerce_to_string(env, v, &mut s_val);
        let mut buf = vec![0u8; 8192];
        let mut written = 0usize;
        let st2 = napi_get_value_string_utf8(
            env,
            s_val,
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
            &mut written,
        );
        runtime::debug_output(&format!(
            "[NativeScript] read_string: coerce_status={st1} s_val_null={} strval_status={st2} written={written}\n",
            s_val.is_null()
        ));
        std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned()
    }

    /// Like `run_script` but returns the thrown exception's message on failure (for diagnostics).
    pub fn run_script_checked(env: NapiEnv, code: &str) -> Result<String, String> {
        unsafe {
            let c = CString::new(code).map_err(|e| e.to_string())?;
            let mut script: NapiValue = std::ptr::null_mut();
            napi_create_string_utf8(env, c.as_ptr(), NAPI_AUTO_LENGTH, &mut script);
            let file = CString::new("<script>").unwrap();
            let mut result: NapiValue = std::ptr::null_mut();
            let st = qjs_execute_script(env, script, file.as_ptr(), &mut result);
            runtime::debug_output(&format!(
                "[NativeScript] run_script_checked: qjs_execute_script status={st} result_null={}\n",
                result.is_null()
            ));
            if st != 0 || result.is_null() {
                let mut exc: NapiValue = std::ptr::null_mut();
                let exc_st = napi_get_and_clear_last_exception(env, &mut exc);
                runtime::debug_output(&format!(
                    "[NativeScript] run_script_checked: get_and_clear_last_exception status={exc_st} exc_null={}\n",
                    exc.is_null()
                ));
                if !exc.is_null() {
                    return Err(read_string(env, exc));
                }
                return Err(format!("execute failed, status={st}"));
            }
            Ok(read_string(env, result))
        }
    }

    /// Evaluate `code` as a script named `filename` for the module runner, returning its
    /// completion value. On failure QuickJS's exception is left pending.
    pub fn eval_named(env: &napi::Env, code: &str, filename: &str) -> Result<NapiValue, ()> {
        use napi::NapiRaw;
        let (Ok(source), Ok(file)) = (env.create_string(code), CString::new(filename)) else {
            return Err(());
        };
        let mut result: NapiValue = std::ptr::null_mut();
        let status = unsafe {
            qjs_execute_script(env.raw() as NapiEnv, source.raw() as NapiValue, file.as_ptr(), &mut result)
        };
        if status != 0 || result.is_null() {
            return Err(());
        }
        Ok(result)
    }

    // One leaked runtime+env for the process (as a real host would keep). Avoids repeated
    // create/free cycles of the engine + shim global caches. Single-threaded use only.
    fn shared_env() -> NapiEnv {
        thread_local! {
            static ENV: std::cell::Cell<NapiEnv> = const { std::cell::Cell::new(std::ptr::null_mut()) };
        }
        ENV.with(|e| {
            let cur = e.get();
            if !cur.is_null() {
                return cur;
            }
            unsafe {
                let mut rt: NapiRuntime = std::ptr::null_mut();
                if qjs_create_runtime(&mut rt) != 0 || rt.is_null() {
                    return std::ptr::null_mut();
                }
                let mut env: NapiEnv = std::ptr::null_mut();
                if qjs_create_napi_env(&mut env, rt) != 0 {
                    return std::ptr::null_mut();
                }
                let mut scope: *mut c_void = std::ptr::null_mut();
                napi_open_handle_scope(env, &mut scope); // kept open for the process
                e.set(env);
                env
            }
        })
    }

    /// The process-lifetime shim `napi_env` as a raw pointer (for napi-rs `Env::from_raw`).
    pub fn shared_env_ptr() -> *mut c_void {
        shared_env()
    }

    /// Run QuickJS's pending-job queue to exhaustion (promise reactions): the standalone event
    /// loop's microtask drain hook.
    pub fn drain_microtasks(env: NapiEnv) {
        unsafe {
            qjs_execute_pending_jobs(env);
        }
    }

    /// Run `code` and return the result's string coercion (via qjs_execute_script).
    pub fn run_script(env: NapiEnv, code: &str) -> Option<String> {
        unsafe {
            let c = CString::new(code).ok()?;
            let mut script: NapiValue = std::ptr::null_mut();
            napi_create_string_utf8(env, c.as_ptr(), NAPI_AUTO_LENGTH, &mut script);
            let file = CString::new("<script>").unwrap();
            let mut result: NapiValue = std::ptr::null_mut();
            if qjs_execute_script(env, script, file.as_ptr(), &mut result) != 0 || result.is_null() {
                return None;
            }
            let mut s_val: NapiValue = std::ptr::null_mut();
            napi_coerce_to_string(env, result, &mut s_val);
            let mut buf = vec![0u8; 8192];
            let mut written = 0usize;
            napi_get_value_string_utf8(
                env,
                s_val,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
                &mut written,
            );
            Some(
                std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    /// Run `code` through the napi provider and return `(int32, string)` coercions of the result.
    pub fn eval_via_napi(code: &str) -> Option<(i32, String)> {
        unsafe {
            let env = shared_env();
            if env.is_null() {
                return None;
            }
            let c = CString::new(code).ok()?;
            let mut script: NapiValue = std::ptr::null_mut();
            napi_create_string_utf8(env, c.as_ptr(), NAPI_AUTO_LENGTH, &mut script);

            let file = CString::new("<napi-test>").unwrap();
            let mut result: NapiValue = std::ptr::null_mut();
            let st = qjs_execute_script(env, script, file.as_ptr(), &mut result);

            let out = if st == 0 && !result.is_null() {
                let mut i = 0i32;
                napi_get_value_int32(env, result, &mut i);
                // Coerce to string, then copy into a zeroed buffer. NB: this shim leaves the
                // out-length (`written`) at 0 even on success, but null-terminates the buffer:
                // so read it as a C string rather than trusting the length.
                let mut buf = vec![0u8; 8192];
                let mut written = 0usize;
                let mut s_val: NapiValue = std::ptr::null_mut();
                if napi_coerce_to_string(env, result, &mut s_val) == 0 {
                    napi_get_value_string_utf8(
                        env,
                        s_val,
                        buf.as_mut_ptr() as *mut c_char,
                        buf.len(),
                        &mut written,
                    );
                }
                let s = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char)
                    .to_string_lossy()
                    .into_owned();
                Some((i, s))
            } else {
                None
            };
            out
        }
    }

    /// Prove napi-rs's high-level `Env` API operates against the shim's `napi_env`: the exact
    /// bridge `runtime::napi_engine` relies on. If this works, the standalone host is just
    /// `Env::from_raw(shim_env)` → `napi_engine::install_globals` + run.
    pub fn napi_rs_bridge_probe() -> napi::Result<String> {
        // napi-rs resolves napi_* at runtime (GetProcAddress against this exe) on Windows, so
        // its symbol table must be populated once from the statically-linked, dllexport'd shim.
        static NAPI_LOADED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        NAPI_LOADED.get_or_init(|| unsafe {
            // Leak the returned Library so the module handle stays valid for the process.
            std::mem::forget(napi::sys::setup());
        });

        let raw = shared_env_ptr() as napi::sys::napi_env;
        let env = unsafe { napi::Env::from_raw(raw) };

        // Core operations napi_engine uses: get_global, create_string, create_object,
        // set_named_property, create_function_from_closure.
        let mut global = env.get_global()?;
        let s = env.create_string("bridge-ok")?;
        global.set_named_property("__probe", s)?;

        let f = env.create_function_from_closure("__add1", |ctx: napi::CallContext| {
            let n: i32 = ctx.get::<napi::JsNumber>(0)?.get_int32()?;
            ctx.env.create_int32(n + 1)
        })?;
        global.set_named_property("__add1", f)?;

        // Read the property back through the ENGINE (script sees what napi-rs set), and call
        // the Rust closure from JS: round-tripping napi-rs ⇄ shim ⇄ engine.
        let out = run_script(raw as *mut std::ffi::c_void, "__probe + ':' + __add1(41)")
            .unwrap_or_default();
        Ok(out)
    }

    /// Build a napi-backed Proxy whose `get` returns `inner` (helper for deep-nesting repro).
    fn proxy_returning(env: &napi::Env, inner: napi::JsUnknown) -> napi::Result<napi::JsObject> {
        use napi::{NapiRaw, NapiValue};
        // Keep `inner` alive across calls by stashing it in the closure via a napi_ref.
        let mut r: napi::sys::napi_ref = std::ptr::null_mut();
        unsafe { napi::sys::napi_create_reference(env.raw(), inner.raw(), 1, &mut r) };
        let r_addr = r as usize;
        let mut handler = env.create_object()?;
        let getf = env.create_function_from_closure("get", move |ctx: napi::CallContext| {
            let env = ctx.env;
            let mut out: napi::sys::napi_value = std::ptr::null_mut();
            unsafe {
                napi::sys::napi_get_reference_value(env.raw(), r_addr as napi::sys::napi_ref, &mut out);
                napi::JsUnknown::from_raw(env.raw(), out)
            }
        })?;
        handler.set_named_property("get", getf)?;
        let global = env.get_global()?;
        let proxy_ctor: napi::JsFunction = global.get_named_property("Proxy")?;
        let target = env.create_object()?;
        let (t, h) = unsafe {
            (
                napi::JsUnknown::from_raw(env.raw(), target.raw())?,
                napi::JsUnknown::from_raw(env.raw(), handler.raw())?,
            )
        };
        proxy_ctor.new_instance(&[t, h])
    }

    /// Deep-nesting repro: `__deep.a.b(5)`: two proxy levels then a leaf fn, matching the
    /// runtime's `Windows.Data.Json.JsonValue.CreateNumberValue(5)` shape.
    pub fn deep_nested_probe() -> napi::Result<String> {
        use napi::{NapiRaw, NapiValue};
        static LOADED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        LOADED.get_or_init(|| unsafe { std::mem::forget(napi::sys::setup()) });
        let raw = shared_env_ptr() as napi::sys::napi_env;
        let env = unsafe { napi::Env::from_raw(raw) };

        let leaf = env.create_function_from_closure("leaf", |c: napi::CallContext| {
            let n: i32 = c.get::<napi::JsNumber>(0).and_then(|v| v.get_int32()).unwrap_or(-1);
            c.env.create_int32(n + 900)
        })?;
        let leaf_un = unsafe { napi::JsUnknown::from_raw(env.raw(), leaf.raw())? };
        let level1 = proxy_returning(&env, leaf_un)?; // .b → leaf
        let level1_un = unsafe { napi::JsUnknown::from_raw(env.raw(), level1.raw())? };
        let level0 = proxy_returning(&env, level1_un)?; // .a → level1
        let mut g = env.get_global()?;
        g.set_named_property("__deep", level0)?;

        Ok(run_script(raw as *mut std::ffi::c_void, "__deep.a.b(5)").unwrap_or_else(|| "<none>".into()))
    }

    /// Isolate the standalone-host crash: a napi function CREATED INSIDE a Proxy get-trap,
    /// returned, then CALLED. (Directly-created functions like __add1 work; this is the case
    /// that faults in the runtime host.)
    pub fn nested_function_probe() -> napi::Result<String> {
        use napi::{NapiRaw, NapiValue};
        static LOADED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        LOADED.get_or_init(|| unsafe { std::mem::forget(napi::sys::setup()) });
        let raw = shared_env_ptr() as napi::sys::napi_env;
        let env = unsafe { napi::Env::from_raw(raw) };

        let mut handler = env.create_object()?;
        // get trap creates a fresh function each access and returns it.
        let getf = env.create_function_from_closure("get", |ctx: napi::CallContext| {
            ctx.env
                .create_function_from_closure("inner", |c2: napi::CallContext| {
                    let n: i32 = c2.get::<napi::JsNumber>(0).and_then(|v| v.get_int32()).unwrap_or(-1);
                    c2.env.create_int32(n + 100)
                })
        })?;
        handler.set_named_property("get", getf)?;
        let global = env.get_global()?;
        let proxy_ctor: napi::JsFunction = global.get_named_property("Proxy")?;
        let target = env.create_object()?;
        let (t_un, h_un) = unsafe {
            (
                napi::JsUnknown::from_raw(env.raw(), target.raw())?,
                napi::JsUnknown::from_raw(env.raw(), handler.raw())?,
            )
        };
        let proxy = proxy_ctor.new_instance(&[t_un, h_un])?;
        let mut g2 = env.get_global()?;
        g2.set_named_property("__nested", proxy)?;

        // Call a function returned from the trap.
        Ok(run_script(raw as *mut std::ffi::c_void, "__nested.whatever(5)").unwrap_or_else(|| "<none>".into()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn deep_nested_call() {
            // Two proxy levels then a leaf fn, called: matches the runtime host's shape.
            let out = deep_nested_probe().expect("deep nested probe failed");
            assert_eq!(out, "905");
        }

        #[test]
        fn nested_function_from_trap() {
            // A napi function created inside a Proxy get-trap, then called.
            let out = nested_function_probe().expect("nested function probe failed");
            assert_eq!(out, "105");
        }

        #[test]
        fn napi_rs_env_over_shim() {
            // napi-rs Env::from_raw(shim env): set a global + a Rust closure via napi-rs, then
            // observe both from the engine. Proves runtime::napi_engine can run on QuickJS.
            let out = napi_rs_bridge_probe().expect("napi-rs bridge over shim failed");
            assert_eq!(out, "bridge-ok:42");
        }

        #[test]
        fn napi_env_runs_js() {
            // The full chain: JSR runtime → napi_env → napi string → execute → read back.
            assert_eq!(eval_via_napi("40 + 2").map(|(i, _)| i), Some(42));
            assert_eq!(
                eval_via_napi("JSON.stringify({a:1})").map(|(_, s)| s),
                Some(r#"{"a":1}"#.to_string())
            );
            assert_eq!(
                eval_via_napi("'he'+'llo'").map(|(_, s)| s),
                Some("hello".to_string())
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        assert_eq!(eval_int("1 + 2"), Some(3));
        assert_eq!(eval_int("40 + 2"), Some(42));
        assert_eq!(eval_int("(function(){ let s=0; for(let i=0;i<=10;i++) s+=i; return s; })()"), Some(55));
    }

    #[test]
    fn strings_and_json() {
        assert_eq!(eval_string("'he' + 'llo'").as_deref(), Some("hello"));
        assert_eq!(
            eval_string("JSON.stringify({a:1,b:[2,3]})").as_deref(),
            Some(r#"{"a":1,"b":[2,3]}"#)
        );
        // Unicode round-trip through the engine.
        assert_eq!(eval_string("'h\u{00e9}llo\u{2713}'").as_deref(), Some("héllo✓"));
    }

    #[test]
    fn modern_js_features() {
        // Exercises regexp (libregexp) + spread + arrow + template literals.
        assert_eq!(
            eval_string("[...'a1b2c3'.matchAll(/\\d/g)].map(m=>m[0]).join('')").as_deref(),
            Some("123")
        );
        assert_eq!(eval_string("`${2**10}`").as_deref(), Some("1024"));
    }

    #[test]
    fn exceptions_are_none() {
        assert_eq!(eval_int("throw new Error('boom')"), None);
        assert_eq!(eval_string("not valid js !!!"), None);
    }
}
