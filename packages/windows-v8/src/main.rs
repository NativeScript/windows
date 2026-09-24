//! Standalone host: the NativeScript Windows runtime on **V8**, no Node. Uses the bundled
//! rusty_v8 (same engine our classic runtime is built on) + napi-android's v8-api.cpp napi shim +
//! our Windows bring-up. The shim statically links and dllexports `napi_*`, so `napi::sys::setup()`'s
//! GetProcAddress-on-the-exe lookup finds them (like the QuickJS/JSC packages).

use napi::{CallContext, Env, JsUnknown, NapiRaw, NapiValue};
use ns_windows_demo::{crash, harness};
use std::ffi::CString;
use windows_v8::ffi;

fn run_script(env: &Env, code: &str) -> Result<String, String> {
    let source = env.create_string(code).map_err(|e| e.to_string())?;
    let file = CString::new("<nv8-host>").unwrap();
    let mut result: napi::sys::napi_value = std::ptr::null_mut();
    let status =
        unsafe { ffi::js_execute_script(env.raw(), source.raw(), file.as_ptr(), &mut result) };
    if status != 0 || result.is_null() {
        let mut pending = false;
        unsafe { napi::sys::napi_is_exception_pending(env.raw(), &mut pending) };
        if pending {
            let mut err: napi::sys::napi_value = std::ptr::null_mut();
            unsafe { napi::sys::napi_get_and_clear_last_exception(env.raw(), &mut err) };
            let msg = unsafe { JsUnknown::from_raw_unchecked(env.raw(), err) }
                .coerce_to_string()
                .and_then(|s| s.into_utf8())
                .and_then(|s| Ok(s.as_str()?.to_owned()))
                .unwrap_or_else(|_| "<unprintable exception>".into());
            return Err(format!("JS exception: {msg}"));
        }
        return Err(format!("js_execute_script status {status}"));
    }
    let val = unsafe { JsUnknown::from_raw_unchecked(env.raw(), result) };
    val.coerce_to_string()
        .and_then(|s| s.into_utf8())
        .and_then(|s| Ok(s.as_str()?.to_owned()))
        .map_err(|e| e.to_string())
}

