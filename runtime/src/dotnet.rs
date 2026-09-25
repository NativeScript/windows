/// BCL / arbitrary .NET hosting via the .NET hostfxr API.
///
/// On first call to `try_init_dotnet` the runtime locates hostfxr.dll,
/// initialises a CLR context from the bridge project's runtimeconfig.json,
/// loads DotNetBridge.dll, and binds function pointers to the two exported
/// entry points:
///
///   `ns_dotnet_invoke(request_ptr, request_len, &response_ptr, &response_len) -> i32`
///   `ns_dotnet_free(ptr)`
///
/// The bridge uses UTF-8 throughout: request is a raw UTF-8 byte slice (no
/// allocation on the Rust side), and the response is a UTF-8 byte buffer
/// freed via `ns_dotnet_free`.  This eliminates the UTF-16 encode/decode
/// round-trip that the original char* ABI required.
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::{HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

/// Mode for initializing .NET host. Controlled by `NS_DOTNET_MODE` env var.
#[derive(PartialEq, Eq)]
enum DotnetMode {
    Auto,
    InProc,
    OutProc,
    Disabled,
}

fn get_dotnet_mode() -> DotnetMode {
    match std::env::var("NS_DOTNET_MODE") {
        Ok(s) => match s.to_ascii_lowercase().as_str() {
            "inproc" => DotnetMode::InProc,
            "outproc" => DotnetMode::OutProc,
            "disabled" | "none" | "0" => DotnetMode::Disabled,
            _ => DotnetMode::Auto,
        },
        Err(_) => DotnetMode::Auto,
    }
}

type FnInitForRuntimeConfig = unsafe extern "C" fn(
    runtime_config_path: *const u16,
    parameters: *const c_void,
    host_context_handle: *mut *mut c_void,
) -> i32;

type FnGetRuntimeDelegate = unsafe extern "C" fn(
    host_context_handle: *mut c_void,
    delegate_type: i32,
    delegate: *mut *mut c_void,
) -> i32;

type FnClose = unsafe extern "C" fn(host_context_handle: *mut c_void) -> i32;

type FnLoadAsmAndGetFnPtr = unsafe extern "C" fn(
    assembly_path: *const u16,
    type_name: *const u16,
    method_name: *const u16,
    delegate_type_name: *const u16,
    reserved: *const c_void,
    delegate: *mut *mut c_void,
) -> i32;

// Switching from char* (UTF-16) to byte* (UTF-8) eliminates:
//   • encode_utf16().collect() → Vec<u16> on every call
//   • String::from_utf16_lossy on every response
// The bridge uses JsonSerializer.Deserialize(ReadOnlySpan<byte>) and
// JsonSerializer.SerializeToUtf8Bytes(), both of which are zero-copy on the
// managed side.

type FnBridgeInvoke = unsafe extern "C" fn(
    request_ptr: *const u8,
    request_len: i32,
    response_ptr: *mut *mut u8,
    response_len: *mut i32,
) -> i32;

// Binary ABI: same signature, different entry point and wire format.
type FnBridgeInvokeBinary = unsafe extern "C" fn(
    request_ptr: *const u8,
    request_len: i32,
    response_ptr: *mut *mut u8,
    response_len: *mut i32,
) -> i32;

type FnBridgeFree = unsafe extern "C" fn(ptr: *mut u8);

// RegisterJsCallback: C# stores this pointer so managed delegates can call
// back into V8.  Signature: (callback_id, args_ptr, args_len, &resp_ptr, &resp_len) -> void
pub(crate) type FnJsCallback = unsafe extern "C" fn(i32, *const u8, i32, *mut *mut u8, *mut i32);
type FnRegisterJsCallback = unsafe extern "C" fn(callback: FnJsCallback) -> i32;

struct DotNetHost {
    _hostfxr: HMODULE,
    invoke: FnBridgeInvoke,
    invoke_binary: FnBridgeInvokeBinary,
    free: FnBridgeFree,
    register_js_callback: FnRegisterJsCallback,
}

// SAFETY: only ever accessed from the main JS/UI thread.
unsafe impl Send for DotNetHost {}
unsafe impl Sync for DotNetHost {}

static DOTNET_HOST: OnceLock<Result<DotNetHost, String>> = OnceLock::new();
static DOTNET_APP_ROOT: OnceLock<String> = OnceLock::new();

pub(crate) fn set_app_root(app_root: &str) {
    let _ = DOTNET_APP_ROOT.get_or_init(|| app_root.to_string());
}

// Which callback fires when a managed delegate/task/UI-thread hop calls back into JS. The
// rusty_v8 engine never registers one explicitly, so it falls back to the classic
// `global_fns::invoke_dotnet_js_callback` (isolate found via `DELEGATE_ISOLATE_PTR`). A napi
// engine registers `napi_engine::dotnet`'s own callback during bring-up, before any JS can reach
// `ensure_dotnet_initialized` — see `napi_engine::dotnet::install_dotnet`.
static JS_CALLBACK_DISPATCHER: OnceLock<FnJsCallback> = OnceLock::new();

pub(crate) fn set_js_callback_dispatcher(cb: FnJsCallback) {
    let _ = JS_CALLBACK_DISPATCHER.set(cb);
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_hostfxr() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("DOTNET_ROOT") {
        if let Some(p) = scan_fxr_dir(&PathBuf::from(root).join("host").join("fxr")) {
            return Some(p);
        }
    }
    for base in [
        r"C:\Program Files\dotnet\host\fxr",
        r"C:\Program Files (x86)\dotnet\host\fxr",
    ] {
        if let Some(p) = scan_fxr_dir(&PathBuf::from(base)) {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(home).join(".dotnet").join("host").join("fxr");
        if let Some(found) = scan_fxr_dir(&p) {
            return Some(found);
        }
    }
    None
}

fn parse_version(s: &str) -> (u64, u64, u64) {
    let mut parts = s.splitn(3, '.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

fn scan_fxr_dir(dir: &PathBuf) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();

    // Sort descending by numeric version so "10.x" sorts above "8.x".
    versions.sort_by(|a, b| {
        let av = parse_version(a.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        let bv = parse_version(b.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        bv.cmp(&av)
    });

    versions.into_iter().find_map(|v| {
        let candidate = v.join("hostfxr.dll");
        candidate.exists().then_some(candidate)
    })
}

fn build_host(bridge_dll: &str, runtime_config: &str) -> Result<DotNetHost, String> {
    // Prefer the hostfxr.dll that is already loaded into this process (e.g. when the
    // host is itself a .NET application such as the UWP toolbox).  Using a different
    // major-version hostfxr inside an already-hosted runtime causes InvalidArgFailure.
    let hostfxr_wide = to_wide_null("hostfxr.dll");
    let hostfxr = unsafe { GetModuleHandleW(PCWSTR(hostfxr_wide.as_ptr())).ok() }
        .map(|h| Ok(h))
        .unwrap_or_else(|| {
            let path = find_hostfxr()
                .ok_or_else(|| "hostfxr.dll not found — is .NET 6+ installed?".to_string())?;
            let wide = to_wide_null(&path.to_string_lossy());
            unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }
                .map_err(|e| format!("LoadLibraryW(hostfxr.dll): {e}"))
        })?;

    macro_rules! resolve {
        ($sym:literal, $ty:ty) => {{
            let name = concat!($sym, "\0");
            let ptr = unsafe { GetProcAddress(hostfxr, PCSTR(name.as_ptr())) }
                .ok_or_else(|| format!("GetProcAddress({}) failed", $sym))?;
            unsafe { std::mem::transmute::<_, $ty>(ptr) }
        }};
    }

    let fn_init: FnInitForRuntimeConfig = resolve!(
        "hostfxr_initialize_for_runtime_config",
        FnInitForRuntimeConfig
    );
    let fn_get_delegate: FnGetRuntimeDelegate =
        resolve!("hostfxr_get_runtime_delegate", FnGetRuntimeDelegate);
    let _fn_close: FnClose = resolve!("hostfxr_close", FnClose);

    let config_wide = to_wide_null(runtime_config);
    let mut ctx: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { fn_init(config_wide.as_ptr(), std::ptr::null(), &mut ctx) };
    if hr < 0 {
        let h = HRESULT(hr);
        let os_msg = crate::error::format_hresult_message(h);
        return Err(format!("hostfxr_initialize_for_runtime_config: {}", os_msg));
    }

    let mut load_fn_ptr: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { fn_get_delegate(ctx, 5, &mut load_fn_ptr) };
    if hr < 0 {
        let h = HRESULT(hr);
        let os_msg = crate::error::format_hresult_message(h);
        return Err(format!("hostfxr_get_runtime_delegate: {}", os_msg));
    }
    let load_asm: FnLoadAsmAndGetFnPtr = unsafe { std::mem::transmute(load_fn_ptr) };

    let dll_wide = to_wide_null(bridge_dll);
    let type_wide = to_wide_null("NativeScriptBridge.Bridge, DotNetBridge");

    // Sentinel defined by the .NET hosting API for [UnmanagedCallersOnly] methods.
    // Must be (const char_t*)-1, NOT null.  Passing null selects the typed-delegate
    // path which requires a matching C# delegate type and returns E_INVALIDARG for
    // [UnmanagedCallersOnly] methods.
    let unmanaged_callers_only: *const u16 = usize::MAX as *const u16;

    let bind = |method: &str| -> Result<*mut c_void, String> {
        let method_wide = to_wide_null(method);
        let mut fn_out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe {
            load_asm(
                dll_wide.as_ptr(),
                type_wide.as_ptr(),
                method_wide.as_ptr(),
                unmanaged_callers_only,
                std::ptr::null(),
                &mut fn_out,
            )
        };
        if hr < 0 {
            let h = HRESULT(hr);
            let os_msg = crate::error::format_hresult_message(h);
            return Err(format!(
                "load_assembly_and_get_function_pointer({method}): {}",
                os_msg
            ));
        }
        if fn_out.is_null() {
            return Err(format!(
                "load_assembly_and_get_function_pointer({method}): returned null"
            ));
        }
        Ok(fn_out)
    };

    let invoke: FnBridgeInvoke = unsafe { std::mem::transmute(bind("Invoke")?) };
    let invoke_binary: FnBridgeInvokeBinary = unsafe { std::mem::transmute(bind("InvokeBinary")?) };
    let free: FnBridgeFree = unsafe { std::mem::transmute(bind("Free")?) };
    let register_js_callback: FnRegisterJsCallback =
        unsafe { std::mem::transmute(bind("RegisterJsCallback")?) };

    Ok(DotNetHost {
        _hostfxr: hostfxr,
        invoke,
        invoke_binary,
        free,
        register_js_callback,
    })
}

// Search for the bridge DLL + runtimeconfig in a set of common locations.
fn find_bridge_and_config(app_root: &str) -> Option<(PathBuf, PathBuf)> {
    let root = PathBuf::from(app_root);

    // Prefer the explicit bridge publish folder where dotnet-tool writes its outputs.
    let mut candidates = vec![
        root.join("dotnet-bridge").join("publish"),
        root.join("dotnet-bridge"),
        root.join("publish").join("dotnet-bridge"),
        root.join("bin"),
        root.clone(),
    ];

    // Add the bridge folder at repo root (useful when running from template)
    if let Some(parent) = root.parent() {
        candidates.push(parent.join("dotnet-bridge").join("publish"));
    }

    // Depth-limited DFS to avoid scanning large trees.
    for base in candidates.into_iter() {
        if !base.exists() {
            continue;
        }
        let mut stack = vec![(base.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > 6 {
                continue;
            }
            if !dir.is_dir() {
                continue;
            }

            let dll = dir.join("DotNetBridge.dll");
            let cfg = dir.join("DotNetBridge.runtimeconfig.json");
            if dll.exists() && cfg.exists() {
                return Some((dll, cfg));
            }

            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push((p, depth + 1));
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn try_init_dotnet(app_root: &str) {
    // Respect NS_DOTNET_MODE so embedders can disable or choose out-of-proc.
    let mode = get_dotnet_mode();
    if mode == DotnetMode::Disabled {
        // Dotnet disabled by env var; skip initialization.
        let _ = DOTNET_HOST.get_or_init(|| Err("DotNet disabled via NS_DOTNET_MODE".to_string()));
        return;
    }

    // Initializing .NET host (verbosity suppressed)

    let res = DOTNET_HOST.get_or_init(|| {
        match find_bridge_and_config(app_root) {
                Some((bridge, config)) => {
                build_host(&bridge.to_string_lossy(), &config.to_string_lossy())
            }
            None => Err(format!(
                "DotNetBridge.dll and/or runtimeconfig not found under {} — run `dotnet publish` in dotnet-bridge/",
                app_root
            )),
        }
    });

    let _ = res;
}

/// Ensure the DotNet host is initialised (lazy). Uses stored `DOTNET_APP_ROOT` or
/// falls back to current directory when app root is not set.
fn ensure_dotnet_initialized() {
    if DOTNET_HOST.get().is_some() {
        return;
    }
    let app_root = DOTNET_APP_ROOT.get().map(|s| s.as_str()).unwrap_or(".");
    try_init_dotnet(app_root);
    // If initialisation succeeded, register the JS callback function so managed
    // delegates can call back into V8. This is idempotent because init_js_callbacks
    // is a no-op when the host isn't available.
    if DOTNET_HOST.get().and_then(|r| r.as_ref().ok()).is_some() {
        if let Some(dispatcher) = js_callback_dispatcher() {
            init_js_callbacks(dispatcher);
        }
    }
}

/// The dispatcher managed delegates call back into JS through: the one a napi engine registered
/// (`set_js_callback_dispatcher`), else the classic engine's V8 dispatcher.
fn js_callback_dispatcher() -> Option<FnJsCallback> {
    #[cfg(feature = "classic")]
    return Some(*JS_CALLBACK_DISPATCHER.get_or_init(|| crate::global_fns::invoke_dotnet_js_callback));
    #[cfg(not(feature = "classic"))]
    JS_CALLBACK_DISPATCHER.get().copied()
}

/// Calls the managed bridge with a JSON request string and returns the JSON
/// response string.  Uses the UTF-8 ABI: request bytes are passed directly
/// (`as_bytes()` — no allocation), response is decoded with
/// `from_utf8_unchecked` (the bridge always outputs valid JSON UTF-8).
pub(crate) fn call_dotnet(request_json: &str) -> Result<String, String> {
    ensure_dotnet_initialized();
    let host = DOTNET_HOST
        .get()
        .ok_or_else(|| "DotNet host not available".to_string())?
        .as_ref()
        .map_err(|e| e.clone())?;

    // Zero-copy: borrow the str's bytes directly — no Vec<u16> allocation.
    let bytes = request_json.as_bytes();
    let mut resp_ptr: *mut u8 = std::ptr::null_mut();
    let mut resp_len: i32 = 0;

    let hr = unsafe {
        (host.invoke)(
            bytes.as_ptr(),
            bytes.len() as i32,
            &mut resp_ptr,
            &mut resp_len,
        )
    };
    if hr < 0 {
        let h = HRESULT(hr);
        let os_msg = crate::error::format_hresult_message(h);
        let msg = format!("DotNetBridge.Invoke: {}", os_msg);
        crate::debug_output(&format!("[DOTNET] Invoke error: {}\n", msg));
        return Err(msg);
    }
    if resp_ptr.is_null() || resp_len < 0 {
        return Err("DotNetBridge.Invoke returned null/empty response".to_string());
    }

    let slice = unsafe { std::slice::from_raw_parts(resp_ptr, resp_len as usize) };
    // SAFETY: the bridge serialises JSON which is always valid UTF-8.
    let result = unsafe { std::str::from_utf8_unchecked(slice) }.to_owned();
    unsafe { (host.free)(resp_ptr) };
    // Trace pointer lookup calls to help debugging missing native ptrs.
    if request_json.contains("GetNativePtrForHandle") {
        // These request/response traces are very verbose — only emit when
        // explicit verbose debugging is requested via `NS_DEBUG`.
        if std::env::var("NS_DEBUG").is_ok() {
            crate::debug_output(&format!(
                "[DOTNET] request: {}\n[DOTNET] response: {}\n",
                request_json, result
            ));
        }
    }

    Ok(result)
}

/// Registers the Rust V8 callback function with the managed bridge so that
/// .NET delegates created via opcode 0x09 can call back into JavaScript.
/// Must be called after `try_init_dotnet` succeeds.
pub(crate) fn init_js_callbacks(callback: FnJsCallback) {
    let host = match DOTNET_HOST.get() {
        Some(Ok(h)) => h,
        _ => return,
    };
    unsafe { (host.register_js_callback)(callback) };
}

/// Calls the managed bridge with a pre-built binary request packet and returns
/// the raw binary response bytes.  No JSON involved on either side.
pub(crate) fn call_dotnet_binary(request: &[u8]) -> Result<Vec<u8>, String> {
    ensure_dotnet_initialized();
    let host = DOTNET_HOST
        .get()
        .ok_or_else(|| "DotNet host not available".to_string())?
        .as_ref()
        .map_err(|e| e.clone())?;

    let mut resp_ptr: *mut u8 = std::ptr::null_mut();
    let mut resp_len: i32 = 0;

    let hr = unsafe {
        (host.invoke_binary)(
            request.as_ptr(),
            request.len() as i32,
            &mut resp_ptr,
            &mut resp_len,
        )
    };
    if hr < 0 {
        let h = HRESULT(hr);
        let os_msg = crate::error::format_hresult_message(h);
        return Err(format!("DotNetBridge.InvokeBinary: {}", os_msg));
    }
    if resp_ptr.is_null() || resp_len < 0 {
        return Err("DotNetBridge.InvokeBinary returned null/empty response".to_string());
    }

    let slice = unsafe { std::slice::from_raw_parts(resp_ptr, resp_len as usize) };
    let result = slice.to_vec();
    unsafe { (host.free)(resp_ptr) };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{create_dir_all, File};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir() -> std::path::PathBuf {
        let mut base = env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("ns_dotnet_test_{}", nanos));
        base
    }

    #[test]
    fn find_bridge_in_publish() {
        let dir = make_temp_dir();
        let pubdir = dir.join("dotnet-bridge").join("publish");
        create_dir_all(&pubdir).unwrap();
        let dll = pubdir.join("DotNetBridge.dll");
        let cfg = pubdir.join("DotNetBridge.runtimeconfig.json");
        File::create(&dll).unwrap();
        let mut f = File::create(&cfg).unwrap();
        writeln!(f, "{{}}\n").unwrap();

        let found = find_bridge_and_config(dir.to_str().unwrap());
        assert!(found.is_some());
        let (found_dll, found_cfg) = found.unwrap();
        assert_eq!(found_dll, dll);
        assert_eq!(found_cfg, cfg);

        let _ = std::fs::remove_file(found_dll);
        let _ = std::fs::remove_file(found_cfg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn find_bridge_in_publish_parent() {
        let dir = make_temp_dir();
        create_dir_all(dir.join("publish").join("dotnet-bridge")).unwrap();
        let pubdir = dir.join("publish").join("dotnet-bridge");
        let dll = pubdir.join("DotNetBridge.dll");
        let cfg = pubdir.join("DotNetBridge.runtimeconfig.json");
        File::create(&dll).unwrap();
        File::create(&cfg).unwrap();

        let found = find_bridge_and_config(dir.to_str().unwrap());
        assert!(found.is_some());
        let (found_dll, found_cfg) = found.unwrap();
        assert_eq!(found_dll, dll);
        assert_eq!(found_cfg, cfg);

        let _ = std::fs::remove_file(found_dll);
        let _ = std::fs::remove_file(found_cfg);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Length-prefixed (u16) string for the bridge's tagged binary wire format.
pub(crate) fn bin_write_str16(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Length-prefixed (u32) string for the bridge's tagged binary wire format.
pub(crate) fn bin_write_str32(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}
