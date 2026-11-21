//! Engine-neutral pieces of the standalone-host / WinUI 3 DLL bring-up.
//!
//! A napi-backed runtime DLL (`nativescript.dll` built for a non-V8 engine) has to expose the
//! same C ABI the classic runtime does — `runtime_init` / `runtime_runscript` /
//! `runtime_pump_timers`, etc. — so the WinUI 3 .NET host can P/Invoke it unchanged. Two parts of
//! that are engine-specific and live in each engine package (they need the engine's shim):
//!
//!   * creating the engine + its `napi_env`, and
//!   * evaluating a script string + draining that engine's microtask queue.
//!
//! Everything else — initializing WinRT, installing the runtime globals and the `Windows`
//! namespace, and driving one turn of the event loop — is identical across engines, so it lives
//! here (in-workspace, compiled with the `napi_engine` feature) rather than being copy-pasted into
//! every engine package. The engine wrapper supplies the `napi_env`; these helpers do the rest.

use napi::Env;

use crate::napi_engine::{event_loop, globals, invoke, module_natives, ns_proxy};

/// Initializes the WinRT runtime on an already-created `napi_env`:
///   1. initialize the WinRT apartment (idempotent),
///   2. install the runtime globals (`console`, `performance`, timers, `interop`, …),
///   3. resolve and expose the root `Windows` namespace on `globalThis`,
///   4. install the event-loop keep-alive natives (`__nsLoopRetain` / `__nsLoopRelease`),
///   5. install the CommonJS module natives (`__nsAppRoot`, `__nsReadTextFile`,
///      `__nsResolveModulePath`) the JS prelude's `require`/`module`/`exports` shim needs —
///      without these, webpack `target: 'node'` bundles throw `ReferenceError: require is not
///      defined` on evaluation.
///
/// This is exactly what the standalone hosts do before running app code; the only thing left to
/// the caller is running the engine's JS prelude/polyfills (which need the engine's own eval).
pub fn initialize_runtime(env: &Env, app_root: &str) -> napi::Result<()> {
    // Look for a sealed app.nsbundle next to app_root before anything else touches the
    // filesystem for JS source — module_natives' __nsReadTextFile/__nsResolveModulePath below
    // consult the decrypted in-memory table first and fall back to disk when none was found.
    crate::source_protect::init_from_app_root(app_root);
    install_panic_logging_hook();
    install_native_crash_handler();
    invoke::ensure_winrt_initialized();
    // Without this, ensure_dotnet_initialized() falls back to "." and never finds DotNetBridge.dll.
    crate::dotnet::set_app_root(app_root);
    globals::install_globals(env)?;
    let windows_ns = ns_proxy::create_namespace_proxy(env, "Windows")?;
    let mut global = env.get_global()?;
    global.set_named_property("Windows", windows_ns)?;
    // Microsoft (WinUI3) and NativeScript (native widget panels) are real WinRT namespaces too;
    // install_globals's install_dotnet already overwrote both with .NET-reflection proxies, so
    // these registrations must come after to win back native WinRT resolution.
    let microsoft_ns = ns_proxy::create_namespace_proxy(env, "Microsoft")?;
    global.set_named_property("Microsoft", microsoft_ns)?;
    let nativescript_ns = ns_proxy::create_namespace_proxy(env, "NativeScript")?;
    global.set_named_property("NativeScript", nativescript_ns)?;
    event_loop::install_loop_natives(env)?;
    module_natives::install_module_natives(env, app_root)?;
    Ok(())
}

/// Drive one turn of the event loop (timers, WinRT async completions, microtasks). WinUI 3 hosts
/// call this once per frame from `CompositionTarget.Rendering`; it never blocks. `drain_microtasks`
/// is the engine's microtask pump (e.g. `js_execute_pending_jobs` / `jsr_drain_microtasks`).
pub fn pump_once<F: FnMut()>(env: &Env, drain_microtasks: &mut F) {
    event_loop::pump_once_from_host(env, drain_microtasks);
}

