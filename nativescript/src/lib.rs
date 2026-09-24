use runtime::Runtime;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Arc;
use std::sync::{Once, OnceLock};

static PANIC_HOOK_INIT: Once = Once::new();
static APP_ROOT: OnceLock<String> = OnceLock::new();
static LOCAL_FOLDER: OnceLock<String> = OnceLock::new();

fn install_panic_hook() {
    PANIC_HOOK_INIT.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown location>".to_string());
            let full = format!("[NativeScript] PANIC at {}: {}\n", location, msg);
            eprint!("{}", full);
            let log_dir = LOCAL_FOLDER
                .get()
                .map(|s| s.as_str())
                .or_else(|| APP_ROOT.get().map(|s| s.as_str()));
            let log_written = if let Some(dir) = log_dir {
                let path = std::path::Path::new(dir).join("nativescript-panic.log");
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(full.as_bytes())
                    })
                    .is_ok()
            } else {
                false
            };
            if !log_written {
                let temp = std::env::temp_dir().join("nativescript-panic.log");
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&temp)
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(full.as_bytes())
                    });
            }
            prev(info);
        }));
    });
}

// ── DIAGNOSTIC: vectored exception handler ──────────────────────────────────
// Captures the runtime's "last WinRT calls" ring buffer + native fault info to a
// file the moment a fatal native exception (severity-error, e.g. a XAML RoFailFast
// stowed exception 0xC000027B) is raised — before the process is torn down.
use std::sync::atomic::{AtomicBool, Ordering};
static VEH_INIT: Once = Once::new();
static VEH_DUMPED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct VehExceptionRecord {
    code: u32,
    flags: u32,
    next: *mut VehExceptionRecord,
    address: *mut c_void,
    number_parameters: u32,
    information: [usize; 15],
}
#[repr(C)]
struct VehExceptionPointers {
    exception_record: *mut VehExceptionRecord,
    context_record: *mut c_void,
}

extern "system" {
    fn AddVectoredExceptionHandler(
        first: u32,
        handler: unsafe extern "system" fn(*mut VehExceptionPointers) -> i32,
    ) -> *mut c_void;
    fn RtlCaptureStackBackTrace(
        skip: u32,
        capture: u32,
        back_trace: *mut *mut c_void,
        back_trace_hash: *mut u32,
    ) -> u16;
}

unsafe extern "system" fn ns_veh(info: *mut VehExceptionPointers) -> i32 {
    const CONTINUE_SEARCH: i32 = 0;
    if info.is_null() {
        return CONTINUE_SEARCH;
    }
    let rec = (*info).exception_record;
    if rec.is_null() {
        return CONTINUE_SEARCH;
    }
    let code = (*rec).code;
    // Only fatal native faults (top nibble 0xC = STATUS_SEVERITY_ERROR). This skips
    // benign C++ (0xE06D7363) and .NET (0xE0434352) language exceptions raised first-chance.
    if (code & 0xF000_0000) != 0xC000_0000 {
        return CONTINUE_SEARCH;
    }
    if VEH_DUMPED.swap(true, Ordering::SeqCst) {
        return CONTINUE_SEARCH;
    }

    let mut report = format!(
        "============================================================\n\
         [NativeScript VEH] fatal native exception\n\
         code=0x{:08X}  address=0x{:016X}  thread={:?}\n\
         --- native return addresses ---\n",
        code,
        (*rec).address as usize,
        std::thread::current().id(),
    );
    let mut frames: [*mut c_void; 48] = [std::ptr::null_mut(); 48];
    let n = RtlCaptureStackBackTrace(0, 48, frames.as_mut_ptr(), std::ptr::null_mut());
    for f in frames.iter().take(n as usize) {
        report.push_str(&format!("0x{:016X}\n", *f as usize));
    }
    report.push('\n');

    let dir = LOCAL_FOLDER
        .get()
        .map(|s| s.as_str())
        .or_else(|| APP_ROOT.get().map(|s| s.as_str()));
    if let Some(dir) = dir {
        let path = std::path::Path::new(dir).join("nativescript-veh.log");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(report.as_bytes())
            });
    }
    CONTINUE_SEARCH
}

fn install_veh() {
    VEH_INIT.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, ns_veh);
    });
}

#[cfg(feature = "devtools")]
use runtime_devtools::{DevtoolsServer, DevtoolsServerConfig};

#[cfg(feature = "devtools")]
thread_local! {
    static DEVTOOLS: std::cell::RefCell<Option<DevtoolsServer>> =
        std::cell::RefCell::new(None);
}

static CTRL_C_INIT: Once = Once::new();

