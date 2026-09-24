//! The standalone host event loop: the piece that turns a bare engine + napi_env into an app
//! runtime. One iteration: drain engine microtasks, pump the Windows message queue (STA WinRT
//! async completions and cross-apartment delegate invokes arrive as messages), fire due timers,
//! then sleep until the next timer or message.
//!
//! The loop exits when it goes idle: no registered timers and a zero keep-alive count. The
//! keep-alive counter is exposed to JS as `__nsLoopRetain` / `__nsLoopRelease`; the prelude's
//! `NSWinRT.toPromise` retains while a WinRT async operation is outstanding and releases when it
//! settles, so `await`ing WinRT keeps the process alive exactly as long as needed (the same
//! ref-counted-pump contract as the Node package's nswinrt.js).
//!
//! Microtask draining is the only per-engine piece (`js_execute_pending_jobs` on the QuickJS/V8/
//! JSC shims, `jsr_drain_microtasks` on Hermes, a no-op on JSC which drains on VM exit), so the
//! host passes it in as a closure.

use std::cell::Cell;
use std::time::{Duration, Instant};

use napi::{Env, JsUnknown, ValueType};
use windows::Win32::UI::WindowsAndMessaging::{
    MsgWaitForMultipleObjectsEx, MWMO_INPUTAVAILABLE, QS_ALLINPUT,
};

use crate::napi_engine::timers;

thread_local! {
    static KEEP_ALIVE: Cell<i64> = const { Cell::new(0) };
}

/// Install `__nsLoopRetain` / `__nsLoopRelease` on the global object (install-if-missing; on
/// Node these are never consulted because nswinrt.js manages its own pump lifecycle).
pub fn install_loop_natives(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;
    let present = matches!(
        global
            .get_named_property::<JsUnknown>("__nsLoopRetain")
            .and_then(|v| v.get_type()),
        Ok(ValueType::Function)
    );
    if present {
        return Ok(());
    }
    let retain = env.create_function_from_closure("__nsLoopRetain", |_ctx| {
        KEEP_ALIVE.with(|k| k.set(k.get() + 1));
        Ok(())
    })?;
    global.set_named_property("__nsLoopRetain", retain)?;
    let release = env.create_function_from_closure("__nsLoopRelease", |_ctx| {
        KEEP_ALIVE.with(|k| k.set((k.get() - 1).max(0)));
        Ok(())
    })?;
    global.set_named_property("__nsLoopRelease", release)?;
    Ok(())
}

/// Run one iteration of the loop: drain microtasks, pump the Windows message queue, fire due
/// timers and immediates (draining microtasks between each). This is the unit the blocking
/// `run_event_loop` repeats; hosts that own their own frame loop (e.g. the WinUI 3 DLL, driven
/// from `CompositionTarget.Rendering`) call it directly once per frame instead.
pub fn pump_once<F: FnMut()>(env: &Env, drain_microtasks: &mut F) {
    drain_microtasks();
    crate::pump_messages();
    drain_microtasks();
    crate::napi_engine::websocket::pump(env);
    drain_microtasks();
    timers::run_due_timers(env);
    drain_microtasks();
    // Check phase: immediates queued so far (including by this iteration's timer callbacks)
    // fire now; ones they queue themselves wait for the next iteration.
    timers::run_due_immediates(env);
    drain_microtasks();
}

/// True when the loop has no more work: no registered timers and a zero keep-alive count.
pub fn is_idle() -> bool {
    !timers::has_pending() && KEEP_ALIVE.with(|k| k.get()) <= 0
}

/// Run the event loop until idle (no timers pending, keep-alive count zero). Returns `true` on a
/// clean idle exit, `false` if `deadline` elapsed first (used by demos/tests as a hang guard;
/// pass `None` for a real app).
pub fn run_event_loop<F: FnMut()>(
    env: &Env,
    mut drain_microtasks: F,
    deadline: Option<Duration>,
) -> bool {
    let start = Instant::now();
    loop {
        pump_once(env, &mut drain_microtasks);

        if is_idle() {
            return true;
        }
        if let Some(limit) = deadline {
            if start.elapsed() >= limit {
                return false;
            }
        }

        // Sleep until the next timer is due or input arrives, capped so DispatcherQueue work
        // (pumped inside pump_messages, not signaled through the message queue) never starves.
        // Pending immediates run on the very next iteration, so don't sleep at all.
        let wait_ms = if timers::has_immediates() {
            0
        } else {
            timers::next_due()
                .map(|due| due.saturating_duration_since(Instant::now()).as_millis() as u32)
                .unwrap_or(50)
                .min(50)
        };
        unsafe {
            MsgWaitForMultipleObjectsEx(None, wait_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
}
