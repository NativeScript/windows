use runtime_binding_gen::{RuntimeExtensionMetadata, RuntimeExtensionRegistry};
use std::collections::HashMap;
use std::fs;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use windows::core::{IUnknown, Interface};
use windows::Storage::Streams::IBuffer;
use windows::Win32::System::WinRT::IBufferByteAccess;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

use crate::type_description::build_runtime_type_descriptor;
use crate::dotnet::{bin_write_str16, bin_write_str32};
use crate::{normalize_js_path, proxy_manifests, throw_js_error, try_resolve_with_known_extensions, Runtime, ASYNC_PUMP_HOOK};
use std::cell::RefCell;
use std::ffi::c_void;

extern "system" {
    fn LocalAlloc(uFlags: u32, uBytes: usize) -> *mut c_void;
}

const LMEM_FIXED: u32 = 0x0000;

#[cfg(feature = "devtools")]
use runtime_devtools::{DevtoolsServer, DevtoolsServerConfig};

#[cfg(feature = "devtools")]
thread_local!(static DEVTOOLS_SERVER: RefCell<Option<DevtoolsServer>> = RefCell::new(None));

thread_local!(static INSPECTOR_DOMAIN_DISPATCHERS: RefCell<HashMap<String, v8::Global<v8::Value>>> = RefCell::new(HashMap::new()));

// Set once `Runtime::drop` has torn down the WinRT apartment (RoUninitialize).
// Backing-store deleters that hold COM refs consult this so they skip the
// Release during isolate disposal (which runs after RoUninitialize): the OS
// reclaims those refs at process exit, and calling Release on a dead apartment
// would access-violate. During normal GC the flag is false and refs are freed.
thread_local!(static COM_TEARDOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// Mark that this thread's COM apartment is being torn down. Called from
/// `Runtime::new` (reset) and `Runtime::drop` (set).
pub(crate) fn set_com_teardown(value: bool) {
    COM_TEARDOWN.with(|c| c.set(value));
}

/// Drop `value` (releasing any COM references it owns), or leak it while the apartment is being
/// torn down: see `COM_TEARDOWN`.
pub(crate) fn drop_unless_com_teardown<T>(value: T) {
    if COM_TEARDOWN.with(|c| c.get()) {
        std::mem::forget(value);
    } else {
        drop(value);
    }
}

/// Drop cached inspector dispatcher handles (isolate-tied `v8::Global`s).
/// Called from `Runtime::drop` while the isolate is still alive.
pub(crate) fn clear_thread_dispatchers() {
    INSPECTOR_DOMAIN_DISPATCHERS.with(|m| m.borrow_mut().clear());
}

const INSPECTOR_DISPATCHERS_GLOBAL: &str = "__nsInspectorDomainDispatchers";

pub(crate) fn value_to_string(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let value = value.to_string(scope)?;
    Some(value.to_rust_string_lossy(scope))
}

pub(crate) fn handle_run_on_ui_thread(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsRunOnUIThread(fn): expected a function argument");
        return;
    }

    let Ok(func) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_js_error(scope, "__nsRunOnUIThread: argument must be a function");
        return;
    };

    // v8::Global is !Send; store it by ID so only the i32 crosses into the closure.
    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::DOTNET_JS_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, v8::Global::new(scope, func));
    });

    crate::ui_dispatcher::post_to_ui_thread(move || {
        let isolate_ptr = crate::DELEGATE_ISOLATE_PTR.with(|c| c.get());
        if isolate_ptr.is_null() {
            crate::DOTNET_JS_CALLBACKS.with(|m| {
                m.borrow_mut().remove(&cb_id);
            });
            return;
        }
        let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
        v8::scope!(scope, isolate);
        let ctx_global = match scope.get_slot::<v8::Global<v8::Context>>() {
            Some(g) => g.clone(),
            None => {
                crate::DOTNET_JS_CALLBACKS.with(|m| {
                    m.borrow_mut().remove(&cb_id);
                });
                return;
            }
        };
        let context = v8::Local::new(scope, &ctx_global);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(tc, scope);

        let func_global = crate::DOTNET_JS_CALLBACKS.with(|m| m.borrow().get(&cb_id).cloned());
        let Some(func_global) = func_global else {
            return;
        };

        let func = v8::Local::new(tc, &func_global);
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let _ = func.call(tc, recv, &[]);

        if tc.has_caught() {
            if let Some(ex) = tc.exception() {
                let msg = ex.to_rust_string_lossy(tc);
                crate::store_last_js_error(msg);
            }
            tc.reset();
        }

        // Explicit microtasks policy: each native→JS task ends with a checkpoint.
        if !crate::defer_microtask_drain() {
            tc.perform_microtask_checkpoint();
        }

        crate::DOTNET_JS_CALLBACKS.with(|m| {
            m.borrow_mut().remove(&cb_id);
        });
    });
}

fn value_to_json_string(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let json = v8::json::stringify(scope, value)?;
    Some(json.to_rust_string_lossy(scope))
}

fn try_get_async_status(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Result<i32, String> {
    if !value.is_object() {
        return Err("Expected a wrapped WinRT async object".to_string());
    }
    let object = value
        .to_object(scope)
        .ok_or_else(|| "Expected a wrapped WinRT async object".to_string())?;
    let status_key = v8::String::new(scope, "Status")
        .ok_or_else(|| "Unable to allocate V8 string for async status lookup".to_string())?;
    let status = object
        .get(scope, status_key.into())
        .ok_or_else(|| "Async object does not expose Status".to_string())?;

    if let Ok(value) = v8::Local::<v8::Int32>::try_from(status) {
        return Ok(value.value());
    }
    if let Ok(value) = v8::Local::<v8::Uint32>::try_from(status) {
        return Ok(value.value() as i32);
    }
    if let Ok(value) = v8::Local::<v8::Number>::try_from(status) {
        return Ok(value.value() as i32);
    }
    if let Some(value) = status.integer_value(scope) {
        return Ok(value as i32);
    }
    if let Some(value) = status.number_value(scope) {
        if value.is_finite() {
            return Ok(value as i32);
        }
    }
    if let Some(value) = status.to_string(scope) {
        let s = value.to_rust_string_lossy(scope).to_ascii_lowercase();
        return match s.as_str() {
            "started" => Ok(0),
            "completed" => Ok(1),
            "canceled" | "cancelled" => Ok(2),
            "error" => Ok(3),
            _ => Err(format!("Async Status is not a recognized value: {s}")),
        };
    }
    Err("Async Status is not a numeric value".to_string())
}



fn default_auto_capture_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("NSWINRT_AUTO_METADATA_PATH") {
        return PathBuf::from(explicit);
    }
    if let Ok(out_dir) = std::env::var("SBG_OUTPUT_DIR") {
        return PathBuf::from(out_dir).join("sbg_metadata.json");
    }
    PathBuf::from("sbg_output").join("sbg_metadata.json")
}

fn polled_event_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    event: crate::worker_threads::PolledWorkerEvent,
) -> Option<v8::Local<'s, v8::Value>> {
    match event {
        crate::worker_threads::PolledWorkerEvent::Message(bytes) => {
            Runtime::deserialize_value(scope, &bytes)
        }
        crate::worker_threads::PolledWorkerEvent::Error(error) => {
            let obj = v8::Object::new(scope);
            if let Some(key) = v8::String::new(scope, "__workerError") {
                if let Some(val) = v8::String::new(scope, error.as_str()) {
                    obj.set(scope, key.into(), val.into());
                }
            }
            Some(obj.into())
        }
        crate::worker_threads::PolledWorkerEvent::Exited => {
            let obj = v8::Object::new(scope);
            if let Some(key) = v8::String::new(scope, "__workerExit") {
                obj.set(scope, key.into(), v8::Boolean::new(scope, true).into());
            }
            Some(obj.into())
        }
    }
}

pub(crate) fn handle_host_wait_for_async(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsHostWaitForAsync expects a WinRT async object");
        return;
    }

    let op_value = args.get(0);
    let timeout_ms = if args.length() >= 2 {
        let t = args.get(1);
        if let Some(v) = t.integer_value(scope) {
            if v >= 0 {
                v as u64
            } else {
                0
            }
        } else if let Some(v) = t.number_value(scope) {
            if v.is_finite() && v >= 0.0 {
                v as u64
            } else {
                0
            }
        } else {
            0
        }
    } else {
        0
    };

    match try_get_async_status(scope, op_value) {
        Ok(0) => {}
        Ok(_) => {
            retval.set(op_value);
            return;
        }
        Err(msg) => {
            throw_js_error(scope, msg.as_str());
            return;
        }
    }

    let deadline = if timeout_ms == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    let mut message = MSG::default();
    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                throw_js_error(
                    scope,
                    format!("Timed out waiting for WinRT async operation after {timeout_ms}ms")
                        .as_str(),
                );
                return;
            }
        }
        match try_get_async_status(scope, op_value) {
            Ok(0) => {
                // deadline == None means timeout_ms == 0: non-blocking check requested.
                // Return the op as-is rather than spinning forever.
                if deadline.is_none() {
                    retval.set(op_value);
                    return;
                }
                while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.into() {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                ASYNC_PUMP_HOOK.with(|hook| {
                    if let Ok(mut guard) = hook.try_borrow_mut() {
                        if let Some(f) = guard.as_mut() {
                            f();
                        }
                    }
                });
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(_) => {
                retval.set(op_value);
                return;
            }
            Err(msg) => {
                throw_js_error(scope, msg.as_str());
                return;
            }
        }
    }
}

// Inspector helpers moved to `inspector.rs`.
pub(crate) use crate::inspector::{
    handle_inspector_send_event, handle_inspector_timestamp, handle_register_domain_dispatcher,
    maybe_attach_devtools,
};

pub(crate) fn handle_enqueue_microtask(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsEnqueueMicrotask(callback) expects 1 argument");
        return;
    }
    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(cb) => cb,
        Err(_) => {
            throw_js_error(
                scope,
                "__nsEnqueueMicrotask(callback) expects callback to be a function",
            );
            return;
        }
    };
    scope.enqueue_microtask(callback);
    retval.set_undefined();
}

fn try_extract_pointer_from_value(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Option<*mut std::ffi::c_void> {
    if value.is_null_or_undefined() {
        return Some(std::ptr::null_mut());
    }
    if let Ok(external) = v8::Local::<v8::External>::try_from(value) {
        return Some(external.value());
    }
    if !value.is_object() {
        return None;
    }
    let object = value.to_object(scope)?;
    if let Some(handle_key) = v8::String::new(scope, "handle") {
        if let Some(handle) = object.get(scope, handle_key.into()) {
            if let Ok(external) = v8::Local::<v8::External>::try_from(handle) {
                return Some(external.value());
            }
            if handle.is_null_or_undefined() {
                return Some(std::ptr::null_mut());
            }
        }
    }
    None
}

pub(crate) fn handle_pointer_key(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsPointerKey expects a pointer-like value");
        return;
    }
    let pointer = match try_extract_pointer_from_value(scope, args.get(0)) {
        Some(p) => p,
        None => {
            throw_js_error(scope, "Unable to extract native pointer from value");
            return;
        }
    };
    let key = format!("0x{:x}", pointer as usize);
    if let Some(value) = v8::String::new(scope, key.as_str()) {
        retval.set(value.into());
    } else {
        retval.set_undefined();
    }
}

pub(crate) fn handle_buffer_to_pointer(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsBufferToPointer expects an ArrayBuffer or ArrayBufferView",
        );
        return;
    }
    let value = args.get(0);
    let pointer = if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        ab.data().map_or(std::ptr::null_mut(), |d| d.as_ptr())
    } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let byte_offset = view.byte_offset();
        let Some(buf) = view.buffer(scope) else {
            throw_js_error(scope, "ArrayBufferView does not expose a backing buffer");
            return;
        };
        buf.data().map_or(std::ptr::null_mut(), |d| unsafe {
            d.as_ptr().add(byte_offset)
        })
    } else if value.is_null_or_undefined() {
        std::ptr::null_mut()
    } else {
        throw_js_error(
            scope,
            "__nsBufferToPointer expects an ArrayBuffer or ArrayBufferView",
        );
        return;
    };

    if pointer.is_null() {
        retval.set_null();
    } else {
        retval.set(v8::External::new(scope, pointer).into());
    }
}

/// Backing-store deleter that releases the COM reference kept alive for the
/// lifetime of the aliasing ArrayBuffer. `deleter_data` is a `Box<IBuffer>`.
unsafe extern "C" fn release_ibuffer_keepalive(
    _data: *mut c_void,
    _byte_length: usize,
    deleter_data: *mut c_void,
) {
    if deleter_data.is_null() {
        return;
    }
    let keep_alive = unsafe { Box::from_raw(deleter_data as *mut IBuffer) };
    if COM_TEARDOWN.with(|c| c.get()) {
        // Apartment is gone; releasing now would access-violate. Leak the ref —
        // the process is exiting and the OS reclaims it.
        std::mem::forget(keep_alive);
    } else {
        drop(keep_alive);
    }
}

/// `__nsArrayBufferFromBuffer(buffer)` — wrap a `Windows.Storage.Streams.IBuffer`
/// as a zero-copy `ArrayBuffer` aliasing the buffer's native storage. The ArrayBuffer
/// covers `IBuffer.Length` (valid bytes). The IBuffer is AddRef'd and released only
/// once V8 collects the ArrayBuffer, so reads/writes through the view are safe and
/// propagate to the underlying WinRT buffer.
pub(crate) fn handle_array_buffer_from_buffer(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsArrayBufferFromBuffer expects a Windows.Storage.Streams.IBuffer",
        );
        return;
    }

    let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) else {
        throw_js_error(
            scope,
            "__nsArrayBufferFromBuffer expects a WinRT IBuffer object",
        );
        return;
    };

    let Some(instance) = crate::ns_proxy::this_instance(scope, obj) else {
        throw_js_error(
            scope,
            "__nsArrayBufferFromBuffer: value is not a WinRT object (no COM instance)",
        );
        return;
    };

    // QI for the valid length and the raw data pointer. Both views address the
    // same underlying object, so a single AddRef'd ref keeps the storage alive.
    let Ok(buffer) = instance.cast::<IBuffer>() else {
        throw_js_error(
            scope,
            "__nsArrayBufferFromBuffer: value does not implement Windows.Storage.Streams.IBuffer",
        );
        return;
    };
    let Ok(byte_access) = instance.cast::<IBufferByteAccess>() else {
        throw_js_error(
            scope,
            "__nsArrayBufferFromBuffer: IBuffer does not expose IBufferByteAccess",
        );
        return;
    };

    let len = buffer.Length().unwrap_or(0) as usize;
    let data = match unsafe { byte_access.Buffer() } {
        Ok(p) => p,
        Err(_) => {
            throw_js_error(scope, "__nsArrayBufferFromBuffer: failed to obtain IBuffer data pointer");
            return;
        }
    };

    if data.is_null() || len == 0 {
        // Nothing to alias — hand back an empty, owned ArrayBuffer.
        let ab = v8::ArrayBuffer::new(scope, 0);
        retval.set(ab.into());
        return;
    }

    // Hand an AddRef'd IBuffer to the deleter so the native storage outlives the
    // ArrayBuffer; `byte_access` is dropped here but the object stays alive.
    let keep_alive = Box::into_raw(Box::new(buffer)) as *mut c_void;
    let backing = unsafe {
        v8::ArrayBuffer::new_backing_store_from_ptr(
            data as *mut c_void,
            len,
            release_ibuffer_keepalive,
            keep_alive,
        )
    };
    let shared = backing.make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &shared);
    retval.set(ab.into());
}

pub(crate) fn handle_proxy_write_text_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(
            scope,
            "__nsProxyWriteTextFile(path, content) expects 2 arguments",
        );
        return;
    }
    let Some(path) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert path argument to string");
        return;
    };
    let Some(content) = value_to_string(scope, args.get(1)) else {
        throw_js_error(scope, "Unable to convert content argument to string");
        return;
    };
    let path_buf = PathBuf::from(&path);
    if let Some(parent) = path_buf.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = fs::create_dir_all(parent) {
                throw_js_error(scope, format!("Failed to create directory: {err}").as_str());
                return;
            }
        }
    }
    if let Err(err) = fs::write(&path_buf, content) {
        throw_js_error(scope, format!("Failed to write file: {err}").as_str());
        return;
    }
    retval.set_bool(true);
}

pub(crate) fn handle_proxy_compile_project(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsProxyCompileProject(csprojPath[, configuration]) expects at least 1 argument",
        );
        return;
    }
    let Some(project_path) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert csprojPath argument to string");
        return;
    };
    let configuration = if args.length() >= 2 {
        value_to_string(scope, args.get(1)).unwrap_or_else(|| "Debug".to_string())
    } else {
        "Debug".to_string()
    };
    let output = match Command::new("dotnet")
        .args([
            "build",
            &project_path,
            "-c",
            &configuration,
            "-v",
            "minimal",
        ])
        .output()
    {
        Ok(o) => o,
        Err(err) => {
            throw_js_error(
                scope,
                format!("Failed to execute dotnet build: {err}").as_str(),
            );
            return;
        }
    };

    let result = v8::Object::new(scope);
    let success = output.status.success();
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    macro_rules! set_prop {
        ($key:literal, $val:expr) => {
            if let Some(k) = v8::String::new(scope, $key) {
                result.set(scope, k.into(), $val);
            }
        };
    }
    set_prop!("success", v8::Boolean::new(scope, success).into());
    set_prop!("exitCode", v8::Integer::new(scope, exit_code).into());
    if let Some(v) = v8::String::new(scope, &stdout) {
        set_prop!("stdout", v.into());
    }
    if let Some(v) = v8::String::new(scope, &stderr) {
        set_prop!("stderr", v.into());
    }
    retval.set(result.into());
}

pub(crate) fn handle_proxy_register_manifest(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsProxyRegisterManifest(manifestJson) expects 1 argument",
        );
        return;
    }
    let Some(manifest) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert manifest argument to string");
        return;
    };
    let mut manifests = proxy_manifests().lock();
    manifests.push(manifest);
    let index = manifests.len() as i32 - 1;
    retval.set(v8::Integer::new(scope, index).into());
}

pub(crate) fn handle_proxy_auto_capture(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsProxyAutoCapture(metadataJson) expects 1 argument",
        );
        return;
    }
    let Some(metadata_json) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert metadata argument to string");
        return;
    };
    let path_buf = default_auto_capture_path();
    if let Some(parent) = path_buf.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = fs::create_dir_all(parent) {
                throw_js_error(
                    scope,
                    format!("Failed to create metadata directory: {err}").as_str(),
                );
                return;
            }
        }
    }
    let normalized =
        match serde_json::from_str::<Vec<RuntimeExtensionMetadata>>(metadata_json.as_str()) {
            Ok(extensions) => {
                let mut registry = RuntimeExtensionRegistry::new();
                for ext in extensions.iter().cloned() {
                    registry.register(ext);
                }
                match serde_json::to_string_pretty(&extensions) {
                    Ok(json) => json,
                    Err(err) => {
                        throw_js_error(
                            scope,
                            format!("Failed to normalize captured metadata: {err}").as_str(),
                        );
                        return;
                    }
                }
            }
            Err(_) => metadata_json,
        };
    if let Err(err) = fs::write(&path_buf, normalized) {
        throw_js_error(
            scope,
            format!("Failed to write captured metadata: {err}").as_str(),
        );
        return;
    }
    if let Some(path) = path_buf.to_str().and_then(|p| v8::String::new(scope, p)) {
        retval.set(path.into());
    } else {
        retval.set_bool(true);
    }
}

pub(crate) fn handle_read_text_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsReadTextFile(path) expects 1 argument");
        return;
    }
    // Reject null/undefined explicitly. value_to_string would otherwise coerce them
    // to the literal strings "null"/"undefined", leading to a confusing "file not
    // found" deeper in the call. A bad module path (e.g. loadModule(null)) must throw
    // a clear JS error here rather than panic the runtime.
    let path_arg = args.get(0);
    if path_arg.is_null_or_undefined() {
        throw_js_error(scope, "__nsReadTextFile: path is null or undefined");
        return;
    }
    let Some(path) = value_to_string(scope, path_arg) else {
        throw_js_error(scope, "Unable to convert path argument to string");
        return;
    };
    if path.is_empty() {
        throw_js_error(scope, "__nsReadTextFile: path is empty");
        return;
    }
    if let Some(content) = crate::source_protect::read_text(path.as_str()) {
        if let Some(value) = v8::String::new(scope, content.as_str()) {
            retval.set(value.into());
        } else {
            retval.set_null();
        }
        return;
    }
    match fs::read_to_string(Path::new(path.as_str())) {
        Ok(content) => {
            if let Some(value) = v8::String::new(scope, content.as_str()) {
                retval.set(value.into());
            } else {
                retval.set_null();
            }
        }
        Err(err) => throw_js_error(scope, format!("Failed to read module file: {err}").as_str()),
    }
}

pub(crate) fn handle_register_winmd(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsRegisterWinmd(path) expects 1 argument");
        return;
    }
    let Some(path) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert path argument to string");
        return;
    };
    match metadata::meta_data_reader::MetadataReader::register_winmd_file(path.as_str()) {
        Ok(()) => retval.set_bool(true),
        Err(err) => throw_js_error(scope, format!("__nsRegisterWinmd: {err}").as_str()),
    }
}

static PACKAGE_ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn package_root() -> Option<&'static PathBuf> {
    PACKAGE_ROOT
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        })
        .as_ref()
}