#[no_mangle]
pub extern "C" fn runtime_install_ctrlc_handler(exit_code: i32) {
    install_panic_hook();
    CTRL_C_INIT.call_once(move || {
        let _ = ctrlc::set_handler(move || {
            println!("Ctrl+C received, shutting down runtime...");
            std::process::exit(exit_code);
        });
    });
}

#[no_mangle]
pub extern "C" fn runtime_set_local_folder(path: *const c_char) {
    if path.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .to_string();
    // Point the runtime's trace log (console.log) at the same folder as the crash/panic
    // logs, instead of the process temp dir (which, for a runFullTrust packaged app, is the system
    // temp — not where the CLI tails). Keep both in sync.
    runtime::set_log_dir(s.clone());
    let _ = LOCAL_FOLDER.set(s);
}

/// Supply a custom key (64 hex chars = 32 bytes) for opening a `key_mode == 1` app.nsbundle
/// container packed with `nsbundle_pack --key-hex`. Must be called before `runtime_init` — same
/// ordering requirement as `runtime_set_local_folder`. Returns 1 on success, 0 on malformed input
/// (wrong length or non-hex characters). Apps sealed with the default pepper (`key_mode == 0`,
/// i.e. `nsbundle_pack` invoked without `--key-hex`) never need to call this.
#[no_mangle]
pub extern "C" fn runtime_set_bundle_key(key_hex: *const c_char) -> c_int {
    if key_hex.is_null() {
        return 0;
    }
    let hex = unsafe { CStr::from_ptr(key_hex) }.to_string_lossy();
    runtime::source_protect::set_custom_key_hex(hex.as_ref()) as c_int
}