/// True when the loop has no outstanding work (no timers, zero keep-alive count) — a self-hosted
/// host can use this to decide when to exit.
pub fn is_idle() -> bool {
    event_loop::is_idle()
}

/// A packaged WinUI 3 app has no console, so the default panic hook's stderr output is invisible.
/// Logs panics to the same trace log as everything else before the process goes down.
fn install_panic_logging_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::debug_output(&format!("[ERROR] [PANIC] {info}\n"));
            prev(info);
        }));
    });
}

/// Native faults bypass `std::panic` and are otherwise invisible in a packaged app.
/// `SetUnhandledExceptionFilter` is unreliable here (a managed .NET host's own exception
/// machinery can intercept the fault first); a vectored handler fires on every first-chance
/// exception regardless. Logs and continues the search — doesn't change how it's handled.
fn install_native_crash_handler() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS;

    type VectoredHandler = unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: VectoredHandler) -> *mut std::ffi::c_void;
    }

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    // Interesting codes only — skip the noise (e.g. 0x406D1388 thread-naming, and 0xE0434352,
    // the CLR's own code for every managed exception passthrough, even ones that get caught).
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;
    const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
    const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;
    // CLR "fatal execution engine error" — not a Win32 NTSTATUS, so it can't join the other
    // consts in one `u32` match arm (mixing raw hex literals of very different magnitudes in a
    // single `matches!` arm list reproducibly crashed rustc's codegen here — filed as its own
    // `if`, not because the logic differs).
    const COR_E_EXECUTIONENGINE: u32 = 0x8013_1506;

    static IN_HANDLER: AtomicBool = AtomicBool::new(false);
    static INSTALLED: std::sync::Once = std::sync::Once::new();

    unsafe extern "system" fn handler(info: *const EXCEPTION_POINTERS) -> i32 {
        let rec = if info.is_null() { std::ptr::null() } else { (*info).ExceptionRecord };
        if rec.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = (*rec).ExceptionCode.0 as u32;
        let interesting = matches!(
            code,
            EXCEPTION_ACCESS_VIOLATION
                | EXCEPTION_STACK_OVERFLOW
                | EXCEPTION_ILLEGAL_INSTRUCTION
                | EXCEPTION_BREAKPOINT
        ) || code == COR_E_EXECUTIONENGINE;
        if !interesting {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        if IN_HANDLER.swap(true, Ordering::SeqCst) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let addr = (*rec).ExceptionAddress as usize;
        let (access, data_addr) = if (*rec).NumberParameters >= 2 {
            ((*rec).ExceptionInformation[0], (*rec).ExceptionInformation[1])
        } else {
            (usize::MAX, 0)
        };
        let kind = match access {
            0 => "READ",
            1 => "WRITE",
            8 => "DEP/EXEC",
            _ => "?",
        };
        let module = module_containing(addr as *const std::ffi::c_void)
            .unwrap_or_else(|| "<unknown module>".to_string());
        let bt = backtrace::Backtrace::new();
        crate::debug_output(&format!(
            "[ERROR] [NATIVE_CRASH] code=0x{code:08X} instr=0x{addr:X} ({module}) {kind} of data_addr=0x{data_addr:X}\n{bt:?}\n"
        ));
        IN_HANDLER.store(false, Ordering::SeqCst);
        EXCEPTION_CONTINUE_SEARCH
    }

    INSTALLED.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, handler);
    });
}

/// Identifies which loaded module (by file name + offset) contains `addr` — far more reliable
/// than `backtrace`'s symbolication for a crash inside a module with no matching debug info
/// (e.g. a vendored engine DLL), which can otherwise report a misleading nearest-symbol guess.
fn module_containing(addr: *const std::ffi::c_void) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    };

    unsafe {
        let mut hmodule = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(addr as *const u16),
            &mut hmodule,
        )
        .ok()?;
        let mut buf = [0u16; 512];
        let len = GetModuleFileNameW(Some(hmodule), &mut buf);
        if len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let file_name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        let offset = (addr as usize).wrapping_sub(hmodule.0 as usize);
        Some(format!("{file_name}+0x{offset:X}"))
    }
}
