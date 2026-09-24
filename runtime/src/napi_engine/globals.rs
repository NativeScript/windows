//! Installs the runtime's engine-neutral globals (`__time`, `performance`, `console`) for
//! Node-API hosts, sharing implementation with `globals/time.rs`, `globals/performance.rs`, and
//! the engine-thin parts of `globals/console.rs`.
//!
//! `install_globals` is install-if-missing: on hosts that already provide `performance` or
//! `console` (Node, Bun, Deno) the host's implementations win; on bare standalone engines
//! (QuickJS/Hermes shims) the runtime's implementations are installed. `__time` is always
//! installed (runtime-private).

use std::time::Instant;

use napi::{CallContext, Env, JsObject, JsUnknown, ValueType};

use crate::globals::console::{write_console, CONSOLE_TIMERS};
use crate::globals::time::PROCESS_START;

fn now_ms() -> f64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as f64 / 1_000_000.0
}

/// ToString-coerce an argument (missing → fallback), mirroring `to_rust_string_lossy`.
fn arg_string(ctx: &CallContext, index: usize, fallback: &str) -> String {
    if ctx.length <= index {
        return fallback.to_string();
    }
    match ctx.get::<JsUnknown>(index) {
        Ok(v) => v
            .coerce_to_string()
            .ok()
            .and_then(|s| s.into_utf8().ok())
            .and_then(|u| u.as_str().map(|s| s.to_owned()).ok())
            .unwrap_or_else(|| fallback.to_string()),
        Err(_) => fallback.to_string(),
    }
}

/// Install runtime globals on `global`. `global` is the host's global object (Node: reachable
/// via `env.get_global()`).
pub fn install_globals(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;

    // __time: always ours (runtime-private monotonic ms clock; origin shared with
    // performance.now via PROCESS_START).
    PROCESS_START.get_or_init(Instant::now);
    let time_fn = env.create_function_from_closure("__time", |_ctx| Ok(now_ms()))?;
    global.set_named_property("__time", time_fn)?;

    // performance.now: only when the host doesn't provide performance.
    let has_performance = matches!(
        global
            .get_named_property::<JsUnknown>("performance")
            .and_then(|v| v.get_type()),
        Ok(ValueType::Object) | Ok(ValueType::Function)
    );
    if !has_performance {
        let mut performance = env.create_object()?;
        let now_fn = env.create_function_from_closure("now", |_ctx| Ok(now_ms()))?;
        performance.set_named_property("now", now_fn)?;
        global.set_named_property("performance", performance)?;
    }

    // console: host wins entirely when present; otherwise install this module's implementation.
    let has_console = matches!(
        global
            .get_named_property::<JsUnknown>("console")
            .and_then(|v| v.get_type()),
        Ok(ValueType::Object)
    );
    if !has_console {
        let mut console = env.create_object()?;
        install_console_timers(env, &mut console)?;
        crate::napi_engine::console::install_console_formatters(env, &mut console)?;
        global.set_named_property("console", console)?;
    }

    // Native URL parse helpers (back the JS URL/URLSearchParams polyfill). Harmless to always
    // install; the polyfill only exposes URL when the host lacks it (Node/Bun/Deno already have it).
    crate::napi_engine::url::install_url_natives(env, &mut global)?;

    // interop.* natives + the NSWinRT.interop JS surface (runtime-private names; idempotent).
    crate::napi_engine::interop::install_interop(env)?;

    // .NET/BCL bridge natives + the NSWinRT.dotnet JS surface (runtime-private names;
    // idempotent; a no-op at the JS layer until a dotnet-bridge/publish/DotNetBridge.dll exists
    // next to the app).
    crate::napi_engine::dotnet::install_dotnet(env)?;

    // NSWinRT.asDelegate(typeName, fn): wraps a JS function as a COM-backed WinRT delegate.
    crate::napi_engine::delegate::install_as_delegate(env)?;

    // NSWinRT.makeItemsSource/extendItemsSource/insertItemsSource/removeItemsSource/
    // updateItemsSource/resetItemsSource: the native IObservableVector<IInspectable> ItemsSource
    // core's ListView (ui/list-view/index.windows.js) requires on Windows.
    crate::napi_engine::items_source::install_items_source(env)?;

    // Timers + event-loop keep-alive for standalone hosts (install-if-missing: on Node/Bun/Deno
    // the host's timers win and the keep-alive natives are simply never consulted).
    crate::napi_engine::timers::install_timers(env)?;
    // Native WebSocket natives (the prelude's WHATWG `WebSocket` wraps them); events are
    // delivered by the event loop.
    crate::napi_engine::websocket::install_websocket(env)?;
    crate::napi_engine::event_loop::install_loop_natives(env)?;
    Ok(())
}

/// console.time / timeEnd / timeLog / assert, sharing `CONSOLE_TIMERS` and `write_console` with
/// the v8 console so output and state stay identical across engines.
pub fn install_console_timers(env: &Env, console: &mut JsObject) -> napi::Result<()> {
    let time_fn = env.create_function_from_closure("time", |ctx: CallContext| {
        let label = arg_string(&ctx, 0, "default");
        CONSOLE_TIMERS.with(|t| {
            let mut map = t.borrow_mut();
            if map.contains_key(&label) {
                write_console(&format!("[WARN] Timer '{}' already exists\n", label));
            } else {
                map.insert(label, Instant::now());
            }
        });
        Ok(())
    })?;
    console.set_named_property("time", time_fn)?;

    let time_end_fn = env.create_function_from_closure("timeEnd", |ctx: CallContext| {
        let label = arg_string(&ctx, 0, "default");
        CONSOLE_TIMERS.with(|t| {
            let mut map = t.borrow_mut();
            if let Some(start) = map.remove(&label) {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                write_console(&format!("[INFO] {}: {:.3}ms - timer ended\n", label, ms));
            } else {
                write_console(&format!("[WARN] Timer '{}' does not exist\n", label));
            }
        });
        Ok(())
    })?;
    console.set_named_property("timeEnd", time_end_fn)?;

    let time_log_fn = env.create_function_from_closure("timeLog", |ctx: CallContext| {
        let label = arg_string(&ctx, 0, "default");
        CONSOLE_TIMERS.with(|t| {
            let map = t.borrow();
            if let Some(start) = map.get(&label) {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                write_console(&format!("[INFO] {}: {:.3}ms\n", label, ms));
            } else {
                write_console(&format!("[WARN] Timer '{}' does not exist\n", label));
            }
        });
        Ok(())
    })?;
    console.set_named_property("timeLog", time_log_fn)?;

    let assert_fn = env.create_function_from_closure("assert", |ctx: CallContext| {
        let passes = ctx.length > 0
            && ctx
                .get::<JsUnknown>(0)
                .and_then(|v| v.coerce_to_bool())
                .and_then(|b| b.get_value())
                .unwrap_or(false);
        if passes {
            return Ok(());
        }
        let mut value = String::from("[ERROR] Assertion failed");
        if ctx.length > 1 {
            value.push_str(": ");
            for i in 1..ctx.length {
                let part = arg_string(&ctx, i, "");
                value.push_str(&part);
                if i + 1 < ctx.length {
                    value.push(' ');
                }
            }
        } else {
            value.push_str(": console.assert");
        }
        value.push('\n');
        write_console(&value);
        Ok(())
    })?;
    console.set_named_property("assert", assert_fn)?;
    Ok(())
}