/// `__nsMsAppxResolve(uri)` — synchronously resolve a `ms-appx:///` URI to a
/// real filesystem path. Returns the path string if the file exists, or null.
///
/// Strips the `ms-appx:///` prefix and joins against the package install root
/// (the directory containing the app executable). Useful for synchronous
/// resource probing where async WinRT APIs are not available.
pub(crate) fn handle_ms_appx_resolve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let Some(uri) = value_to_string(scope, args.get(0)) else {
        retval.set_null();
        return;
    };

    const PREFIX: &str = "ms-appx:///";
    let relative = if uri.len() >= PREFIX.len() && uri[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
    {
        &uri[PREFIX.len()..]
    } else {
        uri.as_str()
    };

    let Some(root) = package_root() else {
        retval.set_null();
        return;
    };

    let full_path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    if full_path.exists() {
        match full_path.to_str().and_then(|s| v8::String::new(scope, s)) {
            Some(s) => retval.set(s.into()),
            None => retval.set_null(),
        }
    } else {
        retval.set_null();
    }
}

pub(crate) fn handle_livesync_copy_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(
            scope,
            "__nsLiveSyncCopyFile(sourcePath, destPath) expects 2 arguments",
        );
        return;
    }
    let Some(source_path) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert sourcePath argument to string");
        return;
    };
    let Some(dest_path) = value_to_string(scope, args.get(1)) else {
        throw_js_error(scope, "Unable to convert destPath argument to string");
        return;
    };
    if let Err(err) = crate::livesync::copy_file(source_path.as_str(), dest_path.as_str()) {
        throw_js_error(scope, err.as_str());
        return;
    }
    retval.set_bool(true);
}

pub(crate) fn handle_resolve_module_path(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(
            scope,
            "__nsResolveModulePath(specifier[, parentPath, appRoot]) expects at least 1 argument",
        );
        return;
    }
    // Reject null/undefined explicitly so a bad `loadModule(null)` surfaces as a clean
    // JS error instead of being coerced to the literal string "null" and then failing
    // deeper in the resolver. The module resolver returns null when it can't find a
    // registered module matching a path (e.g. an unregistered CSS file); passing that
    // null through must throw, never panic the runtime.
    let specifier_arg = args.get(0);
    if specifier_arg.is_null_or_undefined() {
        throw_js_error(
            scope,
            "__nsResolveModulePath: module specifier is null or undefined",
        );
        return;
    }
    let Some(specifier) = value_to_string(scope, specifier_arg) else {
        throw_js_error(scope, "Unable to convert module specifier to string");
        return;
    };
    if specifier.is_empty() {
        throw_js_error(scope, "__nsResolveModulePath: module specifier is empty");
        return;
    }
    let parent_path = if args.length() >= 2 {
        value_to_string(scope, args.get(1))
    } else {
        None
    };
    let app_root = if args.length() >= 3 {
        value_to_string(scope, args.get(2)).unwrap_or_default()
    } else {
        String::new()
    };

    let mut candidate = if specifier.starts_with("./") || specifier.starts_with("../") {
        let parent = parent_path
            .map(|v| normalize_js_path(v.as_str()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        // A packed (virtual, no-plaintext-on-disk) referrer never satisfies `.is_file()`, so also
        // check the sealed bundle's table — otherwise a relative require() from a bundle-only file
        // would wrongly treat the referrer itself as the base directory instead of its parent.
        let base = if parent.is_file() || crate::source_protect::contains(&parent.to_string_lossy())
        {
            parent.parent().map(Path::to_path_buf).unwrap_or(parent)
        } else {
            parent
        };
        base.join(&specifier)
    } else {
        let direct = normalize_js_path(specifier.as_str());
        if direct.is_absolute() {
            direct
        } else {
            let app_base = if app_root.is_empty() {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            } else {
                let lower = PathBuf::from(&app_root).join("app");
                if lower.exists() {
                    lower
                } else {
                    PathBuf::from(&app_root).join("App")
                }
            };
            app_base.join(direct)
        }
    };

    candidate = try_resolve_with_known_extensions(candidate);
    let resolved = candidate.canonicalize().unwrap_or(candidate);
    if let Some(value) = resolved.to_str().and_then(|p| v8::String::new(scope, p)) {
        retval.set(value.into());
    } else {
        retval.set_null();
    }
}

pub(crate) fn handle_proxy_list_manifests(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let manifests = proxy_manifests().lock();
    let array = v8::Array::new(scope, manifests.len() as i32);
    for (i, entry) in manifests.iter().enumerate() {
        if let Some(value) = v8::String::new(scope, entry.as_str()) {
            array.set_index(scope, i as u32, value.into());
        }
    }
    retval.set(array.into());
}

pub(crate) fn handle_describe_winrt_type(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsDescribeWinRTType(typeName) expects 1 argument");
        return;
    }
    let Some(type_name) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert typeName argument to string");
        return;
    };
    let Some(descriptor) = build_runtime_type_descriptor(type_name.as_str()) else {
        retval.set_null();
        return;
    };
    match serde_json::to_string(&descriptor) {
        Ok(json) => {
            if let Some(value) = v8::String::new(scope, json.as_str()) {
                retval.set(value.into());
            } else {
                retval.set_null();
            }
        }
        Err(error) => throw_js_error(
            scope,
            format!("Failed to serialize WinRT descriptor: {error}").as_str(),
        ),
    }
}

pub(crate) fn handle_worker_create_threaded(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 3 {
        throw_js_error(
            scope,
            "__nsWorkerCreateThreaded(source, filename, appRoot) expects 3 arguments",
        );
        return;
    }
    let Some(source) = value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "Unable to convert worker source to string");
        return;
    };
    let Some(filename) = value_to_string(scope, args.get(1)) else {
        throw_js_error(scope, "Unable to convert worker filename to string");
        return;
    };
    let Some(app_root) = value_to_string(scope, args.get(2)) else {
        throw_js_error(scope, "Unable to convert appRoot to string");
        return;
    };
    match crate::worker_threads::create_worker(app_root, source, filename) {
        Ok(worker_id) => retval.set_double(worker_id as f64),
        Err(err) => throw_js_error(scope, err.as_str()),
    }
}

pub(crate) fn handle_worker_post_message(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(
            scope,
            "__nsWorkerPostMessage(workerId, value) expects 2 arguments",
        );
        return;
    }
    let worker_id = args.get(0).number_value(scope).unwrap_or(-1.0);
    if worker_id < 0.0 {
        throw_js_error(scope, "Invalid worker id");
        return;
    }
    let value = args.get(1);
    let Some(bytes) = Runtime::serialize_value(scope, value) else {
        throw_js_error(scope, "DataCloneError: value could not be cloned.");
        return;
    };
    if let Err(err) = crate::worker_threads::post_message(worker_id as u64, bytes) {
        throw_js_error(scope, err.as_str());
    }
}

pub(crate) fn handle_worker_poll_messages(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsWorkerPollMessages(workerId) expects 1 argument");
        return;
    }
    let worker_id = args.get(0).number_value(scope).unwrap_or(-1.0);
    if worker_id < 0.0 {
        throw_js_error(scope, "Invalid worker id");
        return;
    }
    let events = match crate::worker_threads::poll_events(worker_id as u64) {
        Ok(e) => e,
        Err(err) => {
            throw_js_error(scope, err.as_str());
            return;
        }
    };
    let array = v8::Array::new(scope, events.len() as i32);
    for (index, event) in events.into_iter().enumerate() {
        if let Some(value) = polled_event_to_v8(scope, event) {
            array.set_index(scope, index as u32, value);
        }
    }
    retval.set(array.into());
}

pub(crate) fn handle_worker_terminate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsWorkerTerminate(workerId) expects 1 argument");
        return;
    }
    let worker_id = args.get(0).number_value(scope).unwrap_or(-1.0);
    if worker_id < 0.0 {
        throw_js_error(scope, "Invalid worker id");
        return;
    }
    if let Err(err) = crate::worker_threads::terminate_worker(worker_id as u64) {
        throw_js_error(scope, err.as_str());
    }
}

pub(crate) fn handle_worker_poll_messages_blocking(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(
            scope,
            "__nsWorkerPollMessagesBlocking(workerId, timeoutMs) expects 2 arguments",
        );
        return;
    }
    let worker_id = args.get(0).number_value(scope).unwrap_or(-1.0);
    if worker_id < 0.0 {
        throw_js_error(scope, "Invalid worker id");
        return;
    }
    let timeout_ms = args.get(1).number_value(scope).unwrap_or(0.0);
    let timeout_ms = if timeout_ms.is_sign_negative() {
        0_u64
    } else {
        timeout_ms as u64
    };
    let events = match crate::worker_threads::poll_events_blocking(worker_id as u64, timeout_ms) {
        Ok(e) => e,
        Err(err) => {
            throw_js_error(scope, err.as_str());
            return;
        }
    };
    let array = v8::Array::new(scope, events.len() as i32);
    for (index, event) in events.into_iter().enumerate() {
        if let Some(value) = polled_event_to_v8(scope, event) {
            array.set_index(scope, index as u32, value);
        }
    }
    retval.set(array.into());
}

