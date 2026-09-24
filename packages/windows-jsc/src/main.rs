//! Standalone host: the NativeScript Windows runtime on **JavaScriptCore**, no Node.
//! Built only with `--features jsc_link` (needs a real JavaScriptCore.dll/.lib in vendor/x64).
//! The JSC shim (statically linked) dllexports `napi_*`, so `napi::sys::setup()`'s GetProcAddress
//! -on-the-exe lookup finds them (same as the QuickJS package; no forwarders like Hermes needs).

use napi::{Env, JsUnknown, NapiRaw, NapiValue};
use ns_windows_demo::{crash, harness};
use std::ffi::CString;
use windows_jsc::ffi;

fn run_script(env: &Env, code: &str) -> Result<String, String> {
    let source = env.create_string(code).map_err(|e| e.to_string())?;
    let file = CString::new("<njsc-host>").unwrap();
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
    unsafe {
        std::mem::forget(napi::sys::setup());

        let mut runtime: ffi::NapiRuntime = std::ptr::null_mut();
        let mut env_raw: napi::sys::napi_env = std::ptr::null_mut();
        if ffi::js_create_runtime(&mut runtime) != 0
            || ffi::js_create_napi_env(&mut env_raw, runtime) != 0
            || env_raw.is_null()
        {
            eprintln!("failed to create JavaScriptCore napi_env");
            std::process::exit(1);
        }
        let env = Env::from_raw(env_raw);

        runtime::napi_engine::invoke::ensure_winrt_initialized();
        // JSC's JSGlobalObject ships a built-in console that is inert without a ConsoleClient
        // attached; drop it so install_globals's install-if-missing check installs the
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

        let _ = run_script(&env, ns_windows_common::url_polyfill::POLYFILL); // URL/URLSearchParams
        // Before the prelude, which builds `__nsModuleBuiltin` over the runner's natives.
        if let Err(e) = runtime::napi_engine::module_runner::install(&env, eval_for_modules) {
            eprintln!("module runner install failed: {e}");
        }
        let _ = run_script(&env, ns_windows_common::prelude::PRELUDE); // queueMicrotask + NSWinRT
        let drain = || {
            // No-op in the JSC shim (JSC drains microtasks when the VM returns to the host);
            // kept so all four hosts share the same loop contract.
            ffi::js_execute_pending_jobs(env_raw);
        };

        // App mode: `nativescript-windows <script.js>`: run the script, then drive the event
        // loop (timers, WinRT async completions, microtasks) until the app goes idle.
        if let Some(path) = std::env::args().skip(1).find(|a| !a.starts_with('-')) {
            // An ES-module app (`<entry>.mjs`) goes through the module runner.
            if path.ends_with(".mjs") {
                if let Err(e) = runtime::napi_engine::module_runner::run_entry(&env, &path, "") {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                runtime::napi_engine::event_loop::run_event_loop(&env, drain, None);
                return;
            }
            let code = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cannot read {path}: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = run_script(&env, &code) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            runtime::napi_engine::event_loop::run_event_loop(&env, drain, None);
            return;
        }

        if std::env::var("NSWIN_BENCH").is_ok() {
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
        ];
        let ok = harness::run_stages("windows-jsc", &stages, |code| run_script(&env, code))
            && harness::run_stages("windows-jsc", harness::FEATURE_STAGES, |code| {
                run_script(&env, code)
            })
            && harness::run_async_demo(
                "windows-jsc",
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

/// The engine's script evaluator for the module runner (`runtime::napi_engine::module_runner`).
fn eval_for_modules(env: &Env, code: &str, filename: &str) -> Result<napi::sys::napi_value, ()> {
    windows_jsc::eval_named(env, code, filename)
}
