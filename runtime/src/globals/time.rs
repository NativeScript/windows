use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic clock origin shared by both `__time` and `performance.now()`.
/// Initialized on first use (earliest of the two callers wins).
pub(crate) static PROCESS_START: OnceLock<Instant> = OnceLock::new();

#[cfg(feature = "classic")]
fn handle_time(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let start = PROCESS_START.get_or_init(Instant::now);
    retval.set_double(start.elapsed().as_nanos() as f64 / 1_000_000.0);
}

#[cfg(feature = "classic")]
pub fn init_time(scope: &mut v8::PinScope<'_, '_, ()>, global: &mut v8::Local<v8::ObjectTemplate>) {
    // Capture the start time as early as possible.
    PROCESS_START.get_or_init(Instant::now);
    let time = v8::FunctionTemplate::new(scope, handle_time);
    global.set(
        v8::String::new(scope, "__time").unwrap().into(),
        time.into(),
    );
}