/// Installed into every context. Provides NSWinRT, module loader, async helpers,
/// proxy extension infrastructure, and the `Function.prototype.extend` NativeScript API.
const HELPER_SOURCE: &str = r#"
        (function () {
            if (typeof globalThis.queueMicrotask !== 'function') {
                globalThis.queueMicrotask = function (callback) {
                    if (typeof callback !== 'function') {
                        throw new TypeError('queueMicrotask callback must be a function');
                    }

                    if (typeof globalThis.__nsEnqueueMicrotask === 'function') {
                        globalThis.__nsEnqueueMicrotask(callback);
                        return;
                    }

                    Promise.resolve().then(callback).catch(function (err) {
                        if (typeof globalThis.__ns__setTimeout === 'function') {
                            globalThis.__ns__setTimeout(function () { throw err; }, 0);
                        } else if (typeof globalThis.setTimeout === 'function') {
                            globalThis.setTimeout(function () { throw err; }, 0);
                        } else {
                            throw err;
                        }
                    });
                };
            }

            // WHATWG TextEncoder/TextDecoder (UTF-8), which the iOS/Android runtimes provide natively
            // and bundled libraries assume exist.
            if (typeof globalThis.TextEncoder !== 'function') {
                var TextEncoder = function TextEncoder() {};
                TextEncoder.prototype.encoding = 'utf-8';
                TextEncoder.prototype.encodeInto = function (input, dest) {
                    var s = String(input), read = 0, written = 0, n = dest.length;
                    for (var i = 0; i < s.length; i++) {
                        var c = s.charCodeAt(i), units = 1;
                        if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
                            var d = s.charCodeAt(i + 1);
                            if (d >= 0xdc00 && d <= 0xdfff) { c = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00); units = 2; }
                        }
                        if (c >= 0xd800 && c <= 0xdfff) c = 0xfffd; // lone surrogate
                        var len = c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
                        if (written + len > n) break;
                        if (len === 1) dest[written++] = c;
                        else if (len === 2) { dest[written++] = 0xc0 | (c >> 6); dest[written++] = 0x80 | (c & 63); }
                        else if (len === 3) { dest[written++] = 0xe0 | (c >> 12); dest[written++] = 0x80 | ((c >> 6) & 63); dest[written++] = 0x80 | (c & 63); }
                        else { dest[written++] = 0xf0 | (c >> 18); dest[written++] = 0x80 | ((c >> 12) & 63); dest[written++] = 0x80 | ((c >> 6) & 63); dest[written++] = 0x80 | (c & 63); }
                        read += units;
                        i += units - 1;
                    }
                    return { read: read, written: written };
                };
                TextEncoder.prototype.encode = function (input) {
                    var s = input === undefined ? '' : String(input);
                    var buf = new Uint8Array(s.length * 3);
                    return buf.slice(0, this.encodeInto(s, buf).written);
                };
                globalThis.TextEncoder = TextEncoder;
            }
            if (typeof globalThis.TextDecoder !== 'function') {
                var TextDecoder = function TextDecoder(label, options) {
                    var enc = label === undefined ? 'utf-8' : String(label).trim().toLowerCase();
                    if (enc !== 'utf-8' && enc !== 'utf8' && enc !== 'unicode-1-1-utf-8') {
                        throw new RangeError('TextDecoder: unsupported encoding "' + label + '" (only utf-8 is available)');
                    }
                    this.encoding = 'utf-8';
                    this.fatal = !!(options && options.fatal);
                    this.ignoreBOM = !!(options && options.ignoreBOM);
                    this._pending = null;
                    this._bomSeen = false;
                };
                TextDecoder.prototype.decode = function (input, options) {
                    var bytes = input === undefined ? new Uint8Array(0)
                        : input instanceof ArrayBuffer ? new Uint8Array(input)
                        : new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
                    if (this._pending) {
                        var joined = new Uint8Array(this._pending.length + bytes.length);
                        joined.set(this._pending); joined.set(bytes, this._pending.length);
                        bytes = joined; this._pending = null;
                    }
                    var stream = !!(options && options.stream), out = '', chunk = [], i = 0, n = bytes.length;
                    var fatal = this.fatal;
                    function bad() { if (fatal) throw new TypeError('TextDecoder: invalid utf-8'); chunk.push(0xfffd); }
                    while (i < n) {
                        var b = bytes[i], need = b < 0x80 ? 0 : (b & 0xe0) === 0xc0 ? 1 : (b & 0xf0) === 0xe0 ? 2 : (b & 0xf8) === 0xf0 ? 3 : -1;
                        if (need < 0) { bad(); i++; continue; }
                        if (i + need >= n + (need ? 0 : 1)) {
                            if (stream) { this._pending = bytes.slice(i); break; }
                            bad(); break;
                        }
                        var c = need === 0 ? b : b & (0x3f >> need), ok = true;
                        for (var k = 1; k <= need; k++) {
                            var cb = bytes[i + k];
                            if ((cb & 0xc0) !== 0x80) { ok = false; break; }
                            c = (c << 6) | (cb & 63);
                        }
                        if (!ok || (need === 1 && c < 0x80) || (need === 2 && (c < 0x800 || (c >= 0xd800 && c <= 0xdfff))) || (need === 3 && (c < 0x10000 || c > 0x10ffff))) {
                            bad(); i++; continue;
                        }
                        i += need + 1;
                        if (c === 0xfeff && !this.ignoreBOM && !this._bomSeen && out.length === 0 && chunk.length === 0) { this._bomSeen = true; continue; }
                        this._bomSeen = true;
                        if (c > 0xffff) { c -= 0x10000; chunk.push(0xd800 + (c >> 10), 0xdc00 + (c & 1023)); }
                        else chunk.push(c);
                        if (chunk.length > 8192) { out += String.fromCharCode.apply(null, chunk); chunk = []; }
                    }
                    if (!stream) this._bomSeen = false;
                    return out + String.fromCharCode.apply(null, chunk);
                };
                globalThis.TextDecoder = TextDecoder;
            }

            // WHATWG WebSocket over the native WinHTTP socket (runtime/src/websocket.rs). Events are
            // delivered on the JS thread by the runtime pump.
            if (typeof globalThis.WebSocket !== 'function' && typeof globalThis.__nsWebSocketOpen === 'function') {
                var WebSocket = function WebSocket(url, protocols) {
                    if (!(this instanceof WebSocket)) throw new TypeError("Failed to construct 'WebSocket': Please use the 'new' operator");
                    var href = String(url);
                    if (!/^wss?:\/\//i.test(href)) {
                        if (/^https?:\/\//i.test(href)) href = href.replace(/^http/i, 'ws');
                        else throw new SyntaxError("Failed to construct 'WebSocket': The URL '" + href + "' is invalid.");
                    }
                    var list = protocols === undefined ? [] : (Array.isArray(protocols) ? protocols : [protocols]).map(String);
                    this.url = href;
                    this.protocol = '';
                    this.extensions = '';
                    this.readyState = 0;
                    this.bufferedAmount = 0;
                    this.binaryType = 'arraybuffer';
                    this.onopen = null;
                    this.onmessage = null;
                    this.onerror = null;
                    this.onclose = null;
                    this._listeners = {};
                    var self = this;
                    this._id = globalThis.__nsWebSocketOpen(href, list, function (type, a, b, c) {
                        if (type === 'open') {
                            self.readyState = 1;
                            self.protocol = a || '';
                            self._emit({ type: 'open', target: self });
                        } else if (type === 'message') {
                            if (self.readyState === 1) self._emit({ type: 'message', data: a, origin: self.url, target: self });
                        } else if (type === 'error') {
                            self._emit({ type: 'error', message: a, target: self });
                        } else if (type === 'close') {
                            self.readyState = 3;
                            self._emit({ type: 'close', code: a, reason: b || '', wasClean: !!c, target: self });
                        }
                    });
                };
                WebSocket.CONNECTING = WebSocket.prototype.CONNECTING = 0;
                WebSocket.OPEN = WebSocket.prototype.OPEN = 1;
                WebSocket.CLOSING = WebSocket.prototype.CLOSING = 2;
                WebSocket.CLOSED = WebSocket.prototype.CLOSED = 3;
                WebSocket.prototype._emit = function (event) {
                    var handler = this['on' + event.type];
                    if (typeof handler === 'function') handler.call(this, event);
                    var list = (this._listeners[event.type] || []).slice();
                    for (var i = 0; i < list.length; i++) {
                        try { list[i].call(this, event); } catch (e) { setTimeout(function () { throw e; }, 0); }
                    }
                };
                WebSocket.prototype.addEventListener = function (type, listener) {
                    if (typeof listener !== 'function') return;
                    var list = this._listeners[type] || (this._listeners[type] = []);
                    if (list.indexOf(listener) < 0) list.push(listener);
                };
                WebSocket.prototype.removeEventListener = function (type, listener) {
                    var list = this._listeners[type];
                    if (list) { var i = list.indexOf(listener); if (i >= 0) list.splice(i, 1); }
                };
                WebSocket.prototype.dispatchEvent = function (event) { this._emit(event); return true; };
                WebSocket.prototype.send = function (data) {
                    if (this.readyState === 0) throw new Error("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.");
                    if (this.readyState !== 1) return;
                    globalThis.__nsWebSocketSend(this._id, data);
                };
                WebSocket.prototype.close = function (code, reason) {
                    if (this.readyState >= 2) return;
                    this.readyState = 2;
                    globalThis.__nsWebSocketClose(this._id, code === undefined ? 1000 : code, reason === undefined ? '' : String(reason));
                };
                globalThis.WebSocket = WebSocket;
            }

            var defaultTimeoutMs = 0;
            var statusEnum =
                (globalThis.Windows &&
                    globalThis.Windows.Foundation &&
                    globalThis.Windows.Foundation.AsyncStatus) ||
                { Started: 0, Completed: 1, Canceled: 2, Error: 3 };

            function normalizeTimeoutMs(options) {
                if (typeof options === 'number' && Number.isFinite(options) && options >= 0) {
                    return Math.floor(options);
                }

                if (options && typeof options === 'object') {
                    if (typeof options.timeoutMs === 'number' && Number.isFinite(options.timeoutMs) && options.timeoutMs >= 0) {
                        return Math.floor(options.timeoutMs);
                    }
                }

                return defaultTimeoutMs;
            }

            function normalizeStatus(status) {
                if (status == null) {
                    return Number.NaN;
                }
                if (typeof status === 'number') {
                    return status;
                }
                if (typeof status === 'string') {
                    var lower = status.toLowerCase();
                    if (lower === 'started') return 0;
                    if (lower === 'completed') return 1;
                    if (lower === 'canceled' || lower === 'cancelled') return 2;
                    if (lower === 'error') return 3;
                }
                if (typeof status.valueOf === 'function') {
                    var value = status.valueOf();
                    if (typeof value === 'number') {
                        return value;
                    }
                    if (typeof value === 'string') {
                        return normalizeStatus(value);
                    }
                }
                var coerced = Number(status);
                return Number.isNaN(coerced) ? Number.NaN : coerced;
            }

            function setDefaultTimeoutMs(timeoutMs) {
                if (typeof timeoutMs !== 'number' || !Number.isFinite(timeoutMs) || timeoutMs < 0) {
                    throw new Error('NSWinRT.setDefaultTimeoutMs(timeoutMs) expects a finite number >= 0');
                }
                defaultTimeoutMs = Math.floor(timeoutMs);
                return defaultTimeoutMs;
            }

            function wait(op, options) {
                if (typeof globalThis.__nsHostWaitForAsync === 'function') {
                    globalThis.__nsHostWaitForAsync(op, normalizeTimeoutMs(options));
                }
                return op;
            }

            function getStatus(op) {
                return normalizeStatus(op && op.Status);
            }

            function getResults(op) {
                if (op && typeof op.GetResults === 'function') {
                    return op.GetResults();
                }
                return undefined;
            }

            function toPromise(op, options) {
                if (op == null) {
                    return Promise.resolve(op);
                }

                if (op && typeof op.__handle === 'number' && op.__isTask === true
                    && typeof globalThis.__nsDotNetAwaitTask === 'function') {
                    return new Promise(function (resolve, reject) {
                        globalThis.__nsDotNetAwaitTask(op.__handle, resolve, reject);
                    });
                }

                if (typeof op.then === 'function' && !('Completed' in op)) {
                    return op;
                }

                return new Promise(function (resolve, reject) {
                    var settled = false;

                    function settleFromStatus(overrideStatus) {
                        if (settled) { return; }
                        try {
                            var status = normalizeStatus(
                                overrideStatus !== undefined ? overrideStatus : (op && op.Status)
                            );

                            if (status === statusEnum.Completed || status === 1) {
                                settled = true;
                                resolve(getResults(op));
                                return;
                            }
                            if (status === statusEnum.Canceled || status === 2) {
                                settled = true;
                                reject(new Error('WinRT async operation was canceled'));
                                return;
                            }
                            if (status === statusEnum.Error || status === 3) {
                                settled = true;
                                reject((op && op.ErrorCode) || new Error('WinRT async operation failed'));
                                return;
                            }
                            // status === 0 (Started): still running — do not settle yet.
                        } catch (err) {
                            settled = true;
                            reject(err);
                        }
                    }

                    try {
                        if (op && 'Completed' in op) {
                            // Settle immediately if the operation already finished before we
                            // could register the handler — WinRT will not fire Completed
                            // retroactively once the operation has left the Started state.
                            var initialStatus = normalizeStatus(op.Status);
                            if (!Number.isNaN(initialStatus) && initialStatus !== 0) {
                                settleFromStatus(initialStatus);
                                return;
                            }

                            op.Completed = function (asyncInfo, asyncStatus) {
                                settleFromStatus(asyncStatus);
                            };

                            // Re-check after assignment: the op may have completed in the
                            // narrow window between the status read and the handler assignment.
                            var raceStatus = normalizeStatus(op.Status);
                            if (!Number.isNaN(raceStatus) && raceStatus !== 0) {
                                settleFromStatus(raceStatus);
                            }
                            return;
                        }
                    } catch (_) {
                        // Completed setter not available; fall through to synchronous wait.
                    }

                    // Synchronous wait fallback — only safe when a finite timeout is given.
                    // Calling wait() with timeout=0 enters an infinite spin in the host.
                    var timeoutMs = normalizeTimeoutMs(options);
                    if (timeoutMs === 0) {
                        var currentStatus = normalizeStatus(op && op.Status);
                        if (!Number.isNaN(currentStatus) && currentStatus !== 0) {
                            settleFromStatus(currentStatus);
                        } else {
                            reject(new Error(
                                'Cannot await this WinRT async operation: it has no Completed property ' +
                                'and no timeoutMs was specified. Pass { timeoutMs: N } as the second argument.'
                            ));
                        }
                        return;
                    }

                    try {
                        wait(op, options);
                        settleFromStatus();
                    } catch (err) {
                        reject(err);
                    }
                });
            }

            function onCompleted(op, callback, options) {
                if (typeof callback !== 'function') {
                    throw new Error('NSWinRT.onCompleted(op, callback[, options]) expects callback to be a function');
                }

                try {
                    if (op && 'Completed' in op) {
                        op.Completed = function (asyncInfo, asyncStatus) {
                            callback(asyncInfo || op, normalizeStatus(asyncStatus));
                        };
                        return op;
                    }
                } catch (_) {
                    // Fall through to polling fallback.
                }

                Promise.resolve().then(function () {
                    wait(op, options);
                    callback(op, getStatus(op));
                });

                return op;
            }

            globalThis.__nsWinRTToPromise = toPromise;
            globalThis.NSWinRT = globalThis.NSWinRT || {};
            globalThis.NSWinRT.toPromise = toPromise;
            globalThis.NSWinRT.wait = wait;
            globalThis.NSWinRT.getStatus = getStatus;
            globalThis.NSWinRT.getResults = getResults;
            globalThis.NSWinRT.onCompleted = onCompleted;
            globalThis.NSWinRT.setDefaultTimeoutMs = setDefaultTimeoutMs;

            function Pointer(handle) {
                this.handle = handle == null ? null : handle;
            }

            Pointer.prototype.isNull = function () {
                return this.handle == null;
            };


            Pointer.prototype.unwrap = function () {
                return this.handle;
            };

            Pointer.prototype.toString = function () {
                return this.isNull() ? '[Pointer null]' : '[Pointer external]';
            };

            function asPointer(value) {
                return value instanceof Pointer ? value : new Pointer(value);
            }

            function handleOf(value) {
                return value instanceof Pointer ? value.handle : value;
            }

            function asBufferSource(value) {
                if (value == null) {
                    return value;
                }

                if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
                    return value;
                }

                throw new Error('NSWinRT.interop.asBufferSource(value) expects ArrayBuffer or ArrayBufferView');
            }

            function asUint8View(value) {
                var source = asBufferSource(value);
                if (source == null) {
                    return new Uint8Array(0);
                }

                if (source instanceof ArrayBuffer) {
                    return new Uint8Array(source);
                }

                return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
            }

            function asDataView(value) {
                var source = asBufferSource(value);
                if (source == null) {
                    return new DataView(new ArrayBuffer(0));
                }

                if (source instanceof ArrayBuffer) {
                    return new DataView(source);
                }

                return new DataView(source.buffer, source.byteOffset, source.byteLength);
            }

            // WinRT DateTime stores 100ns ticks since 1601-01-01T00:00:00Z.
            var winRtUnixEpochOffsetTicks = 116444736000000000n;

            function toWinRTDateTimeTicks(input) {
                var ms;
                if (input instanceof Date) {
                    ms = input.getTime();
                } else if (typeof input === 'number') {
                    ms = input;
                } else {
                    throw new Error('NSWinRT.interop.toWinRTDateTimeTicks expects Date or millisecond timestamp');
                }

                if (!Number.isFinite(ms)) {
                    throw new Error('Invalid Date/time value for WinRT conversion');
                }

                return BigInt(Math.trunc(ms)) * 10000n + winRtUnixEpochOffsetTicks;
            }

            function fromWinRTDateTimeTicks(value) {
                if (value == null) {
                    return new Date(Number.NaN);
                }

                var ticks = typeof value === 'bigint' ? value : BigInt(Math.trunc(Number(value)));
                var unixTicks = ticks - winRtUnixEpochOffsetTicks;
                var ms = Number(unixTicks / 10000n);
                return new Date(ms);
            }

            var pointerBufferRegistry = new Map();

            function pointerKey(value) {
                if (typeof globalThis.__nsPointerKey !== 'function') {
                    return null;
                }
                return globalThis.__nsPointerKey(value);
            }

            function pointerFromBuffer(value) {
                var source = asBufferSource(value);
                if (source == null || typeof globalThis.__nsBufferToPointer !== 'function') {
                    return null;
                }
                return globalThis.__nsBufferToPointer(source);
            }

            function trackBufferSource(value) {
                var source = asBufferSource(value);
                if (source == null) {
                    return null;
                }

                var pointer = pointerFromBuffer(source);
                var key = pointerKey(pointer);
                if (key != null) {
                    pointerBufferRegistry.set(String(key), source);
                }
                return pointer;
            }

            function resolveTrackedBuffer(pointerLike) {
                var key = pointerKey(pointerLike);
                if (key == null) {
                    return undefined;
                }
                return pointerBufferRegistry.get(String(key));
            }

            var outParamMarker = '__nswinrt_out_param__';

            function looksLikeType(value) {
                return typeof value === 'string' || typeof value === 'function';
            }

            function typeNameOf(value) {
                if (typeof value === 'string') {
                    return value;
                }
                if (typeof value === 'function') {
                    return value.__typeName__ || value.name || '';
                }
                return '';
            }

            function OutParam(typeOrValue, initialValue, hasInitialValue) {
                var typeName = looksLikeType(typeOrValue) ? typeNameOf(typeOrValue) : '';
                Object.defineProperty(this, outParamMarker, {
                    value: true,
                    enumerable: false,
                    configurable: false
                });
                Object.defineProperty(this, 'type', {
                    value: typeName,
                    enumerable: true,
                    configurable: true,
                    writable: true
                });
                this.value = hasInitialValue ? initialValue : undefined;
            }

            OutParam.prototype.out = function (value) {
                return out(this.type || undefined, arguments.length > 0 ? value : this.value);
            };

            function out(typeOrValue, initialValue) {
                if (arguments.length > 1) {
                    return new OutParam(typeOrValue, initialValue, true);
                }
                if (looksLikeType(typeOrValue)) {
                    return new OutParam(typeOrValue, undefined, false);
                }
                return new OutParam(undefined, typeOrValue, arguments.length > 0);
            }

            function isOut(value) {
                return !!(value && typeof value === 'object' && value[outParamMarker] === true);
            }

            globalThis.NSWinRT.interop = {
                Pointer: Pointer,
                OutParam: OutParam,
                pointer: asPointer,
                out: out,
                isOut: isOut,
                isPointer: function (value) {
                    return value instanceof Pointer;
                },
                handleOf: handleOf,
                asBufferSource: asBufferSource,
                asUint8View: asUint8View,
                asDataView: asDataView,
                toWinRTDateTimeTicks: toWinRTDateTimeTicks,
                fromWinRTDateTimeTicks: fromWinRTDateTimeTicks,
                pointerKey: pointerKey,
                pointerFromBuffer: pointerFromBuffer,
                // arrayBufferFromBuffer(buffer) — zero-copy view over a
                // Windows.Storage.Streams.IBuffer. The returned ArrayBuffer aliases
                // the buffer's native storage (covers IBuffer.Length valid bytes) and
                // keeps the IBuffer alive until it is collected. Use `.byteLength` on
                // the result, or wrap in a Uint8Array/DataView to read or write through.
                arrayBufferFromBuffer: function (buffer) {
                    if (buffer == null) {
                        return null;
                    }
                    if (typeof globalThis.__nsArrayBufferFromBuffer !== 'function') {
                        throw new Error('NSWinRT.interop.arrayBufferFromBuffer is unavailable in this runtime');
                    }
                    return globalThis.__nsArrayBufferFromBuffer(buffer);
                },
                trackBufferSource: trackBufferSource,
                resolveTrackedBuffer: resolveTrackedBuffer,
                byteLengthOf: function (value) {
                    var buffer = asBufferSource(value);
                    if (buffer == null) {
                        return 0;
                    }
                    return typeof buffer.byteLength === 'number' ? buffer.byteLength : 0;
                },
                byteOffsetOf: function (value) {
                    if (ArrayBuffer.isView(value)) {
                        return value.byteOffset;
                    }
                    return 0;
                },
                readU8: function (value, offset) {
                    return asDataView(value).getUint8(offset >>> 0);
                },
                writeU8: function (value, offset, input) {
                    asDataView(value).setUint8(offset >>> 0, input >>> 0);
                    return value;
                },
                readI32: function (value, offset, littleEndian) {
                    return asDataView(value).getInt32(offset >>> 0, littleEndian !== false);
                },
                writeI32: function (value, offset, input, littleEndian) {
                    asDataView(value).setInt32(offset >>> 0, input | 0, littleEndian !== false);
                    return value;
                },
                readF32: function (value, offset, littleEndian) {
                    return asDataView(value).getFloat32(offset >>> 0, littleEndian !== false);
                },
                writeF32: function (value, offset, input, littleEndian) {
                    asDataView(value).setFloat32(offset >>> 0, +input, littleEndian !== false);
                    return value;
                },
                readF64: function (value, offset, littleEndian) {
                    return asDataView(value).getFloat64(offset >>> 0, littleEndian !== false);
                },
                writeF64: function (value, offset, input, littleEndian) {
                    asDataView(value).setFloat64(offset >>> 0, +input, littleEndian !== false);
                    return value;
                },
                // reference(typeName, value) — explicit IReference<T> boxing.
                // Normally the runtime boxes automatically from the method signature;
                // use this for advanced cases. Accepts short and fully-qualified names:
                // "Double" | "Int32" | "TimeSpan" | "Windows.Foundation.DateTime" | etc.
                reference: function (typeName, value) {
                    if (typeof globalThis.__nsCreateReference !== 'function') { return null; }
                    return globalThis.__nsCreateReference(typeName, value);
                },
                // Typed concrete-value helpers
                // These create a concrete typed IPropertyValue (not a nullable IReference)
                // so the WinRT runtime can pick the correct overload when a parameter is
                // typed as Object/IInspectable.  Pass the returned object directly to
                // any WinRT method that would otherwise receive an untyped JS number.
                float: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Single', +n); },
                double: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Double', +n); },
                int: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Int32', +n); },
                uint: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('UInt32', +n); },
                long: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Int64', +n); },
                ulong: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('UInt64', +n); },
                short: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Int16', +n); },
                ushort: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('UInt16', +n); },
                byte: function (n) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('UInt8', +n); },
                char: function (c) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Char16', c); },
                bool: function (v) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Boolean', !!v); },
                // Date/time helpers
                timeSpan: function (ms) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('TimeSpan', +ms); },
                dateTime: function (msOrDate) {
                    var ms = (msOrDate instanceof Date) ? msOrDate.getTime() : +msOrDate;
                    return globalThis.__nsTypedValue && globalThis.__nsTypedValue('DateTime', ms);
                },
                guid: function (str) { return globalThis.__nsTypedValue && globalThis.__nsTypedValue('Guid', String(str)); },
            };

            // Expose top-level `interop` alias.
            // This lets code reference `globalThis.interop` while keeping backwards
            // compatibility with `globalThis.NSWinRT`.
            globalThis.interop = globalThis.interop || globalThis.NSWinRT.interop;
            // Ensure NSWinRT.interop is populated when `interop` was already present.
            globalThis.NSWinRT = globalThis.NSWinRT || {};
            globalThis.NSWinRT.interop = globalThis.NSWinRT.interop || globalThis.interop;

            var proxyExtensions = [];
            var proxyInstances = new Map();
            var nextProxyId = 1;

            function ctorName(ctor) {
                return (ctor && (ctor.__winrtProxyName__ || ctor.__typeName__ || ctor.name)) || 'Object';
            }

            // This IIFE runs before the NSWinRT.dotnet block below in script order, but by the
            // time createManagedSubclass is actually CALLED (well after the whole bootstrap has
            // finished evaluating) NSWinRT.dotnet.wrap already exists whenever
            // __nsDotNetCreateJsSubclass does (both are registered together) — wraps a raw tagged
            // handle value into a live DotNet instance proxy exactly like the dotnet block's own
            // private `_wrap`, without needing access to that other IIFE's closure.
            function _wrapDotNetHandle(value) {
                var dotnet = globalThis.NSWinRT && globalThis.NSWinRT.dotnet;
                return (dotnet && typeof dotnet.wrap === 'function') ? dotnet.wrap(value) : value;
            }

            var typeDescriptorCache = Object.create(null);

            function describeWinRTType(typeName) {
                if (!typeName || typeof globalThis.__nsDescribeWinRTType !== 'function') {
                    return null;
                }
                if (Object.prototype.hasOwnProperty.call(typeDescriptorCache, typeName)) {
                    return typeDescriptorCache[typeName];
                }
                try {
                    var raw = globalThis.__nsDescribeWinRTType(typeName);
                    typeDescriptorCache[typeName] = raw ? JSON.parse(raw) : null;
                } catch (_) {
                    typeDescriptorCache[typeName] = null;
                }
                return typeDescriptorCache[typeName];
            }

            function buildFallbackParameterMetadata(fn) {
                var params = [];
                var count = typeof fn === 'function' && Number.isFinite(fn.length) ? fn.length : 0;
                for (var i = 0; i < count; i++) {
                    params.push({ name: 'arg' + i, type: 'Object' });
                }
                return params;
            }

            function normalizeMethodMetadata(name, value, descriptors) {
                for (var i = 0; i < descriptors.length; i++) {
                    var descriptor = descriptors[i];
                    if (!descriptor || !Array.isArray(descriptor.methods)) {
                        continue;
                    }
                    for (var j = 0; j < descriptor.methods.length; j++) {
                        var method = descriptor.methods[j];
                        if (method && method.name === name) {
                            return {
                                name: method.name,
                                returnType: method.returnType || method.return_type || 'Void',
                                parameters: Array.isArray(method.parameters) ? method.parameters : [],
                            };
                        }
                    }
                }

                return {
                    name: name,
                    returnType: name === 'init' ? 'Void' : 'Object',
                    parameters: buildFallbackParameterMetadata(value),
                };
            }

            function normalizePropertyMetadata(name, descriptors) {
                for (var i = 0; i < descriptors.length; i++) {
                    var descriptor = descriptors[i];
                    if (!descriptor || !Array.isArray(descriptor.properties)) {
                        continue;
                    }
                    for (var j = 0; j < descriptor.properties.length; j++) {
                        var property = descriptor.properties[j];
                        if (property && property.name === name) {
                            return {
                                name: property.name,
                                propType: property.propType || property.prop_type || 'Object',
                                readable: property.readable !== false,
                                writable: property.writable !== false,
                            };
                        }
                    }
                }

                return {
                    name: name,
                    propType: 'Object',
                    readable: true,
                    writable: true,
                };
            }

            function collectProxyMethods(overrides, descriptors) {
                var methods = [];
                for (var key in overrides) {
                    if (!Object.prototype.hasOwnProperty.call(overrides, key)) {
                        continue;
                    }
                    if (key === 'interfaces') {
                        continue;
                    }
                    if (typeof overrides[key] === 'function') {
                        methods.push(normalizeMethodMetadata(key, overrides[key], descriptors));
                    }
                }
                methods.sort(function (left, right) {
                    return String(left && left.name || '').localeCompare(String(right && right.name || ''));
                });
                return methods;
            }

            function collectProxyProperties(overrides, descriptors) {
                var props = [];
                for (var key in overrides) {
                    if (!Object.prototype.hasOwnProperty.call(overrides, key)) {
                        continue;
                    }
                    if (key === 'interfaces') {
                        continue;
                    }
                    if (typeof overrides[key] !== 'function') {
                        props.push(normalizePropertyMetadata(key, descriptors));
                    }
                }
                props.sort(function (left, right) {
                    return String(left && left.name || '').localeCompare(String(right && right.name || ''));
                });
                return props;
            }

            function safeIdentifier(name) {
                return String(name || '')
                    .replace(/[^A-Za-z0-9_]/g, '_')
                    .replace(/^([^A-Za-z_])/, '_$1') || 'ProxyType';
            }

            function autoProxyTypeName(baseCtor) {
                var baseType = ctorName(baseCtor) || 'Object';
                var baseShort = safeIdentifier(baseType.split('.').pop());
                return 'NativeScript.Gen.' + baseShort + '_' + (proxyExtensions.length + 1);
            }

            function renderProxyCSharp(meta) {
                var typeName = meta.typeName || ('GeneratedProxy' + (proxyExtensions.length + 1));
                var safeTypeName = safeIdentifier(typeName.split('.').pop());
                var baseType = meta.baseType || 'object';
                var methodStubs = '';
                for (var i = 0; i < meta.methods.length; i++) {
                    var methodMeta = meta.methods[i];
                    var methodName = safeIdentifier((methodMeta && methodMeta.name) || methodMeta);
                    methodStubs +=
                        '    public object __ns_' + methodName + '(params object[] args)\\n' +
                        '    {\\n' +
                        '        return ProxyDispatcher.Invoke(this.__proxyId, "' + methodName + '", args);\\n' +
                        '    }\\n\\n';
                }

                return (
                    'using System;\\n\\n' +
                    'namespace NativeScriptGeneratedProxies\\n' +
                    '{\\n' +
                    '    public static class ProxyDispatcher\\n' +
                    '    {\\n' +
                    '        public static Func<int, string, object[], object> JsInvoke;\\n' +
                    '        public static object Invoke(int id, string method, object[] args)\\n' +
                    '        {\\n' +
                    '            var cb = JsInvoke;\\n' +
                    '            if (cb == null) throw new InvalidOperationException("JsInvoke callback is not registered.");\\n' +
                    '            return cb(id, method, args);\\n' +
                    '        }\\n' +
                    '    }\\n\\n' +
                    '    public class ' + safeTypeName + ' : ' + baseType + '\\n' +
                    '    {\\n' +
                    '        private readonly int __proxyId;\\n\\n' +
                    '        public ' + safeTypeName + '(int proxyId)\\n' +
                    '        {\\n' +
                    '            this.__proxyId = proxyId;\\n' +
                    '        }\\n\\n' +
                    methodStubs +
                    '    }\\n' +
                    '}\\n'
                );
            }

            function renderProxyCsproj(meta) {
                var asmName = safeIdentifier((meta.typeName || 'GeneratedProxy').split('.').pop());
                return (
                    '<Project Sdk="Microsoft.NET.Sdk">\\n' +
                    '  <PropertyGroup>\\n' +
                    '    <TargetFramework>net8.0-windows10.0.19041.0</TargetFramework>\\n' +
                    '    <AssemblyName>' + asmName + '</AssemblyName>\\n' +
                    '    <RootNamespace>NativeScriptGeneratedProxies</RootNamespace>\\n' +
                    '    <ImplicitUsings>enable</ImplicitUsings>\\n' +
                    '    <Nullable>disable</Nullable>\\n' +
                    '    <LangVersion>latest</LangVersion>\\n' +
                    '  </PropertyGroup>\\n' +
                    '</Project>\\n'
                );
            }

            function buildProxyMetadata(baseCtor, typeName, overrides, Extended) {
                var baseType = ctorName(baseCtor);
                var interfaceNames = Array.isArray(overrides.interfaces)
                    ? overrides.interfaces.map(function (iface) { return ctorName(iface); })
                    : [];
                var descriptors = [describeWinRTType(baseType)];
                for (var i = 0; i < interfaceNames.length; i++) {
                    descriptors.push(describeWinRTType(interfaceNames[i]));
                }
                var namespace = '';
                var className = typeName || '';
                if (typeName) {
                    var splitIndex = typeName.lastIndexOf('.');
                    if (splitIndex >= 0) {
                        namespace = typeName.slice(0, splitIndex);
                        className = typeName.slice(splitIndex + 1);
                    }
                }
                var meta = {
                    kind: 'windows-proxy',
                    typeName: typeName || '',
                    className: className || safeIdentifier((typeName || baseType || 'GeneratedProxy').split('.').pop()),
                    namespace: namespace || null,
                    baseType: baseType,
                    baseClass: baseType,
                    interfaces: interfaceNames,
                    methods: collectProxyMethods(overrides, descriptors.filter(Boolean)),
                    properties: collectProxyProperties(overrides, descriptors.filter(Boolean)),
                    isAutoGeneratedName: !typeName,
                    registeredAt: new Date().toISOString(),
                    registered: false,
                    generated: null,
                };
                try {
                    Object.defineProperty(Extended, '__proxyMetadata__', {
                        value: meta,
                        writable: true,
                        configurable: true,
                        enumerable: false,
                    });
                } catch (_) {
                    Extended.__proxyMetadata__ = meta;
                }
                proxyExtensions.push(meta);
                if (typeof globalThis.__nsProxyAutoCapture === 'function') {
                    try {
                        globalThis.__nsProxyAutoCapture(JSON.stringify(proxyExtensions));
                    } catch (_) {
                        // Capture is best-effort; runtime behavior should remain unaffected.
                    }
                }
               
                if (meta.interfaces.length > 0) {
                    if (typeof globalThis.__nsProxyCompileProject === 'function') {
                        try { registerProxy(meta); } catch (_) { }
                    } else if (typeof globalThis.__nsProxyWriteTextFile === 'function') {
                        try { emitProxy(meta); } catch (_) { }
                    }
                }
                return meta;
            }

            function ensureProxyInstance(instance, overrides, ctor) {
                if (!instance || typeof instance !== 'object') {
                    return -1;
                }

                var proxyId = instance.__proxyId;
                if (typeof proxyId !== 'number' || !Number.isFinite(proxyId)) {
                    proxyId = nextProxyId++;
                    try {
                        Object.defineProperty(instance, '__proxyId', {
                            value: proxyId,
                            writable: false,
                            configurable: true,
                            enumerable: false,
                        });
                    } catch (_) {
                        instance.__proxyId = proxyId;
                    }
                }

                proxyInstances.set(proxyId, {
                    instance: instance,
                    overrides: overrides,
                    constructor: ctor,
                });

                return proxyId;
            }

            function makeExtendedConstructor(baseCtor, nameOrOverrides, maybeOverrides) {
                var hasName = typeof nameOrOverrides === 'string';
                var explicitTypeName = hasName ? nameOrOverrides : '';
                var typeName = explicitTypeName || autoProxyTypeName(baseCtor);
                var overrides = hasName ? maybeOverrides : nameOrOverrides;
                if (!overrides || typeof overrides !== 'object') {
                    overrides = {};
                }

                function Extended() {
                    var instance;
                    var args = Array.prototype.slice.call(arguments);
                    try {
                        instance = Reflect.construct(baseCtor, args);
                    } catch (_) {
                        instance = {};
                    }

                    for (var key in overrides) {
                        if (key === 'interfaces') {
                            continue;
                        }
                        var value = overrides[key];
                        if (typeof value === 'function') {
                            try {
                                Object.defineProperty(instance, key, {
                                    value: value,
                                    writable: true,
                                    configurable: true,
                                    enumerable: true,
                                });
                            } catch (_) {
                                instance[key] = value;
                            }
                        } else {
                            instance[key] = value;
                        }
                    }

                    if (typeof overrides.init === 'function') {
                        try {
                            var initResult = overrides.init.apply(instance, args);
                            if (initResult && typeof initResult === 'object') {
                                instance = initResult;
                            }
                        } catch (_) {
                            // Keep constructor resilient; init errors should not crash runtime.
                        }
                    }

                    if (Array.isArray(overrides.interfaces)) {
                        try {
                            Object.defineProperty(instance, '__interfaces__', {
                                value: overrides.interfaces.slice(),
                                writable: false,
                                configurable: true,
                                enumerable: false,
                            });
                        } catch (_) {
                            instance.__interfaces__ = overrides.interfaces.slice();
                        }
                    }

                    ensureProxyInstance(instance, overrides, Extended);

                    return instance;
                }

                Extended.prototype = Object.create((baseCtor && baseCtor.prototype) || Object.prototype);
                Extended.prototype.constructor = Extended;

                for (var protoKey in overrides) {
                    if (protoKey === 'interfaces') {
                        continue;
                    }
                    Extended.prototype[protoKey] = overrides[protoKey];
                }

                if (typeName) {
                    try {
                        Object.defineProperty(Extended, 'name', {
                            value: typeName,
                            configurable: true,
                        });
                    } catch (_) {
                        // Non-critical metadata assignment.
                    }
                }

                Extended.__typeName__ = typeName || ctorName(baseCtor);
                Extended.__nsWinRTClass__ = true;
                var metadata = buildProxyMetadata(baseCtor, typeName, overrides, Extended);

                Extended.extend = function (nextNameOrOverrides, nextMaybeOverrides) {
                    return makeExtendedConstructor(Extended, nextNameOrOverrides, nextMaybeOverrides);
                };

                try {
                    Object.defineProperty(Extended, 'emitProxy', {
                        value: function (outDir) {
                            return NSWinRT.proxy.emit(metadata, outDir);
                        },
                        writable: true,
                        configurable: true,
                        enumerable: false,
                    });
                } catch (_) {
                    Extended.emitProxy = function (outDir) {
                        return NSWinRT.proxy.emit(metadata, outDir);
                    };
                }

                return Extended;
            }

            if (typeof Function.prototype.extend !== 'function') {
                Object.defineProperty(Function.prototype, 'extend', {
                    value: function (nameOrOverrides, maybeOverrides) {
                        return makeManagedConstructor(this, nameOrOverrides, maybeOverrides);
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

            if (typeof Object.extend !== 'function') {
                Object.defineProperty(Object, 'extend', {
                    value: function (nameOrOverrides, maybeOverrides) {
                        return makeManagedConstructor(Object, nameOrOverrides, maybeOverrides);
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

            // Create a managed-backed subclass factory. This returns a constructor
            // that, when invoked, asks the managed bridge to instantiate a real
            // C# subclass whose vtable forwards virtual calls back into the
            // provided JS `overrides` object via the DotNet bridge.
            function makeManagedConstructor(baseCtor, nameOrOverrides, maybeOverrides) {
                var hasName = typeof nameOrOverrides === 'string';
                var explicitTypeName = hasName ? nameOrOverrides : '';
                var typeName = explicitTypeName || autoProxyTypeName(baseCtor);
                var overrides = hasName ? maybeOverrides : nameOrOverrides;
                if (!overrides || typeof overrides !== 'object') overrides = {};

                function Managed() {
                    var args = Array.prototype.slice.call(arguments);
                    // Pass the actual base ctor type name as the assembly param so the
                    // bridge can fall back to it when no static proxy exists for typeName.
                    var baseTypeName = ctorName(baseCtor) || '';
                    var obj = NSWinRT.proxy.createManagedSubclass(baseTypeName, typeName || '', overrides);
                    if (typeof overrides.init === 'function') {
                        try {
                            var initResult = overrides.init.apply(obj, args);
                            if (initResult && typeof initResult === 'object') obj = initResult;
                        } catch (_) { }
                    }
                    return obj;
                }

                Managed.prototype = Object.create((baseCtor && baseCtor.prototype) || Object.prototype);
                Managed.prototype.constructor = Managed;
                Managed.extend = function (nextNameOrOverrides, nextMaybeOverrides) {
                    return makeManagedConstructor(Managed, nextNameOrOverrides, nextMaybeOverrides);
                };
                try {
                    Object.defineProperty(Managed, 'name', { value: typeName || ctorName(baseCtor), configurable: true });
                } catch (_) { }
                Managed.__typeName__ = typeName || ctorName(baseCtor);
                Managed.__nsWinRTClass__ = true;
                var meta = buildProxyMetadata(baseCtor, typeName, overrides, Managed);
                try {
                    Object.defineProperty(Managed, 'emitProxy', {
                        value: function (outDir) { return NSWinRT.proxy.emit(meta, outDir); },
                        writable: true,
                        configurable: true,
                        enumerable: false,
                    });
                } catch (_) {
                    Managed.emitProxy = function (outDir) { return NSWinRT.proxy.emit(meta, outDir); };
                }
                return Managed;
            }

            if (typeof Function.prototype.extendManaged !== 'function') {
                Object.defineProperty(Function.prototype, 'extendManaged', {
                    value: function (nameOrOverrides, maybeOverrides) {
                        return makeManagedConstructor(this, nameOrOverrides, maybeOverrides);
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

            if (typeof Object.extendManaged !== 'function') {
                Object.defineProperty(Object, 'extendManaged', {
                    value: function (nameOrOverrides, maybeOverrides) {
                        return makeManagedConstructor(Object, nameOrOverrides, maybeOverrides);
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

            // Convenience alias for hosts that expect a global BaseClass helper.
            globalThis.BaseClass = globalThis.BaseClass || {};
            if (typeof globalThis.BaseClass.extend !== 'function') {
                globalThis.BaseClass.extend = function (baseCtor, nameOrOverrides, maybeOverrides) {
                    return makeManagedConstructor(baseCtor || Object, nameOrOverrides, maybeOverrides);
                };
            }

            // Registry mapping each WinRT-extended prototype to its lazy factory.
            // Keyed by the new Child.prototype created in __extends_winrt so that
            // the Parent.apply override can look up the right factory per subclass.
            var __nsExtendRegistry__ = new WeakMap();

            function __extends_winrt(Child, Parent) {
                var extendStatics = Object.setPrototypeOf || function (d, b) { d.__proto__ = b; };
                extendStatics(Child, Parent);

                // Lazy factory: deferred until first `new Child()` so that TypeScript
                // has had a chance to populate Child.prototype with override methods.
                var _extended = null;
                var getExtended = function () {
                    if (!_extended) {
                        var overrides = {};
                        var interfaces = [];
                        var proto = Child.prototype;
                        if (proto) {
                            Object.getOwnPropertyNames(proto).forEach(function (key) {
                                if (key === 'constructor') return;
                                if (key === 'interfaces') {
                                    var iv = proto[key];
                                    if (Array.isArray(iv)) interfaces = iv;
                                    return;
                                }
                                overrides[key] = proto[key];
                            });
                        }
                        if (interfaces.length) overrides.interfaces = interfaces;
                        var name = Child.__winrtProxyName__ || Child.name || '';
                        var hasBridge = typeof globalThis.__nsDotNetCreateJsSubclass === 'function';
                        if (Child.__winrtProxyName__ || (interfaces.length > 0 && hasBridge)) {
                            _extended = makeManagedConstructor(Parent, name, overrides);
                        } else {
                            // No bridge or no interfaces: pure JS proxy via Reflect.construct.
                            _extended = makeExtendedConstructor(Parent, name, overrides);
                        }
                    }
                    return _extended;
                };

                var newProto = Object.create((Parent && Parent.prototype) || Object.prototype);
                newProto.constructor = Child;
                Child.prototype = newProto;
                __nsExtendRegistry__.set(newProto, getExtended);

                // Patch Parent.apply/call once per parent class.  The WeakMap lookup
                // inside the override picks the correct factory per subclass so that
                // multiple classes extending the same parent coexist correctly.
                if (!Parent.__nsApplyPatched__) {
                    Parent.__nsApplyPatched__ = true;

                    Parent.apply = function (thiz, args) {
                        if (thiz && thiz.__nsContainer__) return;
                        var factory = __nsExtendRegistry__.get(Object.getPrototypeOf(thiz));
                        if (!factory) return;
                        thiz.__nsContainer__ = true;
                        try {
                            var Extended = factory();
                            var argArr = Array.isArray(args) ? args : (args ? Array.prototype.slice.call(args) : []);
                            if (argArr.length > 0) {
                                thiz.__proto__ = new (Function.prototype.bind.apply(Extended, [null].concat(argArr)))();
                            } else {
                                thiz.__proto__ = new Extended();
                            }
                            return thiz.__proto__;
                        } finally {
                            delete thiz.__nsContainer__;
                        }
                    };

                    Parent.call = function (thiz) {
                        return Parent.apply(thiz, Array.prototype.slice.call(arguments, 1));
                    };
                }
            }

            // Global __extends that TypeScript compiled output resolves via
            // `var __extends = (this && this.__extends) || ...`.
            // Intercepts WinRT parent classes; falls back to standard TS behaviour.
            if (typeof globalThis.__extends !== 'function') {
                var __extends_ts_inner = function (d, b) {
                    if (b !== null && typeof b !== 'function') {
                        throw new TypeError('Class extends value ' + String(b) + ' is not a constructor or null');
                    }
                    var extendStatics = Object.setPrototypeOf || function (d, b) { d.__proto__ = b; };
                    extendStatics(d, b);
                    function __() { this.constructor = d; }
                    d.prototype = b === null ? Object.create(b) : (__.prototype = b.prototype, new __());
                };

                globalThis.__extends = function (d, b) {
                    if (b && b.__nsWinRTClass__) {
                        __extends_winrt(d, b);
                    } else {
                        __extends_ts_inner(d, b);
                    }
                };
            }

            // TypeScript decorator helpers, as the iOS/Android runtimes provide them: compiled
            // (`experimentalDecorators`) code calls a bare `__decorate(...)`. Core's globals module
            // binds tslib's copies too, but an ES-module bundle can evaluate decorated core modules
            // before it runs.
            if (typeof globalThis.__decorate !== 'function') {
                globalThis.__decorate = function (decorators, target, key, desc) {
                    var c = arguments.length;
                    var r = c < 3 ? target : desc === null ? (desc = Object.getOwnPropertyDescriptor(target, key)) : desc, d;
                    if (typeof Reflect === 'object' && typeof Reflect.decorate === 'function') {
                        r = Reflect.decorate(decorators, target, key, desc);
                    } else {
                        for (var i = decorators.length - 1; i >= 0; i--) {
                            if ((d = decorators[i])) r = (c < 3 ? d(r) : c > 3 ? d(target, key, r) : d(target, key)) || r;
                        }
                    }
                    return c > 3 && r && Object.defineProperty(target, key, r), r;
                };
            }
            if (typeof globalThis.__param !== 'function') {
                globalThis.__param = function (paramIndex, decorator) {
                    return function (target, key) { decorator(target, key, paramIndex); };
                };
            }
            if (typeof globalThis.__metadata !== 'function') {
                globalThis.__metadata = function (metadataKey, metadataValue) {
                    if (typeof Reflect === 'object' && typeof Reflect.metadata === 'function') {
                        return Reflect.metadata(metadataKey, metadataValue);
                    }
                };
            }

            // @Interfaces([IFoo, IBar]) — attach WinRT interface list to a class.
            if (typeof globalThis.Interfaces !== 'function') {
                Object.defineProperty(globalThis, 'Interfaces', {
                    value: function Interfaces(interfacesList) {
                        return function (target) {
                            target.prototype.interfaces = Array.isArray(interfacesList) ? interfacesList.slice() : [];
                            return target;
                        };
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

         
            if (typeof globalThis.CSharpProxy !== 'function') {
                Object.defineProperty(globalThis, 'CSharpProxy', {
                    value: function CSharpProxy(name) {
                        return function (target) {
                            target.__winrtProxyName__ = typeof name === 'string' ? name : '';
                            return target;
                        };
                    },
                    writable: true,
                    configurable: true,
                    enumerable: false,
                });
            }

            function defaultProxyOutDir(meta) {
                var typeName = (meta && meta.typeName) ? meta.typeName : 'GeneratedProxy';
                var safe = safeIdentifier(typeName.split('.').pop());
                return './generated-proxies/' + safe;
            }

            function emitProxy(meta, outDir) {
                if (!meta || typeof meta !== 'object') {
                    throw new Error('NSWinRT.proxy.emit(meta[, outDir]) expects a proxy metadata object');
                }
                if (typeof globalThis.__nsProxyWriteTextFile !== 'function') {
                    throw new Error('Host proxy file emitter is not available');
                }

                var dir = outDir || defaultProxyOutDir(meta);
                var csprojPath = dir + '/Proxy.csproj';
                var csPath = dir + '/Proxy.g.cs';
                var csproj = renderProxyCsproj(meta);
                var source = renderProxyCSharp(meta);

                globalThis.__nsProxyWriteTextFile(csprojPath, csproj);
                globalThis.__nsProxyWriteTextFile(csPath, source);

                meta.generated = {
                    dir: dir,
                    csprojPath: csprojPath,
                    csPath: csPath,
                };

                return meta.generated;
            }

            function compileProxy(meta, outDir, configuration) {
                if (typeof globalThis.__nsProxyCompileProject !== 'function') {
                    throw new Error('Host proxy compiler is not available');
                }
                var generated = emitProxy(meta, outDir);
                var result = globalThis.__nsProxyCompileProject(generated.csprojPath, configuration || 'Debug');
                generated.build = result;
                return generated;
            }

            function registerProxy(meta, outDir, configuration) {
                var generated = compileProxy(meta, outDir, configuration);
                var manifest = {
                    kind: 'windows-proxy',
                    typeName: meta.typeName,
                    baseType: meta.baseType,
                    interfaces: meta.interfaces,
                    methods: meta.methods,
                    properties: meta.properties,
                    generated: generated,
                    registration: {
                        hostCanLoadAssemblies: false,
                        note: 'Assembly build succeeded, but runtime CLR proxy activation is not wired yet. Dynamic JS fallback remains active.',
                    },
                };
                if (typeof globalThis.__nsProxyRegisterManifest === 'function') {
                    globalThis.__nsProxyRegisterManifest(JSON.stringify(manifest));
                }
                meta.registered = true;
                meta.registration = manifest.registration;
                return manifest;
            }

            function invokeProxyById(proxyId, methodName, argsArray) {
                var entry = proxyInstances.get(proxyId);
                if (!entry) {
                    throw new Error('Proxy instance not found for id ' + proxyId);
                }

                var target = entry.instance;
                var method = target && target[methodName];
                if (typeof method !== 'function') {
                    throw new Error('Proxy method "' + methodName + '" is not defined on proxy id ' + proxyId);
                }

                return method.apply(target, Array.isArray(argsArray) ? argsArray : []);
            }

            globalThis.__nsInvokeProxyJs = invokeProxyById;

            globalThis.NSWinRT.proxy = {
                getExtensions: function () {
                    return proxyExtensions.slice();
                },
                emit: emitProxy,
                compile: compileProxy,
                register: registerProxy,
                createManagedSubclass: function (assembly, typeName, overrides) {
                    if (typeof globalThis.__nsDotNetCreateJsSubclass !== 'function') {
                        throw new Error('__nsDotNetCreateJsSubclass is not available in this runtime');
                    }
                    if (!overrides || typeof overrides !== 'object') overrides = {};

                    var interfaceNames = Array.isArray(overrides.interfaces)
                        ? overrides.interfaces.map(function (iface) { return ctorName(iface); })
                        : [];

                    // Literal CLR member names this JS object actually overrides. The bridge only
                    // emits an IL override for base virtuals named here (everything else falls
                    // through to the real base implementation) and implements exactly these
                    // interface members from JS (others fall back to default/no-op). Plain
                    // functions are method overrides; accessor descriptors become get_/set_-
                    // prefixed property overrides — matching the literal CLR accessor names the
                    // dynamic proxy already dispatches by. Read via getOwnPropertyDescriptor
                    // throughout so collecting names never invokes a JS getter as a side effect.
                    var memberNames = [];
                    for (var mkey in overrides) {
                        if (!Object.prototype.hasOwnProperty.call(overrides, mkey)) continue;
                        if (mkey === 'interfaces' || mkey === 'init') continue;
                        var mdesc = Object.getOwnPropertyDescriptor(overrides, mkey);
                        if (!mdesc) continue;
                        if (typeof mdesc.get === 'function' || typeof mdesc.set === 'function') {
                            if (typeof mdesc.get === 'function') memberNames.push('get_' + mkey);
                            if (typeof mdesc.set === 'function') memberNames.push('set_' + mkey);
                        } else if (typeof mdesc.value === 'function') {
                            memberNames.push(mkey);
                        }
                    }

                    var dispatcher = function () {
                        var a = Array.prototype.slice.call(arguments);
                        try {
                            var target = _wrapDotNetHandle(a[0]);
                            var method = a[1];
                            var margs = a[2] || [];
                            if (target && typeof method === 'string') {
                                var fn = overrides[method];
                                if (typeof fn === 'function') return fn.apply(target, Array.isArray(margs) ? margs : []);
                            }
                        } catch (_) { }
                    };
                    var handle = globalThis.__nsDotNetCreateJsSubclass(assembly || '', typeName || '', interfaceNames, memberNames, dispatcher);
                    var obj = _wrapDotNetHandle(handle);
                    ensureProxyInstance(obj, overrides, function () { });
                    return obj;
                },
                invokeById: invokeProxyById,
                listRegisteredManifests: function () {
                    if (typeof globalThis.__nsProxyListManifests === 'function') {
                        return globalThis.__nsProxyListManifests();
                    }
                    return [];
                },
            };

            function asDelegate(handler) {
                if (typeof handler === 'function') {
                    return handler;
                }
                if (handler && typeof handler.invoke === 'function') {
                    return handler.invoke.bind(handler);
                }
                throw new Error('NSWinRT.asDelegate(handler) expects a function or { invoke() } object');
            }

            function createEventEmitter() {
                var listeners = [];
                return {
                    add: function (handler) {
                        var normalized = asDelegate(handler);
                        listeners.push(normalized);
                        return {
                            dispose: function () {
                                var idx = listeners.indexOf(normalized);
                                if (idx >= 0) {
                                    listeners.splice(idx, 1);
                                }
                            },
                        };
                    },
                    emit: function () {
                        var args = Array.prototype.slice.call(arguments);
                        listeners.slice().forEach(function (listener) {
                            listener.apply(undefined, args);
                        });
                    },
                    count: function () {
                        return listeners.length;
                    },
                };
            }

            globalThis.NSWinRT.asDelegate = asDelegate;
            globalThis.NSWinRT.createEventEmitter = createEventEmitter;

            var moduleCache = new Map();

            function esmExportAliasAssignments(list) {
                if (!list || !list.trim()) {
                    return '';
                }

                var pairs = [];
                list.split(',').forEach(function (part) {
                    var token = part.trim();
                    if (!token) {
                        return;
                    }
                    var pieces = token.split(/\s+as\s+/i);
                    if (pieces.length === 2) {
                        pairs.push('exports.' + pieces[1].trim() + ' = ' + pieces[0].trim() + ';');
                    } else {
                        pairs.push('exports.' + token + ' = ' + token + ';');
                    }
                });

                return pairs.join('\n');
            }

            function transformEsmToRuntimeModule(source) {
                var transformed = String(source || '');

                transformed = transformed.replace(/^[ \t]*import\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s+['\"]([^'\"]+)['\"];?[ \t]*$/gm,
                    'const $1 = __nsImport("$2", __filename);');
                transformed = transformed.replace(/^[ \t]*import\s+\{\s*([^}]+)\s*\}\s+from\s+['\"]([^'\"]+)['\"];?[ \t]*$/gm,
                    'const { $1 } = __nsImport("$2", __filename);');
                transformed = transformed.replace(/^[ \t]*import\s+([A-Za-z_$][\w$]*)\s+from\s+['\"]([^'\"]+)['\"];?[ \t]*$/gm,
                    'const $1 = (function(m){ return (m && Object.prototype.hasOwnProperty.call(m, "default")) ? m.default : m; })(__nsImport("$2", __filename));');
                transformed = transformed.replace(/^[ \t]*import\s+['\"]([^'\"]+)['\"];?[ \t]*$/gm,
                    '__nsImport("$1", __filename);');
                transformed = transformed.replace(/\bimport\s*\(/g, '__nsDynamicImport(');

                transformed = transformed.replace(/\bexport\s+default\s+/g, 'exports.default = ');
                transformed = transformed.replace(/^[ \t]*export\s+\{\s*([^}]+)\s*\};?[ \t]*$/gm, function (_, list) {
                    return esmExportAliasAssignments(list);
                });

                var exportedNames = [];
                transformed = transformed.replace(/^[ \t]*export\s+(const|let|var)\s+([A-Za-z_$][\w$]*)/gm, function (_, keyword, name) {
                    exportedNames.push(name);
                    return keyword + ' ' + name;
                });
                transformed = transformed.replace(/^[ \t]*export\s+function\s+([A-Za-z_$][\w$]*)/gm, function (_, name) {
                    exportedNames.push(name);
                    return 'function ' + name;
                });
                transformed = transformed.replace(/^[ \t]*export\s+class\s+([A-Za-z_$][\w$]*)/gm, function (_, name) {
                    exportedNames.push(name);
                    return 'class ' + name;
                });

                if (exportedNames.length > 0) {
                    transformed += '\n' + exportedNames.map(function (name) {
                        return 'exports.' + name + ' = ' + name + ';';
                    }).join('\n') + '\n';
                }

                return transformed;
            }

            function executeRuntimeModule(source, filename) {
                var modulePath = String(filename || '');
                if (!modulePath) {
                    throw new Error('executeRuntimeModule requires a module path');
                }
                if (moduleCache.has(modulePath)) {
                    return moduleCache.get(modulePath);
                }

                var module = { exports: {} };
                moduleCache.set(modulePath, module.exports);

                var transformed = transformEsmToRuntimeModule(source);
                var dirname = modulePath.replace(/[\\/][^\\/]*$/, '');
                var executor = new Function('exports', 'module', '__nsImport', '__nsDynamicImport', '__filename', '__dirname', transformed);
                executor(module.exports, module, __nsImport, __nsDynamicImport, modulePath, dirname);
                moduleCache.set(modulePath, module.exports);
                return module.exports;
            }

            function __nsImport(specifier, parentPath) {
                if (typeof globalThis.__nsResolveModulePath !== 'function') {
                    throw new Error('Module resolver host function is not available');
                }
                if (typeof globalThis.__nsReadTextFile !== 'function') {
                    throw new Error('Module file reader host function is not available');
                }

                var resolved = globalThis.__nsResolveModulePath(
                    String(specifier || ''),
                    parentPath ? String(parentPath) : '',
                    globalThis.__nsAppRoot || ''
                );
                if (!resolved) {
                    throw new Error('Unable to resolve module: ' + specifier);
                }
                if (moduleCache.has(resolved)) {
                    return moduleCache.get(resolved);
                }

                var source = globalThis.__nsReadTextFile(resolved);
                return executeRuntimeModule(source, resolved);
            }

            function __nsDynamicImport(specifier, parentPath) {
                if (typeof specifier !== 'string' ||
                    (!specifier.startsWith('https://') && !specifier.startsWith('http://'))) {
                    return Promise.resolve().then(function () {
                        return __nsImport(specifier, parentPath);
                    });
                }

                try {
                    var sec = {};
                    try {
                        var pkgText = globalThis.__nsReadTextFile(
                            (globalThis.__nsAppRoot || '') + '/package.json'
                        );
                        sec = ((JSON.parse(pkgText) || {}).nativescript || {}).security || {};
                    } catch (_) {}

                    if (!sec.allowRemoteModules) {
                        return Promise.reject(new Error(
                            'Remote module imports are disabled. ' +
                            'Set nativescript.security.allowRemoteModules=true in package.json'
                        ));
                    }

                    var allowlist = sec.remoteModuleAllowlist;
                    if (Array.isArray(allowlist) && allowlist.length > 0) {
                        var isAllowed = allowlist.some(function (p) {
                            return typeof p === 'string' && specifier.startsWith(p);
                        });
                        if (!isAllowed) {
                            return Promise.reject(new Error(
                                'Remote module URL is not in the allowlist: ' + specifier
                            ));
                        }
                    }

                    if (moduleCache.has(specifier)) {
                        return Promise.resolve(moduleCache.get(specifier));
                    }

                    var uri    = new Windows.Foundation.Uri(specifier);
                    var client = new Windows.Web.Http.HttpClient();

                    return NSWinRT.toPromise(client.getStringAsync(uri)).then(function (source) {
                        return executeRuntimeModule(String(source), specifier);
                    });
                } catch (e) {
                    return Promise.reject(e instanceof Error ? e : new Error(String(e)));
                }
            }

            globalThis.__nsInvalidateModuleCacheEntry = function (resolvedPath) {
                moduleCache.delete(String(resolvedPath || ''));
            };

            globalThis.__nsClearModuleCache = function () {
                moduleCache.clear();
            };

            globalThis.__nsEvalAsModule = function (source, filename) {
                return executeRuntimeModule(source, filename);
            };
            globalThis.NSWinRT.import = __nsImport;
            globalThis.__nsDynamicImport = __nsDynamicImport;
            globalThis.NSWinRT.dynamicImport = __nsDynamicImport;
        })();

        globalThis.__runtimeVersion = "1.0.0";

        // The runtime registers host-facing timer entrypoints into the JS global:
        //   global.__ns__setTimeout
        //   global.__ns__setInterval
        //   global.__ns__clearTimeout
        //   global.__ns__clearInterval
        // Embedders may replace these with
        // platform-specific implementations. When no host implementation is
        // available, the bootstrap provides default behaviors (throws for
        // setTimeout/setInterval and no-op for clearTimeout/clearInterval).
        (function () {
            if (typeof globalThis.__ns__setTimeout !== 'function') {
                globalThis.__ns__setTimeout = function () {
                    throw new Error('Timers are not available: embedder must provide global.__ns__setTimeout');
                };
            }
            if (typeof globalThis.__ns__setInterval !== 'function') {
                globalThis.__ns__setInterval = function () {
                    throw new Error('Timers are not available: embedder must provide global.__ns__setInterval');
                };
            }
            if (typeof globalThis.__ns__clearTimeout !== 'function') {
                globalThis.__ns__clearTimeout = function () { };
            }
            if (typeof globalThis.__ns__clearInterval !== 'function') {
                globalThis.__ns__clearInterval = function () { };
            }
        })();

        // Uses __nsDwmFlush() — the Windows equivalent of Choreographer /
        // CADisplayLink.  DwmFlush() blocks the calling thread until the next
        // monitor VSync, giving frame-perfect timing at any refresh rate
        // (60 / 120 / 144 / 240 Hz) with no timer overhead.
        //
        // On headless systems DwmFlush() returns immediately (composition
        // disabled), so rAF callbacks fire as fast as microtasks drain —
        // ideal for tests and headless rendering scenarios.
        (function () {
            var _nextId  = 0;
            var _pending = new Map();
            var _running = false;

            function _flush() {
                if (_pending.size === 0) { _running = false; return; }
                // Block until next VSync; returns ms timestamp.
                var ts = (typeof __nsDwmFlush === 'function')
                    ? __nsDwmFlush()
                    : performance.now();
                var cbs = Array.from(_pending);
                _pending.clear();
                for (var i = 0; i < cbs.length; i++) {
                    try { cbs[i][1](ts); } catch (e) {
                        console.log('rAF error:', e && e.message || e);
                    }
                }
                if (_pending.size > 0) queueMicrotask(_flush);
                else _running = false;
            }

            globalThis.requestAnimationFrame = function requestAnimationFrame(callback) {
                if (typeof callback !== 'function') return 0;
                var id = ++_nextId;
                _pending.set(id, callback);
                if (!_running) { _running = true; queueMicrotask(_flush); }
                return id;
            };

            globalThis.cancelAnimationFrame = function cancelAnimationFrame(id) {
                _pending.delete(id);
            };
        })();

        // The native classes are installed by install_url_globals() before this
        // script runs.  This block adds the JS-layer searchParams accessor
        // (mutation propagation + caching), Symbol.iterator, and the blob-URL API.
        (function () {
            // searchParams: lazily created, mutations propagate back to URL.search
            Object.defineProperty(URL.prototype, 'searchParams', {
                get: function () {
                    if (this._searchParams == null) {
                        var self = this;
                        var sp = new URLSearchParams(this.search);
                        Object.defineProperty(sp, '_url', { enumerable: false, writable: false, value: self });
                        var _append = sp.append.bind(sp);
                        sp.append = function (name, value) { _append(name, value); self.search = sp.toString(); };
                        var _delete = sp.delete.bind(sp);
                        sp.delete = function (name, value) { _delete(name, value); self.search = sp.toString(); };
                        var _set = sp.set.bind(sp);
                        sp.set = function (name, value) { _set(name, value); self.search = sp.toString(); };
                        var _sort = sp.sort.bind(sp);
                        sp.sort = function () { _sort(); self.search = sp.toString(); };
                        this._searchParams = sp;
                    }
                    return this._searchParams;
                },
                configurable: true,
            });

            // Make URLSearchParams iterable (entries by default)
            URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;

            var BLOB_STORE = new Map();
            var _blobSeq = 0;
            URL.createObjectURL = function (object, options) {
                if (!(object instanceof globalThis.Blob)) return null;
                var id = (++_blobSeq).toString(16).padStart(8, '0') + '-'
                    + Math.floor(Math.random() * 0x10000).toString(16).padStart(4, '0') + '-4'
                    + Math.floor(Math.random() * 0x1000).toString(16).padStart(3, '0') + '-'
                    + (Math.floor(Math.random() * 4) + 8).toString(16)
                    + Math.floor(Math.random() * 0x1000).toString(16).padStart(3, '0') + '-'
                    + Math.floor(Math.random() * 0x1000000000000).toString(16).padStart(12, '0');
                var url = 'blob:nativescript/' + id;
                BLOB_STORE.set(url, { blob: object, type: object.type, ext: options && options.ext });
                return url;
            };
            URL.revokeObjectURL = function (url) { BLOB_STORE.delete(url); };
            URL.InternalAccessor = {
                getData: function (url) { return BLOB_STORE.get(url) || null; },
            };
        })();

        // ada-url (C++) doesn't expose URLPattern; this JS polyfill covers the
        // common cases: :name capture groups, * / ** wildcards, literal segments.
        (function () {
            if (typeof URLPattern !== 'undefined') return;

            function _esc(s) {
                return s.replace(/[-[\]{}()*+?.,\\^$|#\s]/g, '\\$&');
            }

            function _compile(pat) {
                if (pat === undefined || pat === null) pat = '*';
                var s = String(pat);
                var groups = [];
                var re = '';
                var i = 0;
                while (i < s.length) {
                    var c = s[i];
                    if (c === ':') {
                        var j = i + 1;
                        while (j < s.length && /\w/.test(s[j])) j++;
                        var name = s.slice(i + 1, j);
                        groups.push(name);
                        re += '([^/?#]+?)';
                        i = j;
                    } else if (c === '*' && s[i + 1] === '*') {
                        groups.push('0');
                        re += '(.*)';
                        i += 2;
                    } else if (c === '*') {
                        groups.push('0');
                        re += '([^/?#]*)';
                        i++;
                    } else {
                        re += _esc(c);
                        i++;
                    }
                }
                return { re: new RegExp('^' + re + '$'), groups: groups };
            }

            function _match(compiled, value) {
                var m = String(value || '').match(compiled.re);
                if (!m) return null;
                var groups = {};
                for (var i = 0; i < compiled.groups.length; i++) {
                    groups[compiled.groups[i]] = m[i + 1] !== undefined ? m[i + 1] : undefined;
                }
                return { input: String(value || ''), groups: groups };
            }

            function URLPattern(init, baseURL) {
                if (typeof init === 'string') {
                    try {
                        var u = new URL(init, baseURL || 'http://_placeholder_');
                        init = {
                            protocol: u.protocol.slice(0, -1),
                            hostname: u.hostname,
                            port: u.port,
                            pathname: u.pathname,
                            search: u.search ? u.search.slice(1) : '',
                            hash: u.hash ? u.hash.slice(1) : '',
                        };
                    } catch (e) {
                        init = { pathname: init };
                    }
                }
                init = init || {};
                var opt = function (v, d) { return v !== undefined && v !== null ? String(v) : (d !== undefined ? d : '*'); };
                this.protocol = opt(init.protocol);
                this.username  = opt(init.username);
                this.password  = opt(init.password);
                this.hostname  = opt(init.hostname);
                this.port      = opt(init.port);
                this.pathname  = opt(init.pathname, '/*');
                this.search    = opt(init.search);
                this.hash      = opt(init.hash);
                this._pc = _compile(this.protocol);
                this._uc = _compile(this.username);
                this._pw = _compile(this.password);
                this._hc = _compile(this.hostname);
                this._oc = _compile(this.port);
                this._ac = _compile(this.pathname);
                this._sc = _compile(this.search);
                this._xc = _compile(this.hash);
            }

            URLPattern.prototype.test = function (input, baseURL) {
                return this.exec(input, baseURL) !== null;
            };

            URLPattern.prototype.exec = function (input, baseURL) {
                var url;
                try {
                    if (typeof input === 'string') {
                        url = new URL(input, baseURL || undefined);
                    } else if (input && input.href) {
                        url = new URL(input.href);
                    } else {
                        url = input || {};
                    }
                } catch (e) { return null; }

                var rm = _match(this._pc, url.protocol ? url.protocol.slice(0, -1) : '');
                var um = _match(this._uc, url.username || '');
                var pm = _match(this._pw, url.password || '');
                var hm = _match(this._hc, url.hostname || '');
                var om = _match(this._oc, url.port || '');
                var am = _match(this._ac, url.pathname || '/');
                var sm = _match(this._sc, url.search ? url.search.slice(1) : '');
                var xm = _match(this._xc, url.hash ? url.hash.slice(1) : '');

                if (!rm || !um || !pm || !hm || !om || !am || !sm || !xm) return null;
                return {
                    inputs:   [input],
                    protocol: rm, username: um, password: pm,
                    hostname: hm, port: om,     pathname: am,
                    search:   sm, hash: xm,
                };
            };

            globalThis.URLPattern = URLPattern;
        })();

        // Requires the dotnet-bridge project to be published into
        //   <app-root>/dotnet-bridge/publish/DotNetBridge.dll
        (function () {
            if (typeof globalThis.__nsDotNetInvokeBin !== 'function') return;

            // Binary protocol: no JSON.stringify / JSON.parse.
            // __nsDotNetInvokeBin(handle, typeName, assembly, method, ...args)
            // throws on error, returns the result value directly.
            function _invoke(req) {
                var handle = (req.handle !== undefined && req.handle !== null) ? req.handle : -1;
                var args   = req.args || [];
                return globalThis.__nsDotNetInvokeBin(
                    handle,
                    req.typeName  || '',
                    req.assembly  || '',
                    req.method    || '',
                    ...args
                );
            }

            // Populated lazily on first access; avoids repeated bridge round-trips.
            var _typeInfoCache = {};
            var _emptyInfo = { methods: [], properties: [], staticMethods: [], staticProperties: [], readonlyProperties: [], readonlyStaticProperties: [], writeonlyProperties: [], writeonlyStaticProperties: [] };
            // Optional mapping for namespace prefixes -> assembly simple-name.
            // Exact namespaces are preferred first, then progressively shorter
            // prefixes are tried as a fallback.
            var _namespaceAssemblyMap = Object.create(null);

            function _resolveAssembly(typeName) {
                if (!typeName || typeof typeName !== 'string') return '';
                var probe = String(typeName);
                while (probe) {
                    var assembly = _namespaceAssemblyMap[probe];
                    if (typeof assembly === 'string' && assembly) return assembly;
                    var lastDot = probe.lastIndexOf('.');
                    if (lastDot < 0) break;
                    probe = probe.substring(0, lastDot);
                }
                return '';
            }

            // When the JS GC collects a DotNet proxy the registry fires the
            // callback with the managed handle id, releasing the CLR reference.
            // Explicit sw.release() still works for deterministic teardown.
            var _dotNetFinalizers = typeof FinalizationRegistry === 'function'
                ? new FinalizationRegistry(function (handle) {
                    try { _invoke({ handle: handle, method: '__release', args: [] }); } catch (e) {}
                  })
                : null;

            // Auto-populate namespace→assembly map from the managed side.
            // GetNamespaceAssemblyMapJson scans the app directory for assemblies and
            // returns a JSON map of { rootNamespace: assemblySimpleName }.
            // Runs once at IIFE init; any failure is silently ignored.
            try {
                var _autoNsJson = _invoke({ assembly: '', typeName: 'NativeScriptBridge.Bridge', method: 'GetNamespaceAssemblyMapJson', args: [] });
                if (typeof _autoNsJson === 'string' && _autoNsJson) {
                    var _parsed = JSON.parse(_autoNsJson);
                    if (_parsed && typeof _parsed === 'object') {
                        for (var _k in _parsed) {
                            if (Object.prototype.hasOwnProperty.call(_parsed, _k) && typeof _parsed[_k] === 'string') {
                                _namespaceAssemblyMap[_k] = _parsed[_k];
                            }
                        }
                    }
                }
            } catch (_e) {}

            function _getTypeInfo(assembly, typeName) {
                if (!typeName) return _emptyInfo;
                var cached = _typeInfoCache[typeName];
                if (cached !== undefined) return cached;
                try {
                    // Respect an explicitly-provided assembly name. When empty,
                    // let the managed side attempt resolution (BCL types via Type.GetType).
                    var asm = (typeof assembly === 'string') ? assembly : '';
                    var info = _invoke({ assembly: asm, typeName: typeName, method: '__members__', args: [] });
                    _typeInfoCache[typeName] = (info && typeof info === 'object') ? info : _emptyInfo;
                } catch (e) {
                    _typeInfoCache[typeName] = _emptyInfo;
                }
                return _typeInfoCache[typeName];
            }

            function _unwrap(v) {
                if (v && typeof v === 'object' && typeof v.__handle === 'number') return { __handle: v.__handle };
                return v;
            }

            // Makes sw.Stop() and sw.Elapsed both work naturally.
            // The proxy is registered with _dotNetFinalizers so the CLR reference
            // is released automatically when JS GC collects the proxy.
            function _makeDotNetInstance(handle, assembly, typeName, isTask, nativePtr) {
                var info = _getTypeInfo(assembly, typeName);
                var proxy = new Proxy({}, {
                    get: function (_, prop) {
                        if (typeof prop === 'symbol') return undefined;
                        if (prop === '__handle') return handle;
                        if (prop === '__type')   return typeName;
                        if (prop === '__isTask') return isTask === true;
                        if (prop === '__native_ptr') return nativePtr;
                        if (prop === 'release') return function () {
                            _invoke({ handle: handle, method: '__release', args: [] });
                        };
                        if (prop === 'toString') return function () {
                            return '[DotNetObject ' + typeName + ' #' + handle + ']';
                        };
                        if (prop === 'then') return undefined;
                        // Re-read info in case it was populated after construction.
                        var i = _typeInfoCache[typeName] || _emptyInfo;
                        // Write-only: has setter but no getter — reading it is an error.
                        if (i.writeonlyProperties && i.writeonlyProperties.indexOf(prop) >= 0)
                            throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                        if (i.properties && i.properties.indexOf(prop) >= 0)
                            return _wrap(_invoke({ handle: handle, method: 'get_' + prop, args: [] }));
                        // Not a native property — return a callable for method dispatch.
                        return function () {
                            var args = Array.prototype.slice.call(arguments).map(_unwrap);
                            return _wrap(_invoke({ handle: handle, method: prop, args: args }));
                        };
                    },
                    set: function (_, prop, value) {
                        if (typeof prop === 'symbol') return true;
                        var i = _typeInfoCache[typeName] || _emptyInfo;
                        // Read-only: has getter but no setter — assignment is an error.
                        if (i.readonlyProperties && i.readonlyProperties.indexOf(prop) >= 0)
                            throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                        // Writable (read-write or write-only) — invoke the setter.
                        if ((i.properties && i.properties.indexOf(prop) >= 0) ||
                            (i.writeonlyProperties && i.writeonlyProperties.indexOf(prop) >= 0))
                            _invoke({ handle: handle, method: 'set_' + prop, args: [_unwrap(value)] });
                        // Not a native property — don't intercept, let JS do its thing.
                        return true;
                    },
                });
                if (_dotNetFinalizers) _dotNetFinalizers.register(proxy, handle);
                return proxy;
            }

            function _wrap(value) {
                if (value == null) return null;
                if (Array.isArray(value)) return value.map(_wrap);
                if (typeof value === 'object' && typeof value.__handle === 'number') {
                    var typeName = value.__type || '';
                    var assembly = _resolveAssembly(typeName);
                    return _makeDotNetInstance(value.__handle, assembly, typeName, value.__isTask === true, value.__native_ptr);
                }
                return value;
            }

            globalThis.NSWinRT.dotnet = {
                invoke: function (assembly, typeName, method, args) {
                    return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: method, args: (args || []).map(_unwrap) }));
                },
                get: function (assembly, typeName, prop) {
                    return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: 'get_' + prop, args: [] }));
                },
                fromHandle: function (handle, typeName) {
                    var assembly = _resolveAssembly(typeName || '');
                    return _makeDotNetInstance(handle, assembly, typeName || '');
                },
                // Exposed so other IIFEs in this bootstrap (e.g. the managed-subclass/proxy block,
                // which runs in its own closure earlier in the script and has no direct access to
                // this IIFE's private `_wrap`) can wrap a raw tagged handle value the same way.
                wrap: _wrap,
                // Lazily register a namespace prefix and optional assembly mapping.
                // Example: NSWinRT.dotnet.registerNamespace('com', 'NativeScript');
                registerNamespace: function(rootName, assemblyName) {
                    if (!rootName || typeof rootName !== 'string') return;
                    var name = String(rootName);
                    var root = name.split('.')[0];
                    if (assemblyName && typeof assemblyName === 'string') {
                        _namespaceAssemblyMap[name] = assemblyName;
                        if (!_namespaceAssemblyMap[root]) {
                            _namespaceAssemblyMap[root] = assemblyName;
                        }
                    }
                    if (root in globalThis) return;
                    try {
                        Object.defineProperty(globalThis, root, {
                            configurable: true,
                            enumerable: true,
                            get: function() {
                                var proxy = _makeNamespaceProxy(root);
                                try { Object.defineProperty(globalThis, root, { value: proxy, writable: true, configurable: true, enumerable: true }); } catch (_) {}
                                return proxy;
                            }
                        });
                    } catch (_) {
                        try { globalThis[root] = _makeNamespaceProxy(root); } catch (_) {}
                    }
                },
                registerNamespaces: function(arr) {
                    if (!Array.isArray(arr)) return;
                    for (var i = 0; i < arr.length; i++) this.registerNamespace(arr[i]);
                },
            };

            // System.Diagnostics.Stopwatch.StartNew()    →  static method call
            // System.Environment.MachineName             →  static property get
            // new System.Text.StringBuilder(64)          →  constructor
            // sw.Stop()                                  →  instance method
            // sw.Elapsed                                 →  instance property
            function _makeNamespaceProxy(path) {
                function _node() {}
                return new Proxy(_node, {
                    get: function (_, prop) {
                        if (typeof prop === 'symbol') return undefined;
                        // Prevent string-coercion methods from descending into sub-proxies.
                        // V8's console.log / JSON.stringify call toString/valueOf on unknown objects;
                        // returning a sub-proxy causes the apply trap to fire with a non-existent
                        // type name, producing a spurious "Type not found" bridge error.
                        if (prop === 'toString' || prop === 'valueOf')
                            return function() { return '[.NET ' + path + ']'; };
                        var assembly = _resolveAssembly(path);
                        var info = _getTypeInfo(assembly, path);
                        // Write-only static property — reading it is an error.
                        if (info.writeonlyStaticProperties && info.writeonlyStaticProperties.indexOf(prop) >= 0)
                            throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                        // Readable static property: resolve value immediately.
                        if (info.staticProperties && info.staticProperties.indexOf(prop) >= 0)
                            return _wrap(_invoke({ assembly: assembly, typeName: path, method: 'get_' + prop, args: [] }));
                        // Static method: return a callable.
                        if (info.staticMethods && info.staticMethods.indexOf(prop) >= 0) {
                            return function () {
                                var args = Array.prototype.slice.call(arguments).map(_unwrap);
                                return _wrap(_invoke({ assembly: assembly, typeName: path, method: prop, args: args }));
                            };
                        }
                        // Namespace / sub-type: keep descending.
                        return _makeNamespaceProxy(path + '.' + prop);
                    },
                    set: function (_, prop, value) {
                        if (typeof prop === 'symbol') return true;
                        var assembly = _resolveAssembly(path);
                        var info = _getTypeInfo(assembly, path);
                        // Read-only static property — assignment is an error.
                        if (info.readonlyStaticProperties && info.readonlyStaticProperties.indexOf(prop) >= 0)
                            throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                        // Writable (read-write or write-only) — invoke the setter.
                        if ((info.staticProperties && info.staticProperties.indexOf(prop) >= 0) ||
                            (info.writeonlyStaticProperties && info.writeonlyStaticProperties.indexOf(prop) >= 0))
                            _invoke({ assembly: assembly, typeName: path, method: 'set_' + prop, args: [_unwrap(value)] });
                        // Not a native property — don't intercept.
                        return true;
                    },
                    apply: function (_, _this, args) {
                        var lastDot  = path.lastIndexOf('.');
                        // No dot means this is a top-level name being called directly —
                        // there is no type+method pair to dispatch, so bail out.
                        if (lastDot <= 0) return undefined;
                        var typeName = path.substring(0, lastDot);
                        var method   = path.substring(lastDot + 1);
                        var assembly = _resolveAssembly(typeName);
                        return _wrap(_invoke({ assembly: assembly, typeName: typeName, method: method, args: args.map(_unwrap) }));
                    },
                    construct: function (_, args) {
                        var assembly = _resolveAssembly(path);
                        return _wrap(_invoke({ assembly: assembly, typeName: path, method: '.ctor', args: args.map(_unwrap) }));
                    },
                });
            }
            globalThis.System         = _makeNamespaceProxy('System');
            globalThis.Microsoft      = _makeNamespaceProxy('Microsoft');
            globalThis.NativeScript   = _makeNamespaceProxy('NativeScript');

            // Creates a typed .NET BCL delegate (not a WinRT COM delegate).
            // Use this when the API expects a managed System.Action,
            // System.EventHandler, etc. rather than a WinRT delegate interface.
            // For WinRT delegate types use NSWinRT.asDelegate instead.
            if (typeof globalThis.__nsDotNetAwaitTask === 'function') {
                globalThis.NSWinRT.dotnet.taskToPromise = function (obj) {
                    var h = obj && typeof obj.__handle === 'number' ? obj.__handle
                          : typeof obj === 'number' ? obj : -1;
                    if (h < 0) return Promise.resolve(obj);
                    return new Promise(function (resolve, reject) {
                        globalThis.__nsDotNetAwaitTask(h, resolve, reject);
                    });
                };
            }

            if (typeof globalThis.__nsDotNetCreateDelegate === 'function') {
                globalThis.NSWinRT.dotnet.asDelegate = function(typeNameOrFn, fn) {
                    var typeName, callback;
                    if (typeof typeNameOrFn === 'function') {
                        typeName = '';
                        callback = typeNameOrFn;
                    } else {
                        typeName = typeNameOrFn || '';
                        callback = fn;
                    }
                    if (typeof callback !== 'function')
                        throw new TypeError('NSWinRT.dotnet.asDelegate: callback must be a function');
                    var wrapped = function() {
                        var args = Array.prototype.slice.call(arguments).map(_wrap);
                        return callback.apply(null, args);
                    };
                    return globalThis.__nsDotNetCreateDelegate(typeName, wrapped);
                };
            }

            if (typeof globalThis.__nsRunOnUIThread === 'function') {
                globalThis.NSWinRT.runOnUIThread = function(fn) {
                    if (typeof fn !== 'function') throw new TypeError('NSWinRT.runOnUIThread: expected a function');
                    return globalThis.__nsRunOnUIThread(fn);
                };
            }
        })();

        (function () {
            if (typeof globalThis.__nsWin32Call !== 'function') return;

            // Auto-type a bare JS value to a {type, value} descriptor.
            function _autoType(v) {
                if (v === null || v === undefined) return { type: 'pointer', value: 0 };
                if (typeof v === 'object' && typeof v.type === 'string') return v;
                if (typeof v === 'string')  return { type: 'wstr', value: v };
                if (typeof v === 'boolean') return { type: 'bool', value: v };
                if (typeof v === 'number')
                    return { type: Number.isInteger(v) ? 'i32' : 'f64', value: v };
                return { type: 'i32', value: Number(v) };
            }

            globalThis.NSWinRT.win32 = {
                /**
                 * Low-level call with explicit typed args.
                 *   NSWinRT.win32.call('user32.dll', 'MessageBoxW', 'i32',
                 *     {type:'pointer',value:0}, {type:'wstr',value:'Hello'}, ...)
                 */
                call: function (dll, fn_name, returnType) {
                    var args = Array.prototype.slice.call(arguments, 3);
                    if (typeof globalThis.__nsWin32CallRaw === 'function') {
                        var rawArgs = [dll, fn_name, returnType];
                        for (var i = 0; i < args.length; i++) {
                            rawArgs.push(args[i].type, args[i].value);
                        }
                        return globalThis.__nsWin32CallRaw.apply(null, rawArgs);
                    }
                    var json = JSON.stringify({ dll: dll, fn: fn_name, returnType: returnType, args: args });
                    return JSON.parse(globalThis.__nsWin32Call(json)).value;
                },

                /**
                 * Returns a Proxy where every property is a callable Win32 function.
                 * Args are auto-typed from their JS values; strings become wstr,
                 * numbers become i32 (or f64 if non-integer), null becomes pointer(0).
                 *
                 *   const user32 = NSWinRT.win32.bind('user32.dll');
                 *   const { MessageBoxW } = NSWinRT.win32.bind('user32.dll');
                 *   MessageBoxW(null, 'Hello!', 'Title', 0);
                 */
                bind: function (dll, returnType) {
                    var ret = returnType || 'i32';
                    var raw = globalThis.__nsWin32CallRaw;
                    var bindFast = globalThis.__nsWin32BindFast;
                    var win32 = globalThis.NSWinRT.win32;
                    // One closure per name: a fresh closure per property access
                    // defeats V8's inline caches at the call sites.
                    var fns = Object.create(null);
                    return new Proxy({}, {
                        get: function (_, name) {
                            if (typeof name !== 'string') return undefined;
                            var hit = fns[name];
                            if (hit !== undefined) return hit;
                            var fn;
                            if (typeof raw === 'function') {
                                // Numeric signatures upgrade to a V8 Fast API Function
                                // on first call; types derived from the call site lock
                                // in, matching the native CIF cache. The wrapper keeps
                                // delegating so captured references (import() globals,
                                // destructuring) get the fast path too.
                                var tryFast = typeof bindFast === 'function';
                                var fastFn = null;
                                fn = function () {
                                    if (fastFn !== null) return fastFn.apply(null, arguments);
                                    if (tryFast) {
                                        tryFast = false;
                                        var types = [];
                                        var ok = arguments.length <= 4;
                                        for (var i = 0; ok && i < arguments.length; i++) {
                                            var v = arguments[i];
                                            if (typeof v === 'number') types.push(Number.isInteger(v) ? 'i32' : 'f64');
                                            else if (typeof v === 'boolean') types.push('bool');
                                            else if (v === null || v === undefined) types.push('pointer');
                                            else ok = false;
                                        }
                                        if (ok) {
                                            var f = bindFast(dll, name, ret, types);
                                            if (f) {
                                                fastFn = f;
                                                fns[name] = f;
                                                return f.apply(null, arguments);
                                            }
                                        }
                                    }
                                    var rawArgs = [dll, name, ret];
                                    for (var i = 0; i < arguments.length; i++) {
                                        var t = _autoType(arguments[i]);
                                        rawArgs.push(t.type, t.value);
                                    }
                                    return raw.apply(null, rawArgs);
                                };
                            } else {
                                fn = function () {
                                    var args = Array.prototype.slice.call(arguments).map(_autoType);
                                    return win32.call.apply(win32, [dll, name, ret].concat(args));
                                };
                            }
                            fns[name] = fn;
                            return fn;
                        },
                    });
                },

                /**
                 * Returns an object with typed function wrappers from explicit signatures.
                 *
                 *   const api = NSWinRT.win32.define('user32.dll', {
                 *       MessageBoxW:      ['pointer','wstr','wstr','u32'],
                 *       GetSystemMetrics: ['i32'],
                 *   }, 'i32');
                 *   api.MessageBoxW(null, 'Hello', 'Title', 0);
                 */
                define: function (dll, signatures, defaultReturnType) {
                    var ret = defaultReturnType || 'i32';
                    var win32 = globalThis.NSWinRT.win32;
                    var obj = {};
                    Object.keys(signatures).forEach(function (name) {
                        var argTypes = signatures[name];
                        obj[name] = function () {
                            var jsArgs = Array.prototype.slice.call(arguments);
                            var typedArgs = jsArgs.map(function (v, i) {
                                var ty = argTypes[i];
                                return ty ? { type: ty, value: v } : _autoType(v);
                            });
                            return win32.call.apply(win32, [dll, name, ret].concat(typedArgs));
                        };
                    });
                    return obj;
                },

                /**
                 * Enumerates all exports of a DLL and installs each one as a global
                 * function on `globalThis`, auto-typing args.
                 *
                 *   NSWinRT.win32.import('user32.dll');
                 *   MessageBoxW(null, 'Hello!', 'Title', 0); // ← now a plain global
                 */
                import: function (dll, returnType) {
                    if (typeof globalThis.__nsWin32Exports !== 'function') return;
                    var exports = JSON.parse(globalThis.__nsWin32Exports(dll));
                    var bound   = globalThis.NSWinRT.win32.bind(dll, returnType || 'i32');
                    exports.forEach(function (name) {
                        if (!(name in globalThis))
                            Object.defineProperty(globalThis, name, {
                                value: bound[name], writable: true, configurable: true,
                            });
                    });
                },
            };
        })();

        // Wraps a JS function as a typed WinRT delegate via the native JsDelegate
        // COM bridge using WinRT metadata for GUID and parameter-type resolution.
        //
        //   NSWinRT.asDelegate('Windows.UI.Popups.UICommandInvokedHandler', fn)
        //
        // Returns {handle} which can be passed directly to any WinRT event-add
        // or method that expects that delegate type.
        // Callback parameters that carry COM object pointers arrive as
        // {handle: External}; wrap them with the appropriate WinRT constructor
        // to get a full proxy.
        // For managed BCL delegates (System.Action etc.) use
        // NSWinRT.dotnet.asDelegate() instead.
        (function () {
            globalThis.NSWinRT.asDelegate = function(typeNameOrFn, fn) {
                // Two-argument form: asDelegate(typeName, fn) → COM-backed JsDelegate
                if (typeof typeNameOrFn === 'string') {
                    if (typeof fn !== 'function')
                        throw new TypeError('NSWinRT.asDelegate: callback must be a function');
                    if (typeof globalThis.__nsAsDelegate === 'function')
                        return globalThis.__nsAsDelegate(typeNameOrFn, fn);
                    return fn;
                }
                // One-argument form: asDelegate(fn) or asDelegate({invoke(){}})
                if (typeof typeNameOrFn === 'function') {
                    return typeNameOrFn;
                }
                if (typeNameOrFn && typeof typeNameOrFn.invoke === 'function') {
                    return typeNameOrFn.invoke.bind(typeNameOrFn);
                }
                throw new TypeError('NSWinRT.asDelegate: expected a function, { invoke() } object, or (typeName, fn) pair');
            };
            // Build a native WinRT IVector<IInspectable> of `count` boxed Int32 indices,
            // suitable for assigning to a XAML ItemsSource (enables WinUI virtualization).
            globalThis.NSWinRT.makeItemsSource = function(count) {
                if (typeof globalThis.__nsMakeItemsSource === 'function')
                    return globalThis.__nsMakeItemsSource(count >>> 0);
                return null;
            };
            // Grow a source built by makeItemsSource to `newCount` items in place (fires VectorChanged
            // so WinUI adds only the new rows — preserves scroll). Used by ListView infinite-scroll.
            globalThis.NSWinRT.extendItemsSource = function(source, newCount) {
                if (source && typeof globalThis.__nsExtendItemsSource === 'function')
                    globalThis.__nsExtendItemsSource(source, newCount >>> 0);
            };
            // Granular in-place edits of a makeItemsSource source (fire VectorChanged so WinUI updates
            // only the affected rows — no full re-realize). Used by ListView for ObservableArray
            // add/delete/update/splice instead of refresh(). index/count are clamped to uint32.
            globalThis.NSWinRT.insertItemsSource = function(source, index, count) {
                if (source && typeof globalThis.__nsInsertItemsSource === 'function')
                    globalThis.__nsInsertItemsSource(source, index >>> 0, count >>> 0);
            };
            globalThis.NSWinRT.removeItemsSource = function(source, index, count) {
                if (source && typeof globalThis.__nsRemoveItemsSource === 'function')
                    globalThis.__nsRemoveItemsSource(source, index >>> 0, count >>> 0);
            };
            globalThis.NSWinRT.updateItemsSource = function(source, index, count) {
                if (source && typeof globalThis.__nsUpdateItemsSource === 'function')
                    globalThis.__nsUpdateItemsSource(source, index >>> 0, count >>> 0);
            };
            // Bulk change → rebuild to `count` rows + fire ONE VectorChanged(Reset). WinRT has no range
            // event, so this is the single-event way to apply a wholesale replace / large splice.
            globalThis.NSWinRT.resetItemsSource = function(source, count) {
                if (source && typeof globalThis.__nsResetItemsSource === 'function')
                    globalThis.__nsResetItemsSource(source, count >>> 0);
            };
        })();

        (function () {
            var cjsCache = new Map();

            function resolveSpecifier(specifier, callerFile) {
                // Guard non-string / empty specifiers (e.g. loadModule(null) when the
                // module resolver can't find a registered module for a path). Throw a
                // clear error instead of letting `specifier.charAt(0)` raise a cryptic
                // TypeError or reaching native code with a coerced "null".
                if (typeof specifier !== 'string' || specifier.length === 0) {
                    throw new Error('Cannot find module: ' + String(specifier));
                }
                if (typeof globalThis.__nsResolveModulePath !== 'function') return null;
                var appRoot = (globalThis.__nsAppRoot || '').replace(/[\\/]+$/, '');

                // NativeScript tilde alias: ~/foo → {appRoot}/app/foo
                if (specifier.charAt(0) === '~' && specifier.charAt(1) === '/') {
                    var abs = appRoot + '\\app\\' + specifier.substring(2).replace(/\//g, '\\');
                    // Pass absolute path; resolver treats it as-is and tries extensions.
                    return globalThis.__nsResolveModulePath(abs, '', appRoot) || abs;
                }

                // Relative (./foo, ../foo) or bare name: use native resolver with caller context.
                // Fall back to app/bundle.js as parent so top-level require('./chunk.js') works.
                var parent = callerFile || (appRoot + '\\app\\bundle.js');
                return globalThis.__nsResolveModulePath(specifier, parent, appRoot);
            }

            function makeRequire(callerFile) {
                return function require(specifier) {
                    if (specifier === 'ns:module' && globalThis.__nsModuleBuiltin) return globalThis.__nsModuleBuiltin;
                    var resolved = resolveSpecifier(specifier, callerFile);
                    if (!resolved) throw new Error('Cannot find module: ' + specifier);

                    var key = resolved.replace(/\\/g, '/').toLowerCase();
                    if (cjsCache.has(key)) return cjsCache.get(key).exports;

                    var mod = { id: resolved, filename: resolved, exports: {} };
                    cjsCache.set(key, mod); // set before eval to break circular deps

                    var isJson = key.endsWith('.json');
                    var content = typeof globalThis.__nsReadTextFile === 'function'
                        ? globalThis.__nsReadTextFile(resolved) : null;

                    if (isJson) {
                        try { mod.exports = JSON.parse(content || '{}'); } catch (_) { mod.exports = {}; }
                        return mod.exports;
                    }

                    if (content === null || content === undefined)
                        throw new Error('Failed to read module: ' + resolved);

                    var dirName = resolved.replace(/\//g, '\\').replace(/\\[^\\]*$/, '');
                    var childRequire = makeRequire(resolved);

                    try {
                        var factory = new Function('module', 'exports', 'require', '__filename', '__dirname', content);
                        factory(mod, mod.exports, childRequire, resolved, dirName);
                    } catch (e) {
                        cjsCache.delete(key);
                        throw e;
                    }
                    cjsCache.set(key, mod);
                    return mod.exports;
                };
            }

            globalThis.require = makeRequire(null);

            // The `ns:module` builtin (require/import/import()): the dev-loader control surface
            // @nativescript/vite's HMR client drives, as on iOS/Android. Members are frozen and
            // non-throwing to feature-detect.
            if (!globalThis.__nsModuleBuiltin && typeof globalThis.__nsModuleConfigureLoader === 'function') {
                var nsModule = {
                    configureLoader: function configureLoader(config) {
                        if (!config || typeof config !== 'object') throw new TypeError('configureLoader expects a config object');
                        globalThis.__nsModuleConfigureLoader(JSON.stringify(config));
                    },
                    invalidateModules: function invalidateModules(urls) {
                        return globalThis.__nsModuleInvalidate(urls);
                    },
                    getLoadedModuleUrls: function getLoadedModuleUrls() {
                        return globalThis.__nsModuleLoadedUrls();
                    },
                    createRequire: function createRequire(filenameOrURL) {
                        var value = filenameOrURL && typeof filenameOrURL === 'object' ? filenameOrURL.href : filenameOrURL;
                        if (typeof value !== 'string') {
                            throw new TypeError("The argument 'filename' must be a file URL object, file URL string, or absolute path string.");
                        }
                        if (/^https?:/.test(value)) {
                            throw new TypeError('createRequire() cannot take an http(s) URL (' + value + '): use import() for remote modules.');
                        }
                        if (value.indexOf('file:') === 0) {
                            value = decodeURIComponent(value.replace(/^file:\/\/(localhost)?/, '').replace(/[?#].*$/, '')).replace(/^\/([A-Za-z]:)/, '$1');
                        }
                        return makeRequire(value);
                    },
                };
                Object.defineProperty(globalThis, '__nsModuleBuiltin', { value: Object.freeze(nsModule), enumerable: false });
            }

            // Top-level CJS globals for scripts executed outside a factory wrapper
            // (e.g., when the host calls runtime_runscript directly with a CJS file).
            if (typeof globalThis.module === 'undefined') {
                var _topMod = { id: '<main>', exports: {} };
                Object.defineProperty(globalThis, 'module',  { value: _topMod, writable: true, configurable: true });
                Object.defineProperty(globalThis, 'exports', { value: _topMod.exports, writable: true, configurable: true });
            }

            // Provide __dirname / __filename globals for webpack target:'node' bundles.
            // webpack leaves these undefined when building for node (expects Node.js to
            // provide them via its module wrapper).  Our runtime is not Node.js, so we
            // supply the app directory as a reasonable fallback value.  Modules that need
            // the exact source path will still work because @nativescript/core/globals only
            // stores the value on `global.__dirname` for downstream use.
            if (typeof globalThis.__dirname === 'undefined') {
                var _appRoot2 = (globalThis.__nsAppRoot || '').replace(/[\\/]+$/, '');
                var _appDir = _appRoot2 + '\\app';
                Object.defineProperty(globalThis, '__dirname',  { value: _appDir, writable: true, configurable: true });
                Object.defineProperty(globalThis, '__filename', { value: _appDir + '\\bundle.js', writable: true, configurable: true });
            }
        })();
        
        (function () {
            if (typeof WeakRef === 'undefined') return;
            WeakRef.prototype.get = WeakRef.prototype.deref;
            WeakRef.prototype.__hasWarnedAboutClear = false;
            WeakRef.prototype.clear = function () {
                if (WeakRef.prototype.__hasWarnedAboutClear) return;
                WeakRef.prototype.__hasWarnedAboutClear = true;
                console.warn('WeakRef.clear() is non-standard and has been deprecated. It does nothing and the call can be safely removed.');
            };
        })();
        "#;

/// Calls the .NET bridge with a JSON request string and returns the JSON
/// response string.  Throws a JS error if the host is not initialised or the
/// call fails at the hosting layer (application-level errors are returned as
/// `{"error":"…"}` inside the JSON, matching the bridge contract).
pub(crate) fn handle_dotnet_invoke(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsDotNetInvoke: expected a JSON string argument");
        return;
    }
    let Some(json_v) = args.get(0).to_string(scope) else {
        throw_js_error(scope, "__nsDotNetInvoke: argument must be a string");
        return;
    };
    let json = json_v.to_rust_string_lossy(scope);
    match crate::dotnet::call_dotnet(&json) {
        Ok(result) => {
            if let Some(s) = v8::String::new(scope, &result) {
                retval.set(s.into());
            }
        }
        Err(e) => throw_js_error(scope, &e),
    }
}

/// Binary-protocol bridge: builds a compact request packet from structured V8
/// arguments, calls the C# InvokeBinary entry point, and converts the binary
/// response directly into a V8 value — no JSON.stringify / JSON.parse on either
/// side.
///
/// Argument convention (matches the JS _invoke wrapper):
///   args[0]  handle: i32 (≥0) or null/undefined (−1 ⇒ static)
///   args[1]  typeName: string  (empty for instance calls)
///   args[2]  assembly: string  (empty for instance calls)
///   args[3]  method:   string
///   args[4…] method arguments as raw JS values
pub(crate) fn handle_dotnet_invoke_binary(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 4 {
        throw_js_error(
            scope,
            "__nsDotNetInvokeBin: expected (handle, typeName, assembly, method, ...args)",
        );
        return;
    }

    let handle_v8 = args.get(0);
    let handle: i32 = if handle_v8.is_null_or_undefined() {
        -1
    } else if let Ok(n) = v8::Local::<v8::Integer>::try_from(handle_v8) {
        n.value() as i32
    } else if let Ok(n) = v8::Local::<v8::Number>::try_from(handle_v8) {
        n.value() as i32
    } else {
        -1
    };

    let type_name = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let assembly = args
        .get(2)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let method = args
        .get(3)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let mut req: Vec<u8> = Vec::with_capacity(64);

    // Determine opcode.
    let op: u8 = if handle >= 0 {
        match method.as_str() {
            "__release" => 0x04,
            "__members__" => 0x05,
            _ => 0x01,
        }
    } else {
        match method.as_str() {
            "__members__" => 0x06,
            ".ctor" => 0x03,
            _ => 0x02,
        }
    };

    req.push(op);

    match op {
        0x01 | 0x04 | 0x05 => req.extend_from_slice(&handle.to_le_bytes()),
        _ => {
            bin_write_str16(&mut req, type_name.as_bytes());
            bin_write_str16(&mut req, assembly.as_bytes());
        }
    }

    if op == 0x01 || op == 0x02 {
        bin_write_str16(&mut req, method.as_bytes());
    }

    if matches!(op, 0x01 | 0x02 | 0x03) {
        let arg_count = (args.length() - 4).max(0) as u8;
        req.push(arg_count);
        for i in 4..4 + arg_count as i32 {
            bin_write_v8_arg(&mut req, scope, args.get(i));
        }
    }

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => match bin_read_response(scope, &response) {
            Ok(v8_val) => retval.set(v8_val),
            Err(e) => throw_js_error(scope, &e),
        },
        Err(e) => throw_js_error(scope, &e),
    }
}

/// Called by the Rust runtime (via stored function pointer) when a managed .NET
/// delegate fires.  Runs on the V8/JS isolate thread — same pattern as the
/// existing WinRT JsDelegate COM bridge.
///
/// Wire format for args_ptr: [count: u8] [tagged values...]  (response-tag scheme)
pub(crate) unsafe extern "C" fn invoke_dotnet_js_callback(
    callback_id: i32,
    args_ptr: *const u8,
    args_len: i32,
    _resp_ptr: *mut *mut u8,
    _resp_len: *mut i32,
) {
    let isolate_ptr = crate::DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return;
    }

    let isolate: &mut v8::Isolate = &mut *isolate_ptr;
    v8::scope!(scope, isolate);
    let ctx_global = match scope.get_slot::<v8::Global<v8::Context>>() {
        Some(g) => g.clone(),
        None => return,
    };
    let context = v8::Local::new(scope, &ctx_global);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(tc, scope);

    let func_global = crate::DOTNET_JS_CALLBACKS.with(|m| m.borrow().get(&callback_id).cloned());
    let Some(func_global) = func_global else {
        return;
    };
    let func = v8::Local::new(tc, &func_global);
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();

    let args_slice = if args_len > 0 {
        std::slice::from_raw_parts(args_ptr, args_len as usize)
    } else {
        &[]
    };
    let js_args = parse_dotnet_callback_args(tc, args_slice);

    let result_val = func.call(tc, recv, &js_args);
    if tc.has_caught() {
        if let Some(ex) = tc.exception() {
            let msg = ex.to_rust_string_lossy(tc);
            crate::store_last_js_error(msg);
        }
        tc.reset();
    }
    // Serialize the returned value (if any) into the binary response format
    // and return it to managed via resp_ptr/resp_len. Managed will call
    // Bridge.Free (Marshal.FreeHGlobal) to release this memory, so allocate
    // with the Windows LocalAlloc/LMEM_FIXED allocator for compatibility.
    let mut out_buf: Vec<u8> = Vec::new();
    match result_val {
        Some(rv) => {
            // Convert the returned V8 value into a single tagged binary value.
            bin_write_v8_value(&mut out_buf, tc, rv);
        }
        None => {
            // No return value: null tag.
            out_buf.push(0x00u8);
        }
    }

    if out_buf.is_empty() {
        // No response: leave resp pointers null / zero.
        if !_resp_ptr.is_null() {
            *_resp_ptr = std::ptr::null_mut();
        }
        if !_resp_len.is_null() {
            *_resp_len = 0;
        }
    } else {
        // Allocate memory using LocalAlloc so managed Marshal.FreeHGlobal can free it.
        let size = out_buf.len();
        unsafe {
            let p = LocalAlloc(LMEM_FIXED, size);
            if p.is_null() {
                if !_resp_ptr.is_null() {
                    *_resp_ptr = std::ptr::null_mut();
                }
                if !_resp_len.is_null() {
                    *_resp_len = 0;
                }
            } else {
                let dest = p as *mut u8;
                std::ptr::copy_nonoverlapping(out_buf.as_ptr(), dest, size);
                if !_resp_ptr.is_null() {
                    *_resp_ptr = dest;
                }
                if !_resp_len.is_null() {
                    *_resp_len = size as i32;
                }
            }
        }
    }
    // If this callback was registered as a one-shot, remove it now to avoid leaks.
    crate::DOTNET_ONESHOT_JS_CALLBACKS.with(|s| {
        let mut set = s.borrow_mut();
        if set.contains(&callback_id) {
            // Remove the stored JS function and clear the oneshot marker.
            crate::DOTNET_JS_CALLBACKS.with(|m| {
                m.borrow_mut().remove(&callback_id);
            });
            set.remove(&callback_id);
        }
    });

    // Managed delegates can fire from anywhere, including XAML's render walk —
    // same re-entrancy contract as js_delegate_run: defer the drain when possible.
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }
}

fn parse_dotnet_callback_args<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Vec<v8::Local<'s, v8::Value>> {
    if bytes.is_empty() {
        return vec![];
    }
    let count = bytes[0] as usize;
    let mut pos = 1usize;
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        if pos >= bytes.len() {
            break;
        }
        match bin_read_value(scope, bytes, &mut pos) {
            Ok(v) => result.push(v),
            Err(_) => break,
        }
    }
    result
}

/// Registers a JS function as a typed .NET delegate and returns the managed
/// handle.  The handle can be passed to any .NET method that accepts a delegate.
///
/// Args: args[0] = typeName (string, "" → System.Action), args[1] = function
pub(crate) fn handle_dotnet_create_delegate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        throw_js_error(
            scope,
            "__nsDotNetCreateDelegate(typeName, fn): expected 2 arguments",
        );
        return;
    }

    let type_name = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let Ok(func) = v8::Local::<v8::Function>::try_from(args.get(1)) else {
        throw_js_error(
            scope,
            "__nsDotNetCreateDelegate: second argument must be a function",
        );
        return;
    };

    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::DOTNET_JS_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, v8::Global::new(scope, func));
    });

    // opcode 0x09 | type_name (str16) | callback_id (i32)
    let mut req: Vec<u8> = Vec::with_capacity(32);
    req.push(0x09);
    bin_write_str16(&mut req, type_name.as_bytes());
    req.extend_from_slice(&cb_id.to_le_bytes());

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => match bin_read_response(scope, &response) {
            Ok(v) => retval.set(v),
            Err(e) => throw_js_error(scope, &e),
        },
        Err(e) => throw_js_error(scope, &e),
    }
}

/// Reads a JS array of strings into a `Vec<String>`. Returns `None` on a non-array or on any
/// element that fails to stringify (mirrors the pattern already used by `handle_win32_bind_fast`).
fn read_js_string_array(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> Option<Vec<String>> {
    let arr = v8::Local::<v8::Array>::try_from(value).ok()?;
    let mut out = Vec::with_capacity(arr.length() as usize);
    for i in 0..arr.length() {
        let v = arr.get_index(scope, i)?;
        out.push(value_to_string(scope, v)?);
    }
    Some(out)
}

/// Create a managed subclass instance backed by JS overrides.
/// Args: args[0] = assembly (string or ''), args[1] = typeName (string),
///       args[2] = interfaceNames (string[]), args[3] = memberNames (string[]),
///       args[4] = function (callable to receive invocations)
pub(crate) fn handle_dotnet_create_js_subclass(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 5 {
        throw_js_error(
            scope,
            "__nsDotNetCreateJsSubclass(assembly, typeName, interfaceNames, memberNames, fn): expected 5 arguments",
        );
        return;
    }

    let assembly = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let type_name = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();

    let Some(interface_names) = read_js_string_array(scope, args.get(2)) else {
        throw_js_error(
            scope,
            "__nsDotNetCreateJsSubclass: third argument must be an array of interface name strings",
        );
        return;
    };

    let Some(member_names) = read_js_string_array(scope, args.get(3)) else {
        throw_js_error(
            scope,
            "__nsDotNetCreateJsSubclass: fourth argument must be an array of member name strings",
        );
        return;
    };

    // Accept a function as the callback.
    let cb_val = args.get(4);
    let Ok(cb_fn) = v8::Local::<v8::Function>::try_from(cb_val) else {
        throw_js_error(
            scope,
            "__nsDotNetCreateJsSubclass: fifth argument must be a function",
        );
        return;
    };

    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::DOTNET_JS_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, v8::Global::new(scope, cb_fn));
    });

    let mut req: Vec<u8> = Vec::with_capacity(64);
    req.push(0x0A);
    bin_write_str16(&mut req, assembly.as_bytes());
    bin_write_str16(&mut req, type_name.as_bytes());
    req.extend_from_slice(&(interface_names.len() as i32).to_le_bytes());
    for name in &interface_names {
        bin_write_str16(&mut req, name.as_bytes());
    }
    req.extend_from_slice(&(member_names.len() as i32).to_le_bytes());
    for name in &member_names {
        bin_write_str16(&mut req, name.as_bytes());
    }
    req.extend_from_slice(&cb_id.to_le_bytes());

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => match bin_read_response(scope, &response) {
            Ok(v) => retval.set(v),
            Err(e) => throw_js_error(scope, &e),
        },
        Err(e) => throw_js_error(scope, &e),
    }
}


pub(crate) fn handle_dotnet_await_task(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    if args.length() < 3 {
        throw_js_error(
            scope,
            "__nsDotNetAwaitTask(handleId, resolve, reject): expected 3 arguments",
        );
        return;
    }

    let handle_id: i32 = if let Ok(n) = v8::Local::<v8::Integer>::try_from(args.get(0)) {
        n.value() as i32
    } else if let Ok(n) = v8::Local::<v8::Number>::try_from(args.get(0)) {
        n.value() as i32
    } else {
        throw_js_error(
            scope,
            "__nsDotNetAwaitTask: first argument must be a handle id (integer)",
        );
        return;
    };

    let Ok(resolve_fn) = v8::Local::<v8::Function>::try_from(args.get(1)) else {
        throw_js_error(
            scope,
            "__nsDotNetAwaitTask: second argument must be a resolve function",
        );
        return;
    };
    let Ok(reject_fn) = v8::Local::<v8::Function>::try_from(args.get(2)) else {
        throw_js_error(
            scope,
            "__nsDotNetAwaitTask: third argument must be a reject function",
        );
        return;
    };

    let resolve_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let reject_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    crate::DOTNET_JS_CALLBACKS.with(|m| {
        let mut map = m.borrow_mut();
        map.insert(resolve_id, v8::Global::new(scope, resolve_fn));
        map.insert(reject_id, v8::Global::new(scope, reject_fn));
    });

    // Binary instance call: 0x01 | handle(i32) | "__dotnet_await__"(str16) | 2 | i32 resolveId | i32 rejectId
    let mut req: Vec<u8> = Vec::with_capacity(32);
    req.push(0x01u8);
    req.extend_from_slice(&handle_id.to_le_bytes());
    bin_write_str16(&mut req, b"__dotnet_await__");
    req.push(2u8);
    req.push(0x03u8);
    req.extend_from_slice(&resolve_id.to_le_bytes());
    req.push(0x03u8);
    req.extend_from_slice(&reject_id.to_le_bytes());

    if let Err(e) = crate::dotnet::call_dotnet_binary(&req) {
        crate::DOTNET_JS_CALLBACKS.with(|m| {
            let mut map = m.borrow_mut();
            map.remove(&resolve_id);
            map.remove(&reject_id);
        });
        throw_js_error(scope, &e);
    }
}

fn bin_write_v8_arg(
    buf: &mut Vec<u8>,
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) {
    if arg.is_null_or_undefined() {
        buf.push(0x00);
        return;
    }

    if arg.is_boolean() {
        buf.push(if arg.is_true() { 0x02 } else { 0x01 });
        return;
    }

    // Integer (Smi) check before general Number to preserve i32 tag where possible.
    if let Ok(n) = v8::Local::<v8::Integer>::try_from(arg) {
        let v = n.value();
        if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
            buf.push(0x03);
            buf.extend_from_slice(&(v as i32).to_le_bytes());
            return;
        }
    }

    if let Ok(n) = v8::Local::<v8::Number>::try_from(arg) {
        buf.push(0x04);
        buf.extend_from_slice(&n.value().to_bits().to_le_bytes());
        return;
    }

    if arg.is_string() {
        if let Some(s) = arg.to_string(scope) {
            let bytes = s.to_rust_string_lossy(scope).into_bytes();
            buf.push(0x05);
            bin_write_str16(buf, &bytes);
            return;
        }
    }

    if arg.is_object() {
        if let Some(obj) = arg.to_object(scope) {
            // Dotnet bridge handle: { __handle: i32 }
            if let Some(key) = v8::String::new(scope, "__handle") {
                if let Some(hval) = obj.get(scope, key.into()) {
                    if let Ok(n) = v8::Local::<v8::Integer>::try_from(hval) {
                        buf.push(0x06);
                        buf.extend_from_slice(&(n.value() as i32).to_le_bytes());
                        return;
                    }
                    if let Ok(n) = v8::Local::<v8::Number>::try_from(hval) {
                        buf.push(0x06);
                        buf.extend_from_slice(&(n.value() as i32).to_le_bytes());
                        return;
                    }
                }
            }
            // WinRT COM pointer: BigInt exposed as __native_ptr on WinRT proxy objects.
            // Encoded as tag 0x0A + 8-byte little-endian u64 so C# can call
            // Marshal.GetObjectForIUnknown to obtain a managed RCW.
            if let Some(key) = v8::String::new(scope, "__native_ptr") {
                if let Some(pval) = obj.get(scope, key.into()) {
                    if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(pval) {
                        let (ptr, _) = bi.u64_value();
                        if ptr != 0 {
                            // Ensure ownership semantics: AddRef the raw COM pointer
                            // before emitting it to managed code so the managed side
                            // can safely wrap and Release it later.
                            unsafe {
                                let unknown = ManuallyDrop::new(IUnknown::from_raw(ptr as *mut _));
                                let vtable = unknown.vtable();
                                ((*vtable).AddRef)(unknown.as_raw());
                            }
                            buf.push(0x0A);
                            buf.extend_from_slice(&ptr.to_le_bytes());
                            return;
                        }
                    }
                }
            }
        }

        // Fallback: stringify object
        if let Some(sv) = arg.to_string(scope) {
            let bytes = sv.to_rust_string_lossy(scope).into_bytes();
            buf.push(0x05);
            bin_write_str16(buf, &bytes);
            return;
        }
    }

    // Final fallback: null
    buf.push(0x00);
}


fn bin_write_v8_value(
    buf: &mut Vec<u8>,
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) {
    if arg.is_null_or_undefined() {
        buf.push(0x00);
        return;
    }
    if arg.is_boolean() {
        buf.push(if arg.is_true() { 0x02 } else { 0x01 });
        return;
    }

    // Integer before general number
    if let Ok(n) = v8::Local::<v8::Integer>::try_from(arg) {
        let v = n.value();
        if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
            buf.push(0x03);
            buf.extend_from_slice(&(v as i32).to_le_bytes());
            return;
        }
    }
    if let Ok(n) = v8::Local::<v8::Number>::try_from(arg) {
        buf.push(0x04);
        buf.extend_from_slice(&n.value().to_bits().to_le_bytes());
        return;
    }

    if arg.is_string() {
        if let Some(s) = arg.to_string(scope) {
            let bytes = s.to_rust_string_lossy(scope).into_bytes();
            buf.push(0x05);
            bin_write_str32(buf, &bytes);
            return;
        }
    }

    if arg.is_array() {
        if let Some(arr) = v8::Local::<v8::Array>::try_from(arg).ok() {
            let len = arr.length() as u32;
            buf.push(0x07);
            buf.extend_from_slice(&len.to_le_bytes());
            for i in 0..len {
                if let Some(it) = arr.get_index(scope, i) {
                    bin_write_v8_value(buf, scope, it);
                } else {
                    buf.push(0x00);
                }
            }
            return;
        }
    }

    if arg.is_object() {
        if let Some(obj) = arg.to_object(scope) {
            // Dotnet bridge handle: { __handle: i32 }
            if let Some(key) = v8::String::new(scope, "__handle") {
                if let Some(hval) = obj.get(scope, key.into()) {
                    if let Ok(n) = v8::Local::<v8::Integer>::try_from(hval) {
                        buf.push(0x06);
                        buf.extend_from_slice(&(n.value() as i32).to_le_bytes());
                        return;
                    }
                    if let Ok(n) = v8::Local::<v8::Number>::try_from(hval) {
                        buf.push(0x06);
                        buf.extend_from_slice(&(n.value() as i32).to_le_bytes());
                        return;
                    }
                }
            }
            // WinRT COM pointer: __native_ptr BigInt -> 0x0A + 8-byte u64
            if let Some(key) = v8::String::new(scope, "__native_ptr") {
                if let Some(pval) = obj.get(scope, key.into()) {
                    if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(pval) {
                        let (ptr, _) = bi.u64_value();
                        if ptr != 0 {
                            // Emit native pointer tag (0x0A) + 8-byte pointer value.
                            buf.push(0x0A);
                            buf.extend_from_slice(&ptr.to_le_bytes());
                            return;
                        }
                    }
                }
            }
            // fallback: stringify
            if let Some(sv) = arg.to_string(scope) {
                let bytes = sv.to_rust_string_lossy(scope).into_bytes();
                buf.push(0x05);
                bin_write_str32(buf, &bytes);
                return;
            }
        }
    }

    // final fallback: null
    buf.push(0x00);
}

fn bin_read_response<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Result<v8::Local<'s, v8::Value>, String> {
    if bytes.is_empty() {
        return Ok(v8::null(scope).into());
    }
    let mut pos = 0usize;
    bin_read_value(scope, bytes, &mut pos)
}

fn bin_read_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
    pos: &mut usize,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let tag = *bytes.get(*pos).ok_or("response truncated")?;
    *pos += 1;

    match tag {
        0x00 => Ok(v8::null(scope).into()),
        0x01 => Ok(v8::Boolean::new(scope, false).into()),
        0x02 => Ok(v8::Boolean::new(scope, true).into()),

        0x03 => {
            let v = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "i32 read")?);
            *pos += 4;
            Ok(v8::Integer::new(scope, v).into())
        }

        0x04 => {
            let bits =
                u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().map_err(|_| "f64 read")?);
            *pos += 8;
            Ok(v8::Number::new(scope, f64::from_bits(bits)).into())
        }

        0x05 => {
            // string: u32 len + utf8
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "str len")?)
                as usize;
            *pos += 4;
            let s = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
            *pos += len;
            Ok(v8::String::new(scope, s)
                .map(Into::into)
                .unwrap_or_else(|| v8::null(scope).into()))
        }

        0x06 | 0x0C => {
            // handle → JS object {__handle, __type} ; 0x0C adds __isTask:true
            let id = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "handle id")?);
            *pos += 4;
            let type_len =
                u16::from_le_bytes(bytes[*pos..*pos + 2].try_into().map_err(|_| "type len")?)
                    as usize;
            *pos += 2;
            let type_name =
                std::str::from_utf8(&bytes[*pos..*pos + type_len]).map_err(|_| "utf8")?;
            *pos += type_len;

            let obj = v8::Object::new(scope);
            let hk = v8::String::new(scope, "__handle").ok_or("v8 str")?;
            let tk = v8::String::new(scope, "__type").ok_or("v8 str")?;
            let tv = v8::String::new(scope, type_name).ok_or("v8 str")?;
            obj.set(scope, hk.into(), v8::Integer::new(scope, id).into());
            obj.set(scope, tk.into(), tv.into());
            if tag == 0x0C {
                let ik = v8::String::new(scope, "__isTask").ok_or("v8 str")?;
                obj.set(scope, ik.into(), v8::Boolean::new(scope, true).into());
            }

            // Optional native pointer included by the bridge.
            // Read a presence flag byte, then a little-endian i64 pointer.
            if bytes.len() - *pos >= 1 {
                let flag = bytes[*pos];
                *pos += 1;
                if flag != 0 {
                    if bytes.len() - *pos >= 8 {
                        let raw = i64::from_le_bytes(
                            bytes[*pos..*pos + 8].try_into().map_err(|_| "i64 read")?,
                        );
                        *pos += 8;
                        let nk = v8::String::new(scope, "__native_ptr").ok_or("v8 str")?;
                        if raw >= 0 {
                            obj.set(
                                scope,
                                nk.into(),
                                v8::BigInt::new_from_u64(scope, raw as u64).into(),
                            );
                        } else {
                            obj.set(
                                scope,
                                nk.into(),
                                v8::BigInt::new_from_i64(scope, raw).into(),
                            );
                        }
                    }
                }
            }
            Ok(obj.into())
        }

        0x07 => {
            // array: u32 count + N items
            let count =
                u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "arr count")?)
                    as usize;
            *pos += 4;
            let arr = v8::Array::new(scope, count as i32);
            for i in 0..count {
                let item = bin_read_value(scope, bytes, pos)?;
                arr.set_index(scope, i as u32, item);
            }
            Ok(arr.into())
        }

        0x08 => {
            // members: 8 string arrays — methods/props/staticMethods/staticProps + readonly/writeonly variants
            let obj = v8::Object::new(scope);
            for key in [
                "methods",
                "properties",
                "staticMethods",
                "staticProperties",
                "readonlyProperties",
                "readonlyStaticProperties",
                "writeonlyProperties",
                "writeonlyStaticProperties",
            ] {
                let count = u16::from_le_bytes(
                    bytes[*pos..*pos + 2]
                        .try_into()
                        .map_err(|_| "member count")?,
                ) as usize;
                *pos += 2;
                let arr = v8::Array::new(scope, count as i32);
                for i in 0..count {
                    let len = u16::from_le_bytes(
                        bytes[*pos..*pos + 2].try_into().map_err(|_| "str len")?,
                    ) as usize;
                    *pos += 2;
                    let s = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
                    *pos += len;
                    if let Some(sv) = v8::String::new(scope, s) {
                        arr.set_index(scope, i as u32, sv.into());
                    }
                }
                let k = v8::String::new(scope, key).ok_or("v8 str")?;
                obj.set(scope, k.into(), arr.into());
            }
            Ok(obj.into())
        }

        0xFF => {
            // error: u32 len + utf8 message
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "err len")?)
                as usize;
            *pos += 4;
            let msg = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
            Err(msg.to_string())
        }

        t => Err(format!("Unknown binary response tag 0x{t:02X}")),
    }
}

