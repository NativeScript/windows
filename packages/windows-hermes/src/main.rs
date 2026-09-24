//! Standalone host: the NativeScript Windows runtime running on **Microsoft's prebuilt
//! Hermes**, with no Node/Bun/Deno. Hermes's `hermes.dll` exports both the JSR C API
//! (`jsr_create_runtime`, `jsr_runtime_get_node_api_env`, ...) and a full `napi_*` surface. So:
//!   1. `napi::sys::setup()` populates napi-sys from the exe's forwarded exports (build.rs
//!      re-exports Hermes's `napi_*`), so napi-rs's `Env` works.
//!   2. `jsr_*` brings up a Hermes runtime + napi_env; we open a napi_env scope on this thread.
//!   3. runtime::napi_engine (the engine-neutral WinRT interop) runs unchanged over that env.
//!
//! The FFI + engine bring-up (create runtime/env, run a script, drain microtasks) live in the
//! crate lib (`windows_hermes`) so the `nativescript.dll` adapter (`abi.rs`) reuses them.

use napi::Env;
use ns_windows_demo::{crash, harness};
use windows_hermes::{create_runtime_env, drain_microtasks, ffi, run_script};

fn main() {
    crash::install();
    unsafe {
        // 1. Populate napi-sys from the exe's forwarded Hermes napi_* exports.
        std::mem::forget(napi::sys::setup());

        // 2. Bring up a Hermes runtime + its napi_env, with a scope open on this thread.
        let (runtime, env_raw, scope) = match create_runtime_env() {
            Some(t) => t,
            None => {
                eprintln!("failed to create Hermes napi_env");
                std::process::exit(1);
            }
        };
        let env = Env::from_raw(env_raw);

        // 3. Bring up the WinRT runtime over this env: identical calls to the Node/QuickJS hosts.
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

        // The full WinRT runtime running on standalone Hermes (no Node/Bun/Deno).
        let _ = run_script(&env, ns_windows_common::url_polyfill::POLYFILL); // URL/URLSearchParams
        // Before the prelude, which builds `__nsModuleBuiltin` over the runner's natives.
        if let Err(e) = runtime::napi_engine::module_runner::install(&env, eval_for_modules) {
            eprintln!("module runner install failed: {e}");
        }
        let _ = run_script(&env, ns_windows_common::prelude::PRELUDE); // queueMicrotask + NSWinRT
        let drain = || drain_microtasks(env_raw);

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
            ffi::jsr_close_napi_env_scope(env_raw, scope);
            let _ = ffi::jsr_delete_runtime(runtime);
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
        let ok = harness::run_stages("windows-hermes", &stages, |code| run_script(&env, code))
            && harness::run_stages("windows-hermes", harness::FEATURE_STAGES, |code| {
                run_script(&env, code)
            })
            && harness::run_async_demo(
                "windows-hermes",
                |code| run_script(&env, code),
                || {
                    runtime::napi_engine::event_loop::run_event_loop(
                        &env,
                        drain,
                        Some(std::time::Duration::from_secs(15)),
                    )
                },
            );

        ffi::jsr_close_napi_env_scope(env_raw, scope);
        let _ = ffi::jsr_delete_runtime(runtime);

        if !ok {
            std::process::exit(1);
        }
    }
}

/// The engine's script evaluator for the module runner (`runtime::napi_engine::module_runner`).
fn eval_for_modules(env: &Env, code: &str, filename: &str) -> Result<napi::sys::napi_value, ()> {
    windows_hermes::eval_named(env, code, filename)
}