fn main() {
    crash::install();

    // Initialize the V8 platform via rusty_v8 (the shim's C++ can't link NewDefaultPlatform's
    // libc++ unique_ptr). Must happen before js_create_runtime creates an isolate.
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    unsafe {
        std::mem::forget(napi::sys::setup());

        let mut runtime: ffi::NapiRuntime = std::ptr::null_mut();
        let mut env_raw: napi::sys::napi_env = std::ptr::null_mut();
        if ffi::js_create_runtime(&mut runtime) != 0
            || ffi::js_create_napi_env(&mut env_raw, runtime) != 0
            || env_raw.is_null()
        {
            eprintln!("failed to create V8 napi_env");
            std::process::exit(1);
        }
        let env = Env::from_raw(env_raw);

        runtime::napi_engine::invoke::ensure_winrt_initialized();
        // V8's context snapshot ships a built-in console that is inert without an attached
        // inspector; drop it so install_globals's install-if-missing check installs the
        // runtime's real console instead.
        let _ = run_script(&env, "delete globalThis.console;");
        if let Err(e) = runtime::napi_engine::globals::install_globals(&env) {
            eprintln!("install_globals failed: {e}");
            std::process::exit(1);
        }
        match runtime::napi_engine::ns_proxy::create_namespace_proxy(&env, "Windows") {
            Ok(windows_ns) => {
                if let Ok(mut global) = env.get_global() {
                    let _ = global.set_named_property("Windows", windows_ns);
                }
            }
            Err(e) => {
                eprintln!("namespace setup failed: {e}");
                std::process::exit(1);
            }
        }

        // Minimal `interop.arrayBufferFromBuffer` so the zero-copy stage can exercise the external
        // ArrayBuffer path (the Node package wires the full interop surface via nswinrt.js).
        if let Ok(mut interop_obj) = env.create_object() {
            if let Ok(f) = env.create_function_from_closure(
                "arrayBufferFromBuffer",
                |ctx: CallContext| {
                    let env = &ctx.env;
                    let buf = ctx.get::<JsUnknown>(0)?;
                    runtime::napi_engine::interop::array_buffer_from_buffer(env, &buf)
                },
            ) {
                let _ = interop_obj.set_named_property("arrayBufferFromBuffer", f);
            }
            if let Ok(mut g) = env.get_global() {
                let _ = g.set_named_property("interop", interop_obj);
            }
        }

        let _ = run_script(&env, ns_windows_common::url_polyfill::POLYFILL); // URL/URLSearchParams
        let _ = run_script(&env, ns_windows_common::prelude::PRELUDE); // queueMicrotask + NSWinRT
        let drain = || {
            ffi::js_execute_pending_jobs(env_raw);
        };

        // App mode: `nativescript-windows <script.js>`: run the script, then drive the event
        // loop (timers, WinRT async completions, microtasks) until the app goes idle.
        if let Some(path) = std::env::args().skip(1).find(|a| !a.starts_with('-')) {
            let code = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cannot read {path}: {e}");
                    std::process::exit(1);
                }
            };
            // An ES-module app (`<entry>.mjs`) goes through V8's module loader (csrc/win_jsr.cpp).
            if path.ends_with(".mjs") {
                let key = runtime::esm_loader::entry_key(&path, "");
                let (Ok(code), Ok(key)) = (CString::new(code), CString::new(key)) else {
                    eprintln!("{path}: source or path contains a NUL byte");
                    std::process::exit(1);
                };
                if ffi::js_run_module(env.raw(), code.as_ptr(), key.as_ptr()) != 0 {
                    std::process::exit(1);
                }
                runtime::napi_engine::event_loop::run_event_loop(&env, drain, None);
                return;
            }
            if let Err(e) = run_script(&env, &code) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            runtime::napi_engine::event_loop::run_event_loop(&env, drain, None);
            return;
        }

        if std::env::var("NSWIN_BENCH").is_ok() {
            // Host objects are the default hybrid path; set NSWIN_NO_HOSTOBJ=1 to bench full-Proxy.
            match run_script(&env, ns_windows_demo::bench::WORKLOAD) {
                Ok(v) => print!("{v}"),
                Err(e) => eprintln!("[bench] ERROR: {e}"),
            }
            return;
        }
        let stages = [
            ("engine", "1 + 1"),
            ("runtime globals (performance/__time)", "typeof performance + ',' + typeof __time"),
            ("runtime closure call (__time)", "typeof __time()"),
            ("Windows namespace", "typeof Windows"),
            ("namespace resolution (Windows.Data.Json)", "typeof Windows.Data.Json"),
            ("class resolution (JsonObject)", "typeof Windows.Data.Json.JsonObject"),
            ("static method resolution", "typeof Windows.Data.Json.JsonValue.CreateNumberValue"),
            ("enum resolution", "typeof Windows.Data.Json.JsonValueType.Number"),
            ("static method x10", "for (var i=0;i<10;i++) Windows.Data.Json.JsonValue.CreateNumberValue; 'ok'"),
            ("WinRT call #1 (round-trip)", "Windows.Data.Json.JsonValue.CreateNumberValue(5).GetNumber()"),
            ("WinRT call #2 (round-trip)", "Windows.Data.Json.JsonValue.CreateNumberValue(42).GetNumber()"),
            ("WinRT call #3 (string)", "Windows.Data.Json.JsonValue.CreateStringValue('hi').GetString()"),
            ("WinRT calls x20 (stress)", "var s=0; for (var i=0;i<20;i++) s+=Windows.Data.Json.JsonValue.CreateNumberValue(i).GetNumber(); s"),
            ("JsonObject round-trip", "var o=new Windows.Data.Json.JsonObject(); o.SetNamedValue('a', Windows.Data.Json.JsonValue.CreateNumberValue(7)); o.GetNamedNumber('a')+':'+o.Stringify()"),
            ("URL parse components", "var u=new URL('https://us:pw@ex.com:8443/a/b?x=1&y=2#h'); u.host+'|'+u.pathname+'|'+u.hash+'|'+u.origin"),
            ("URLSearchParams get/getAll", "var u=new URL('https://e.com/?a=1&a=2&b=3'); u.searchParams.getAll('a').join(',')+'|'+u.searchParams.get('b')"),
            ("URL setter syncs href", "var u=new URL('https://e.com/'); u.pathname='/x'; u.searchParams.set('q','hi'); u.href"),
            ("URL.canParse", "String(URL.canParse('http://ok/'))+','+String(URL.canParse('nope'))"),
            // Exercises napi_create_external_arraybuffer (the zero-copy path). Guarded so a missing
            // interop/IBuffer surface reports instead of failing the run.
            ("zero-copy ArrayBuffer", "try { var b = Windows.Security.Cryptography.CryptographicBuffer.GenerateRandom(8); var ab = interop.arrayBufferFromBuffer(b); 'byteLength='+ab.byteLength+' ('+(ab instanceof ArrayBuffer)+')' } catch(e) { 'skipped: '+e }"),
        ];
        let ok = harness::run_stages("windows-v8", &stages, |code| run_script(&env, code))
            && harness::run_stages("windows-v8", harness::FEATURE_STAGES, |code| {
                run_script(&env, code)
            })
            && harness::run_async_demo(
                "windows-v8",
                |code| run_script(&env, code),
                || {
                    runtime::napi_engine::event_loop::run_event_loop(
                        &env,
                        drain,
                        Some(std::time::Duration::from_secs(15)),
                    )
                },
            );
        if !ok {
            std::process::exit(1);
        }
    }
}