#[no_mangle]
pub extern "C" fn runtime_init(app_root: *const c_char) -> i64 {
    install_veh();
    // Debug (devtools) hosts may load ES modules from a dev server (Vite HMR).
    runtime::esm_http::set_debug_build(cfg!(feature = "devtools"));
    let result = std::panic::catch_unwind(|| {
        let mut boxed = if app_root.is_null() {
            Box::new(Runtime::new(""))
        } else {
            let string = unsafe { CStr::from_ptr(app_root) }.to_string_lossy();
            let _ = APP_ROOT.set(string.to_string());
            Box::new(Runtime::new(string.as_ref()))
        };
        boxed.register_delegate_isolate_ptr();
        Box::into_raw(boxed) as i64
    });
    match result {
        Ok(ptr) => ptr,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            eprintln!("[NativeScript] runtime_init panic: {}", msg);
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn runtime_deinit(runtime: i64) {
    if runtime != 0 {
        let _ = std::panic::catch_unwind(|| {
            let runtime: *mut Runtime = runtime as _;
            let _ = unsafe { Box::from_raw(runtime) };
        });
    }
}

#[no_mangle]
pub extern "C" fn runtime_runscript(runtime: i64, script: *const c_char, filename: *const c_char) {
    if runtime != 0 {
        let _ = std::panic::catch_unwind(|| {
            let runtime: *mut Runtime = runtime as _;
            let runtime = unsafe { &mut *runtime };
            let script = unsafe { CStr::from_ptr(script) }.to_string_lossy();
            let filename = if filename.is_null() {
                "main.js".to_string()
            } else {
                unsafe { CStr::from_ptr(filename) }
                    .to_string_lossy()
                    .into_owned()
            };
            runtime.run_script(script.as_ref(), &filename);
        });
    }
}

/// Forward a host lifecycle event into JS via `globalThis.__nsOnAppEvent(kind, message)`.
#[no_mangle]
pub extern "C" fn runtime_notify_app_event(runtime: i64, kind: c_int, message: *const c_char) {
    if runtime != 0 {
        let _ = std::panic::catch_unwind(|| {
            let runtime: *mut Runtime = runtime as _;
            let runtime = unsafe { &mut *runtime };
            if !message.is_null() {
                let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
                runtime.notify_app_event(kind, Some(&message));
            } else {
                runtime.notify_app_event(kind, None);
            }
        });
    }
}

/// Returns the last JS error (message + stack) or NULL. Caller must free with `runtime_free_js_error`.
#[no_mangle]
pub extern "C" fn runtime_get_last_js_error() -> *mut c_char {
    match runtime::get_last_js_error() {
        Some(s) => CString::new(s)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// Free a string previously returned by `runtime_get_last_js_error`.
#[no_mangle]
pub extern "C" fn runtime_free_js_error(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// Read a JS source file out of the sealed app.nsbundle loaded for this process, if any. Returns
/// NULL when no bundle was loaded, or the given path isn't in it — this doubles as an existence
/// probe, so callers (`RuntimeHost.cs`'s `VExists`/`VReadAllText`) can use one call for both.
/// Non-NULL results must be freed with `runtime_free_protected_string`.
#[no_mangle]
pub extern "C" fn runtime_read_protected_file(virtual_path: *const c_char) -> *mut c_char {
    if virtual_path.is_null() {
        return std::ptr::null_mut();
    }
    let path = unsafe { CStr::from_ptr(virtual_path) }.to_string_lossy();
    match runtime::source_protect::read_text(path.as_ref()) {
        Some(content) => CString::new(content)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// Free a string previously returned by `runtime_read_protected_file`.
#[no_mangle]
pub extern "C" fn runtime_free_protected_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

// ─── Devtools FFI ─────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn runtime_has_devtools() -> bool {
    cfg!(feature = "devtools")
}

/// Returns a null-terminated WebSocket URL on success, or NULL. Caller must free with `runtime_free_string`.
#[cfg(feature = "devtools")]
#[no_mangle]
pub extern "C" fn runtime_devtools_start(runtime: i64, port: u16) -> *mut c_char {
    if runtime == 0 {
        return std::ptr::null_mut();
    }
    let rt = unsafe { &mut *(runtime as *mut Runtime) };

    let config = DevtoolsServerConfig {
        host: "127.0.0.1".to_string(),
        port,
    };
    // Split borrows: copy the Global handle first, then take the mutable isolate borrow.
    let global_ctx = rt.global_context().clone();
    let forwarder: Option<Arc<dyn Fn(&str) + Send + Sync>> = Some(Arc::new(|s: &str| {
        runtime::debug_output(s);
    }));

    let dispatcher: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>> =
        Some(Arc::new(|msg: &str| {
            runtime::inspector::try_dispatch_inspector_message_to_js(msg)
        }));

    match DevtoolsServer::attach(
        &config,
        rt.isolate_mut(),
        &global_ctx,
        forwarder,
        dispatcher,
    ) {
        Err(_) => std::ptr::null_mut(),
        Ok(server) => {
            let ws_url = server.endpoint().websocket_url.clone();

            runtime::ASYNC_PUMP_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|| {
                    DEVTOOLS.with(|d| {
                        if let Ok(mut guard) = d.try_borrow_mut() {
                            if let Some(s) = guard.as_mut() {
                                s.pump_messages();
                            }
                        }
                    });
                }));
            });

            DEVTOOLS.with(|d| {
                *d.borrow_mut() = Some(server);
            });

            CString::new(ws_url)
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut())
        }
    }
}

#[cfg(feature = "devtools")]
#[no_mangle]
pub extern "C" fn runtime_devtools_pump(_runtime: i64) {
    let _ = std::panic::catch_unwind(|| {
        DEVTOOLS.with(|d| {
            if let Ok(mut guard) = d.try_borrow_mut() {
                if let Some(s) = guard.as_mut() {
                    s.pump_messages();
                }
            }
        });
    });
}

/// Drain the JS timer queue on the calling thread.
///
/// Must be called regularly on the V8/UI thread so that `setTimeout` /
/// `setInterval` callbacks fire. XAML hosts wire this to
/// `CompositionTarget.Rendering`; console hosts call it in their own loop.
///
/// Automatically detects context:
/// - XAML host: flushes V8 microtasks only (Win32 messages are pumped by XAML).
/// - Console/self-hosted: also drains Win32 messages so WinRT async `Completed`
///   callbacks (e.g. `BitmapImage.SetSourceAsync`) can fire.
#[no_mangle]
pub extern "C" fn runtime_pump_timers() {
    let _ = std::panic::catch_unwind(|| {
        runtime::timers::pump();
        if runtime::ui_dispatcher::needs_win32_pump() {
            runtime::pump_messages();
        } else {
            runtime::pump_dispatcher();
        }
    });
}

/// Pump Win32 messages and flush V8 microtasks.
///
/// For console apps and other hosts that have no XAML event loop.
/// Call this in a tight loop (e.g. `MsgWaitForMultipleObjects` or plain sleep loop)
/// to let WinRT async `Completed` callbacks (e.g. `BitmapImage.SetSourceAsync`)
/// fire and their Promise continuations run.
///
/// Do NOT call from `CompositionTarget.Rendering` — use `runtime_pump_timers` there.
///
/// Returns `true` if at least one Win32 message was dispatched.
#[no_mangle]
pub extern "C" fn runtime_pump_messages() -> bool {
    std::panic::catch_unwind(|| runtime::pump_messages()).unwrap_or(false)
}

/// Free a string previously returned by `runtime_devtools_start`.
#[cfg(feature = "devtools")]
#[no_mangle]
pub extern "C" fn runtime_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
