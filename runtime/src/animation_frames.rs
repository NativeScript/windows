//! `requestAnimationFrame` for the classic engine.
//!
//! JS queues callbacks (the prelude's `requestAnimationFrame`) and asks for a frame with
//! `__nsRequestFrame()`. The host's pump (`runtime_pump_timers`, which the app template drives
//! once per compositor frame from `CompositionTarget.Rendering`, outside the render walk) then
//! runs the queued callbacks once and drains microtasks, which is where rendering work such as
//! canvas presents happens. Nothing waits for vsync on the UI thread, and a continuous rAF loop
//! gives the dispatcher back between frames.

use std::cell::Cell;

use crate::DELEGATE_ISOLATE_PTR;

thread_local! {
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// `__nsRequestFrame()`: run animation callbacks at the next pump.
pub(crate) fn handle_request_frame(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    REQUESTED.with(|r| r.set(true));
}

/// Drops a pending request (the runtime on this thread is going away).
pub(crate) fn clear_thread() {
    REQUESTED.with(|r| r.set(false));
}

fn now_ms() -> f64 {
    crate::globals::time::PROCESS_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as f64
        / 1_000_000.0
}

/// Runs this thread's pending animation-frame callbacks, if a frame was requested, then drains
/// microtasks. Returns whether callbacks ran.
pub fn pump() -> bool {
    if !REQUESTED.with(|r| r.replace(false)) {
        return false;
    }
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return false;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    v8::scope!(scope, isolate);
    let Some(context) = scope.get_slot::<v8::Global<v8::Context>>().cloned() else {
        return false;
    };
    let context = v8::Local::new(scope, &context);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(tc, scope);

    let global = context.global(tc);
    let run = v8::String::new(tc, "__nsRunAnimationFrames")
        .and_then(|key| global.get(tc, key.into()))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok());
    let Some(run) = run else {
        return false;
    };
    let timestamp = v8::Number::new(tc, now_ms());
    let _ = run.call(tc, global.into(), &[timestamp.into()]);
    if tc.has_caught() {
        if let Some(message) = tc
            .exception()
            .and_then(|e| e.to_string(tc))
            .map(|s| s.to_rust_string_lossy(tc))
        {
            eprintln!("[NativeScript] requestAnimationFrame error: {message}");
        }
        tc.reset();
    }
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
    true
}