/// Returns a JSON array of exported function names for the given DLL, e.g.
/// `["MessageBoxW","CreateWindowExW",…]`.  Used by `NSWinRT.win32.import()`.
pub(crate) fn handle_win32_exports(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let Some(dll_v) = args.get(0).to_string(scope) else {
        return;
    };
    let dll = dll_v.to_rust_string_lossy(scope);
    let names = match crate::win32::list_exports(&dll) {
        Ok(v) => v,
        Err(_) => vec![],
    };
    let json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string());
    if let Some(s) = v8::String::new(scope, &json) {
        retval.set(s.into());
    }
}

/// Blocks the calling thread until the next DWM VSync (monitor refresh), then
/// returns the elapsed milliseconds since process start as a `f64`.
///
/// This is the Windows equivalent of Android's `Choreographer` / iOS's
/// `CADisplayLink`: it yields exactly at the display's refresh boundary,
/// giving requestAnimationFrame perfect frame timing at any Hz (60/120/144/240).
///
/// On headless systems where DWM composition is disabled, `DwmFlush` returns
/// immediately with an error, so this call is non-blocking in that case.
pub(crate) fn handle_dwm_flush(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    use windows::Win32::Graphics::Dwm::DwmFlush;
    // Best-effort: ignore DWM_E_COMPOSITIONDISABLED on headless systems.
    let _ = unsafe { DwmFlush() };
    let ts = crate::globals::time::PROCESS_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as f64
        / 1_000_000.0;
    retval.set_double(ts);
}

