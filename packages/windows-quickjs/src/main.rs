//! Standalone host: the NativeScript Windows runtime running on **embedded QuickJS**,
//! with no Node/Bun/Deno. It wires three proven pieces together:
//!   1. windows-quickjs: quickjs-ng + the napi-android node_api shim → a `napi_env`.
//!   2. napi-rs `Env::from_raw` over that env (needs `napi::sys::setup()` first on Windows).
//!   3. runtime::napi_engine: the engine-neutral WinRT interop (globals, namespace proxies),
//!      unchanged from the Node package.
//!
//! Demonstrates real WinRT (JsonObject/JsonValue) driven from JS on QuickJS.

use napi::Env;
use ns_windows_demo::{crash, harness};
use windows_quickjs::shim;

fn main() {
    crash::install(); // symbolize any native crash via the host PDB
    unsafe {
        // Populate napi-rs's symbol table from this exe's (dllexport'd, statically-linked) shim.
        std::mem::forget(napi::sys::setup());

        let raw = shim::shared_env_ptr() as napi::sys::napi_env;
        if raw.is_null() {
            eprintln!("failed to create QuickJS napi_env");
            std::process::exit(1);
        }
        let env = Env::from_raw(raw);

        // Bring up the WinRT runtime over this env: identical calls to the Node package.
        runtime::napi_engine::invoke::ensure_winrt_initialized();
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

        // The full WinRT runtime running on standalone QuickJS (no Node/Bun/Deno).
        let raw_v = raw as *mut std::ffi::c_void;
        // Install the URL/URLSearchParams polyfill (QuickJS has no built-in URL) and the runtime
        // prelude (queueMicrotask + NSWinRT.toPromise over the event-loop keep-alive natives).
        let _ = shim::run_script_checked(raw_v, ns_windows_common::url_polyfill::POLYFILL);
        // Before the prelude, which builds `__nsModuleBuiltin` over the runner's natives.
        if let Err(e) = runtime::napi_engine::module_runner::install(&env, eval_for_modules) {
            eprintln!("module runner install failed: {e}");
        }
        let _ = shim::run_script_checked(raw_v, ns_windows_common::prelude::PRELUDE);
        let drain = || shim::drain_microtasks(raw_v);

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
            if let Err(e) = shim::run_script_checked(raw_v, &code) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            runtime::napi_engine::event_loop::run_event_loop(&env, drain, None);
            return;
        }

        // Benchmark mode: run the shared WinRT workload and exit (NSWIN_BENCH=1).
        if std::env::var("NSWIN_BENCH").is_ok() {
            match shim::run_script_checked(raw_v, ns_windows_demo::bench::WORKLOAD) {
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
            // Repeated static-method resolution (the double-free repro, now fixed).
            ("static method x10", "for (var i=0;i<10;i++) Windows.Data.Json.JsonValue.CreateNumberValue; 'ok'"),
            // WinRT method calls round-trip (static -> instance proxy -> instance method).
            ("WinRT call #1 (round-trip)", "Windows.Data.Json.JsonValue.CreateNumberValue(5).GetNumber()"),
            ("WinRT call #2 (round-trip)", "Windows.Data.Json.JsonValue.CreateNumberValue(42).GetNumber()"),
            ("WinRT call #3 (string)", "Windows.Data.Json.JsonValue.CreateStringValue('hi').GetString()"),
            ("WinRT calls x20 (stress)", "var s=0; for (var i=0;i<20;i++) s+=Windows.Data.Json.JsonValue.CreateNumberValue(i).GetNumber(); s"),
            // Full object round-trip: construct, set, stringify (ctor proxy + instance methods).
            ("JsonObject round-trip", "var o=new Windows.Data.Json.JsonObject(); o.SetNamedValue('a', Windows.Data.Json.JsonValue.CreateNumberValue(7)); o.GetNamedNumber('a')+':'+o.Stringify()"),
            // Force GC, then keep using the proxies: proves finalizers no longer corrupt state.
            ("post-GC reuse", "gc(); Windows.Data.Json.JsonValue.CreateNumberValue(99).GetNumber()"),
            // URL / URLSearchParams polyfill (native __urlParse/__urlWith over the url crate).
            ("URL parse components", "var u=new URL('https://us:pw@ex.com:8443/a/b?x=1&y=2#h'); u.host+'|'+u.pathname+'|'+u.hash+'|'+u.origin"),
            ("URLSearchParams get/getAll", "var u=new URL('https://e.com/?a=1&a=2&b=3'); u.searchParams.getAll('a').join(',')+'|'+u.searchParams.get('b')"),
            ("URL setter syncs href", "var u=new URL('https://e.com/'); u.pathname='/x'; u.searchParams.set('q','hi'); u.href"),
            ("URL.canParse", "String(URL.canParse('http://ok/'))+','+String(URL.canParse('nope'))"),
        ];
        let ok = harness::run_stages("windows-quickjs", &stages, |code| {
            shim::run_script_checked(raw_v, code).map_err(|e| e.to_string())
        }) && harness::run_stages("windows-quickjs", harness::FEATURE_STAGES, |code| {
            shim::run_script_checked(raw_v, code).map_err(|e| e.to_string())
        }) && harness::run_async_demo(
            "windows-quickjs",
            |code| shim::run_script_checked(raw_v, code).map_err(|e| e.to_string()),
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
    windows_quickjs::shim::eval_named(env, code, filename).map(|v| v as napi::sys::napi_value)
}