pub(crate) fn handle_tns_uptime(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let ts = crate::globals::time::PROCESS_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as f64
        / 1_000_000.0;
    retval.set_double(ts);
}

pub(crate) fn handle_ns_uuid(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    match windows::core::GUID::new() {
        Ok(g) => {
            let s = format!(
                "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                g.data1,
                g.data2,
                g.data3,
                g.data4[0],
                g.data4[1],
                g.data4[2],
                g.data4[3],
                g.data4[4],
                g.data4[5],
                g.data4[6],
                g.data4[7],
            );
            if let Some(v) = v8::String::new(scope, &s) {
                retval.set(v.into());
            }
        }
        Err(_) => retval.set(v8::undefined(scope).into()),
    }
}

pub(crate) fn handle_is_ui_thread(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let is_ui = crate::ui_dispatcher::is_ui_thread();
    retval.set_bool(is_ui);
}

pub(crate) fn handle_thread_info(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let os_tid = crate::ui_dispatcher::get_ui_thread_os_tid();
    let rust_tid = crate::ui_dispatcher::get_ui_thread_rust_tid();
    let is_ui = crate::ui_dispatcher::is_ui_thread();

    let obj = v8::Object::new(scope);
    // osTid: number | null
    if let Some(k) = v8::String::new(scope, "osTid") {
        if let Some(t) = os_tid {
            obj.set(scope, k.into(), v8::Number::new(scope, t as f64).into());
        } else {
            obj.set(scope, k.into(), v8::null(scope).into());
        }
    }
    // rustTid: string | null
    if let Some(k) = v8::String::new(scope, "rustTid") {
        if let Some(s) = rust_tid {
            if let Some(v) = v8::String::new(scope, &s) {
                obj.set(scope, k.into(), v.into());
            }
        } else {
            obj.set(scope, k.into(), v8::null(scope).into());
        }
    }
    // isUiThread: bool
    if let Some(k) = v8::String::new(scope, "isUiThread") {
        obj.set(scope, k.into(), v8::Boolean::new(scope, is_ui).into());
    }

    retval.set(obj.into());
}

pub(crate) fn handle_get_last_js_error(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if let Some(err) = crate::get_last_js_error() {
        if let Some(s) = v8::String::new(scope, &err) {
            retval.set(s.into());
            return;
        }
    }
    retval.set(v8::null(scope).into());
}

// __nsTypedValue(typeName, value) — box a JS value as a concrete WinRT IPropertyValue.
//   Useful for passing typed primitives to Object/IInspectable parameters and for
//   method overload disambiguation.
//
// __nsCreateReference(typeName, value) — explicit IReference<T> boxing (alias).
//   PropertyValue::Create* produces an object implementing both IPropertyValue AND
//   IReference<T>, so both helpers share the same Rust implementation.
//
// Supported type names (a "Windows.Foundation." prefix is also accepted, e.g.
// "Windows.Foundation.TimeSpan"):
//   "Single" | "Double" | "Int32" | "UInt32" | "Int64" | "UInt64" | "Int16" |
//   "UInt16" | "Byte"/"UInt8" | "Char16" | "Boolean" | "String" |
//   "TimeSpan" (accepts ms number or {Duration:ticks}) |
//   "DateTime" (accepts ms-since-epoch or {UniversalTime:ticks}) |
//   "Guid" (accepts "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" string)

fn box_and_return(
    scope: &mut v8::PinScope<'_, '_>,
    type_name: &str,
    value: v8::Local<v8::Value>,
    mut retval: v8::ReturnValue,
) {
    let Some(nv) = crate::value::box_as_typed_value(scope, value, type_name) else {
        retval.set(v8::null(scope).into());
        return;
    };
    let ptr = unsafe { nv.pointer };
    let ext = v8::External::new(scope, ptr);
    let obj = v8::Object::new(scope);
    if let Some(k) = v8::String::new(scope, "handle") {
        obj.set(scope, k.into(), ext.into());
    }
    retval.set(obj.into());
}

pub(crate) fn handle_typed_value(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        crate::throw_js_error(scope, "__nsTypedValue(typeName, value) expects 2 arguments");
        return;
    }
    let Some(tn) = args.get(0).to_string(scope) else {
        crate::throw_js_error(scope, "__nsTypedValue: typeName must be a string");
        return;
    };
    let type_name = tn.to_rust_string_lossy(scope);
    box_and_return(scope, type_name.trim(), args.get(1), retval);
}

pub(crate) fn handle_create_reference(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    retval: v8::ReturnValue,
) {
    if args.length() < 2 {
        crate::throw_js_error(
            scope,
            "__nsCreateReference(typeName, value) expects 2 arguments",
        );
        return;
    }
    let Some(tn) = args.get(0).to_string(scope) else {
        crate::throw_js_error(scope, "__nsCreateReference: typeName must be a string");
        return;
    };
    let type_name = tn.to_rust_string_lossy(scope);
    box_and_return(scope, type_name.trim(), args.get(1), retval);
}

/// Fast typed Win32 dispatch — no JSON round-trip.
/// Args: (dll, fn_name, retType, argType₀, argVal₀, argType₁, argVal₁, …)
/// Returns the result as a typed V8 value (number, bool, or null for void).
pub(crate) fn handle_win32_call_raw(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 3 {
        throw_js_error(scope, "__nsWin32CallRaw: expected (dll, fn, retType, ...)");
        return;
    }
    let Some(dll_v) = args.get(0).to_string(scope) else {
        throw_js_error(scope, "__nsWin32CallRaw: dll must be a string");
        return;
    };
    let Some(fn_v) = args.get(1).to_string(scope) else {
        throw_js_error(scope, "__nsWin32CallRaw: fn must be a string");
        return;
    };
    let Some(ret_v) = args.get(2).to_string(scope) else {
        throw_js_error(scope, "__nsWin32CallRaw: retType must be a string");
        return;
    };
    let dll = dll_v.to_rust_string_lossy(scope);
    let fn_name = fn_v.to_rust_string_lossy(scope);
    let ret_type = ret_v.to_rust_string_lossy(scope);

    let pair_count = ((args.length() - 3) / 2) as usize;
    let mut type_strs: Vec<String> = Vec::with_capacity(pair_count);
    let mut raw_vals: Vec<crate::win32::RawArgValue> = Vec::with_capacity(pair_count);

    let mut i: i32 = 3;
    while i + 1 < args.length() {
        let Some(ty_v) = args.get(i).to_string(scope) else {
            break;
        };
        let ty_str = ty_v.to_rust_string_lossy(scope);
        let v8_val = args.get(i + 1);
        let raw_val = if ty_str == "wstr" || ty_str == "str" {
            let s = v8_val
                .to_string(scope)
                .map(|sv| sv.to_rust_string_lossy(scope))
                .unwrap_or_default();
            crate::win32::RawArgValue::Str(s)
        } else {
            let n = v8_val.number_value(scope).unwrap_or(0.0);
            crate::win32::RawArgValue::Number(n)
        };
        type_strs.push(ty_str);
        raw_vals.push(raw_val);
        i += 2;
    }

    let arg_pairs: Vec<(&str, crate::win32::RawArgValue)> =
        type_strs.iter().map(|s| s.as_str()).zip(raw_vals).collect();

    match crate::win32::call_win32_direct(&dll, &fn_name, &ret_type, &arg_pairs) {
        Ok(result) => match result {
            crate::win32::Win32Result::Null => retval.set(v8::null(scope).into()),
            crate::win32::Win32Result::Bool(b) => retval.set_bool(b),
            crate::win32::Win32Result::I64(v) => retval.set_double(v as f64),
            crate::win32::Win32Result::U64(v) => retval.set_double(v as f64),
            crate::win32::Win32Result::F64(v) => retval.set_double(v),
        },
        Err(e) => throw_js_error(scope, &e),
    }
}

/// `__nsWin32BindFast(dll, fn, retType, [argTypes])` — build a V8 Fast API
/// capable bound Function for a purely numeric Win32 signature, or null when
/// the signature is not fast-eligible (the JS caller keeps its fallback path).
pub(crate) fn handle_win32_bind_fast(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    retval.set(v8::null(scope).into());
    if args.length() < 3 {
        return;
    }
    let Some(dll) = value_to_string(scope, args.get(0)) else {
        return;
    };
    let Some(fn_name) = value_to_string(scope, args.get(1)) else {
        return;
    };
    let Some(ret_type) = value_to_string(scope, args.get(2)) else {
        return;
    };

    let mut arg_types: Vec<String> = Vec::new();
    if args.length() >= 4 {
        let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(3)) else {
            return;
        };
        for i in 0..arr.length() {
            let Some(v) = arr.get_index(scope, i) else {
                return;
            };
            let Some(s) = value_to_string(scope, v) else {
                return;
            };
            arg_types.push(s);
        }
    }

    if let Some(f) =
        crate::win32_fast::make_fast_bound_function(scope, &dll, &fn_name, &ret_type, &arg_types)
    {
        retval.set(f.into());
    }
}

/// Calls an arbitrary Win32 function via libffi dynamic dispatch.
/// Accepts a JSON string `{dll, fn, returnType, args:[{type,value}…]}` and
/// returns `{"value": <result>}` or `{"error": "…"}`.
pub(crate) fn handle_win32_call(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    if args.length() < 1 {
        throw_js_error(scope, "__nsWin32Call: expected a JSON string argument");
        return;
    }
    let Some(json_v) = args.get(0).to_string(scope) else {
        throw_js_error(scope, "__nsWin32Call: argument must be a string");
        return;
    };
    let json = json_v.to_rust_string_lossy(scope);
    let result_json = match crate::win32::call_win32_json(&json) {
        Ok(v) => v,
        Err(e) => format!(r#"{{"error":{}}}"#, serde_json::to_string(&e).unwrap()),
    };
    if let Some(s) = v8::String::new(scope, &result_json) {
        retval.set(s.into());
    }
}

/// Installs all host-side global functions and the runtime bootstrap JS into `scope`.
pub(crate) fn init_async_helpers(
    scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>,
    app_root: &str,
) {
    let global = scope.get_current_context().global(scope);

    // One macro call per builtin replaces 4 lines of boilerplate.
    macro_rules! register {
        ($name:literal, $handler:expr) => {{
            if let (Some(n), Some(f)) = (
                v8::String::new(scope, $name),
                v8::Function::new(scope, $handler),
            ) {
                global.define_own_property(
                    scope,
                    n.into(),
                    f.into(),
                    v8::PropertyAttribute::READ_ONLY,
                );
            }
        }};
    }

    register!("__nsHostWaitForAsync", handle_host_wait_for_async);
    register!("__nsEnqueueMicrotask", handle_enqueue_microtask);
    register!("__nsPointerKey", handle_pointer_key);
    register!("__nsBufferToPointer", handle_buffer_to_pointer);
    register!("__nsArrayBufferFromBuffer", handle_array_buffer_from_buffer);
    register!("__nsProxyWriteTextFile", handle_proxy_write_text_file);
    register!("__nsProxyCompileProject", handle_proxy_compile_project);
    register!("__nsProxyRegisterManifest", handle_proxy_register_manifest);
    register!("__nsProxyListManifests", handle_proxy_list_manifests);
    register!("__nsProxyAutoCapture", handle_proxy_auto_capture);
    register!("__nsReadTextFile", handle_read_text_file);
    register!("__nsRegisterWinmd", handle_register_winmd);
    register!("__nsResolveModulePath", handle_resolve_module_path);
    register!("__nsDescribeWinRTType", handle_describe_winrt_type);
    register!("__nsWorkerCreateThreaded", handle_worker_create_threaded);
    register!("__nsWorkerPostMessage", handle_worker_post_message);
    register!("__nsWorkerPollMessages", handle_worker_poll_messages);
    register!("__nsWorkerTerminate", handle_worker_terminate);
    register!(
        "__nsWorkerPollMessagesBlocking",
        handle_worker_poll_messages_blocking
    );
    register!("__nsLiveSyncCopyFile", handle_livesync_copy_file);
    register!("__nsAsDelegate", crate::handle_as_delegate);
    register!("__nsMakeItemsSource", crate::handle_make_items_source);
    register!("__nsExtendItemsSource", crate::handle_extend_items_source);
    register!("__nsInsertItemsSource", crate::handle_insert_items_source);
    register!("__nsRemoveItemsSource", crate::handle_remove_items_source);
    register!("__nsUpdateItemsSource", crate::handle_update_items_source);
    register!("__nsResetItemsSource", crate::handle_reset_items_source);
    register!("__nsDotNetInvoke", handle_dotnet_invoke);
    register!("__nsDotNetInvokeBin", handle_dotnet_invoke_binary);
    register!("__nsDotNetCreateDelegate", handle_dotnet_create_delegate);
    register!(
        "__nsDotNetCreateJsSubclass",
        handle_dotnet_create_js_subclass
    );
    register!("__nsDotNetAwaitTask", handle_dotnet_await_task);
    register!("__nsRunOnUIThread", handle_run_on_ui_thread);
    register!("__nsWin32Call", handle_win32_call);
    register!("__nsWin32CallRaw", handle_win32_call_raw);
    register!("__nsWin32BindFast", handle_win32_bind_fast);
    register!("__nsWin32Exports", handle_win32_exports);
    register!("__ns__setTimeout", crate::timers::handle_ns_set_timeout);
    register!("__ns__setInterval", crate::timers::handle_ns_set_interval);
    register!("__ns__clearTimeout", crate::timers::handle_ns_clear_timeout);
    register!(
        "__ns__clearInterval",
        crate::timers::handle_ns_clear_interval
    );
    register!("__nsDwmFlush", handle_dwm_flush);
    register!("__tns_uptime", handle_tns_uptime);
    register!("__nsUUID", handle_ns_uuid);
    register!("__nsIsUiThread", handle_is_ui_thread);
    register!("__nsThreadInfo", handle_thread_info);
    register!("__nsGetLastJsError", handle_get_last_js_error);
    register!("__nsTypedValue", handle_typed_value);
    register!("__nsCreateReference", handle_create_reference);
    register!("__nsMsAppxResolve", handle_ms_appx_resolve);
    register!("__nsWebSocketOpen", crate::websocket::handle_open);
    register!("__nsWebSocketSend", crate::websocket::handle_send);
    register!("__nsWebSocketClose", crate::websocket::handle_close);
    register!("__nsModuleConfigureLoader", crate::handle_ns_module_configure_loader);
    register!("__nsModuleInvalidate", crate::handle_ns_module_invalidate);
    register!("__nsModuleLoadedUrls", crate::handle_ns_module_loaded_urls);

    // DevTools host hooks: allow JS to register domain dispatchers and
    // post events/timestamps to the DevTools server when enabled.
    register!(
        "__registerDomainDispatcher",
        handle_register_domain_dispatcher
    );
    register!("__inspectorSendEvent", handle_inspector_send_event);
    register!("__inspectorTimestamp", handle_inspector_timestamp);

    // Initialize native timers scheduler (non-blocking). This registers a
    // pump into `ASYNC_PUMP_HOOK` so blocking waits will also process timers.
    crate::timers::init();

    if let (Some(k), Some(v)) = (
        v8::String::new(scope, "__nsAppRoot"),
        v8::String::new(scope, app_root),
    ) {
        global.define_own_property(scope, k.into(), v.into(), v8::PropertyAttribute::READ_ONLY);
    }

    // This benefits host-provided timers that rely on system timer resolution.
    // Ignored on failure (e.g., sandboxed environments).
    let _ = crate::win32::call_win32_json(
        r#"{"dll":"winmm.dll","fn":"timeBeginPeriod","returnType":"u32","args":[{"type":"u32","value":1}]}"#,
    );

    // Store app root for optional .NET initialization. Actual host initialisation
    // is performed lazily on first .NET use.
    crate::dotnet::set_app_root(app_root);

    crate::globals::url::install_url_globals(scope);

    if let Some(src) = v8::String::new(scope, HELPER_SOURCE) {
        if let Some(script) = v8::Script::compile(scope, src, None) {
            script.run(scope);
        }
    }

    crate::message_port::install_message_port_runtime(scope);
    crate::worker_support::install_worker_runtime(scope);
    crate::hmr_support::install_hmr_support(scope);
    crate::livesync::install_livesync_support(scope);
    // Attempt to attach the DevTools server (no-op if built without `devtools`).
    maybe_attach_devtools(scope);
}
