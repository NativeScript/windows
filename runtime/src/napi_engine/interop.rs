//! Node-API implementation of the `interop.*` helpers exposed to JS as `NSWinRT.interop` (the
//! classic runtime exposes the same surface from `global_fns.rs`).
//!
//! The napi path never constructs a `Runtime`, so its winmd auto-scan doesn't run;
//! `scan_default_winmd_dirs` (cwd + exe dir) covers it from `ensure_winrt_initialized`, and
//! `register_winmd` / `scan_winmd_dir` add locations explicitly (WebView2, app types).

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Once;

use napi::{sys, CallContext, Env, JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue, ValueType};
use windows::core::{IUnknown, Interface};
use windows::Storage::Streams::IBuffer;
use windows::Win32::System::WinRT::IBufferByteAccess;

use crate::napi_engine::value::{
    as_unknown, buffer_data, js_to_rust_string, ptr_from_external, try_get_external_handle,
};

/// A fresh UUID string, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, generated via `CoCreateGuid`.
pub fn ns_uuid() -> String {
    match windows::core::GUID::new() {
        Ok(g) => format!(
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
        ),
        Err(_) => String::new(),
    }
}

/// Register a single third-party `.winmd` for metadata resolution.
pub fn register_winmd(path: &str) -> Result<(), String> {
    metadata::meta_data_reader::MetadataReader::register_winmd_file(path)
}

/// Register every `.winmd` in `dir` (non-recursive). Returns the count registered.
pub fn scan_winmd_dir(dir: &str) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_winmd = path
            .extension()
            .map_or(false, |ext| ext.eq_ignore_ascii_case("winmd"));
        if is_winmd {
            if let Some(p) = path.to_str() {
                if register_winmd(p).is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Scan the default locations once (cwd + the addon/executable directory) for third-party
/// `.winmd` files and the app's metadata filter, mirroring `Runtime::new` for the napi path.
pub fn scan_default_winmd_dirs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        metadata::meta_data_reader::MetadataReader::load_filter(
            [
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf())),
                std::env::current_dir().ok(),
            ]
            .into_iter()
            .flatten(),
        );
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(s) = cwd.to_str() {
                scan_winmd_dir(s);
            }
        }
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        {
            if let Some(s) = dir.to_str() {
                scan_winmd_dir(s);
            }
        }
    });
}

/// napi finalizer for the AddRef'd IBuffer kept alive behind a zero-copy ArrayBuffer.
unsafe extern "C" fn finalize_ibuffer(
    _env: sys::napi_env,
    _data: *mut c_void,
    hint: *mut c_void,
) {
    if !hint.is_null() {
        // The napi path never calls RoUninitialize, so releasing here is safe (unlike the
        // rusty_v8 runtime's COM_TEARDOWN guard, which handles isolate teardown).
        drop(Box::from_raw(hint as *mut IBuffer));
    }
}

/// Zero-copy `ArrayBuffer` aliasing a `Windows.Storage.Streams.IBuffer`'s native storage
/// (covers `IBuffer.Length`). The IBuffer is AddRef'd for the ArrayBuffer's lifetime so
/// reads/writes propagate through.
///
/// Where the host V8 forbids external ArrayBuffers (newer Node with the pointer-compression
/// sandbox), falls back to a one-time copy so the call still succeeds (write-through is then
/// lost — documented in the JS wrapper).
pub fn array_buffer_from_buffer(env: &Env, buffer: &JsUnknown) -> napi::Result<JsUnknown> {
    let reason = |m: &str| napi::Error::from_reason(m.to_string());

    // Extract the COM pointer from the proxy's `handle` external (or a direct external).
    let raw = ptr_from_external(env, buffer).or_else(|| {
        if !matches!(buffer.get_type(), Ok(ValueType::Object)) {
            return None;
        }
        let obj: napi::JsObject = unsafe { buffer.cast() };
        let handle = obj.get_named_property::<JsUnknown>("handle").ok()?;
        ptr_from_external(env, &handle)
    });
    let Some(raw) = raw else {
        return Err(reason(
            "arrayBufferFromBuffer expects a WinRT IBuffer (no COM handle)",
        ));
    };

    // Borrow → owned so both QIs share one keep-alive reference.
    let owned: IUnknown = unsafe {
        let borrowed = ManuallyDrop::new(IUnknown::from_raw(raw));
        (*borrowed).clone()
    };
    let ibuffer: IBuffer = owned
        .cast()
        .map_err(|_| reason("value does not implement Windows.Storage.Streams.IBuffer"))?;
    let byte_access: IBufferByteAccess = owned
        .cast()
        .map_err(|_| reason("IBuffer does not expose IBufferByteAccess"))?;

    let len = ibuffer.Length().unwrap_or(0) as usize;
    let data = unsafe { byte_access.Buffer() }
        .map_err(|_| reason("failed to obtain IBuffer data pointer"))?;

    if data.is_null() || len == 0 {
        let mut out: sys::napi_value = std::ptr::null_mut();
        let mut tmp: *mut c_void = std::ptr::null_mut();
        let st = unsafe { sys::napi_create_arraybuffer(env.raw(), 0, &mut tmp, &mut out) };
        if st != sys::Status::napi_ok {
            return Err(reason("failed to create empty ArrayBuffer"));
        }
        return Ok(unsafe { JsUnknown::from_raw_unchecked(env.raw(), out) });
    }

    // Zero-copy external ArrayBuffer, keeping the IBuffer alive via the finalizer.
    let keep = Box::into_raw(Box::new(ibuffer)) as *mut c_void;
    let mut out: sys::napi_value = std::ptr::null_mut();
    let status = unsafe {
        sys::napi_create_external_arraybuffer(
            env.raw(),
            data as *mut c_void,
            len,
            Some(finalize_ibuffer),
            keep,
            &mut out,
        )
    };
    if status == sys::Status::napi_ok {
        return Ok(unsafe { JsUnknown::from_raw_unchecked(env.raw(), out) });
    }

    // Fallback (external buffers unsupported): copy the bytes, release the keep-alive.
    unsafe {
        let mut copy_ptr: *mut c_void = std::ptr::null_mut();
        let mut ab: sys::napi_value = std::ptr::null_mut();
        let st = sys::napi_create_arraybuffer(env.raw(), len, &mut copy_ptr, &mut ab);
        drop(Box::from_raw(keep as *mut IBuffer));
        if st != sys::Status::napi_ok {
            return Err(reason("failed to create ArrayBuffer copy"));
        }
        std::ptr::copy_nonoverlapping(data, copy_ptr as *mut u8, len);
        Ok(as_unknown(
            env,
            JsUnknown::from_raw_unchecked(env.raw(), ab),
        ))
    }
}

/// Box a JS value as a concrete WinRT `IPropertyValue`, returned as `{ handle }` (the shape
/// arg parsers accept). Unknown type names return JS `null`.
pub fn typed_value(env: &Env, type_name: &str, value: &JsUnknown) -> napi::Result<JsUnknown> {
    let Some(nv) = crate::napi_engine::value::box_as_typed_value(env, value, type_name.trim())
    else {
        return Ok(as_unknown(env, env.get_null()?));
    };
    let ptr = unsafe { nv.pointer };
    let ext = env.create_external(ptr as usize, None)?;
    let mut obj = env.create_object()?;
    obj.set_named_property("handle", ext)?;
    Ok(as_unknown(env, obj))
}

/// Stable `"0x…"` key for a pointer-like value (external, `{handle}` object, or null).
pub fn pointer_key(env: &Env, value: &JsUnknown) -> napi::Result<String> {
    let ptr = extract_pointer_like(env, value).ok_or_else(|| {
        napi::Error::from_reason("Unable to extract native pointer from value".to_string())
    })?;
    Ok(format!("0x{:x}", ptr as usize))
}

fn extract_pointer_like(env: &Env, value: &JsUnknown) -> Option<*mut c_void> {
    match value.get_type().ok()? {
        ValueType::Null | ValueType::Undefined => Some(std::ptr::null_mut()),
        ValueType::External => ptr_from_external(env, value),
        ValueType::Object | ValueType::Function => {
            let obj: JsObject = unsafe { value.cast() };
            try_get_external_handle(env, &obj)
        }
        _ => None,
    }
}

/// Backing-store address of an ArrayBuffer/ArrayBufferView as a pointer external (`null` if empty).
pub fn buffer_to_pointer(env: &Env, value: &JsUnknown) -> napi::Result<JsUnknown> {
    if matches!(
        value.get_type(),
        Ok(ValueType::Null) | Ok(ValueType::Undefined)
    ) {
        return Ok(as_unknown(env, env.get_null()?));
    }
    let Some((data, _)) = buffer_data(env, value) else {
        return Err(napi::Error::from_reason(
            "__nsBufferToPointer expects an ArrayBuffer or ArrayBufferView".to_string(),
        ));
    };
    if data.is_null() {
        return Ok(as_unknown(env, env.get_null()?));
    }
    let ext = env.create_external(data as usize, None)?;
    Ok(as_unknown(env, ext))
}

fn string_arg(ctx: &CallContext, index: usize) -> String {
    ctx.get::<JsUnknown>(index)
        .map(|v| js_to_rust_string(&ctx.env, &v))
        .unwrap_or_default()
}

fn typed_value_cb(ctx: &CallContext, name: &str) -> napi::Result<JsUnknown> {
    if ctx.length < 2 {
        return Err(napi::Error::from_reason(format!(
            "{name}(typeName, value) expects 2 arguments"
        )));
    }
    let type_name = string_arg(ctx, 0);
    let value = ctx.get::<JsUnknown>(1)?;
    typed_value(&ctx.env, &type_name, &value)
}

/// Install the `interop.*` surface: the `__ns*` natives + the `NSWinRT.interop` JS layer,
/// shared by the Node addon and every standalone engine. Idempotent. The JS is run via the
/// global `Function` ctor rather than `napi_run_script`, which not every engine shim provides.
pub fn install_interop(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;

    let uuid_fn = env.create_function_from_closure("__nsUUID", |_ctx| Ok(ns_uuid()))?;
    global.set_named_property("__nsUUID", uuid_fn)?;

    let typed_fn = env.create_function_from_closure("__nsTypedValue", |ctx| {
        typed_value_cb(&ctx, "__nsTypedValue")
    })?;
    global.set_named_property("__nsTypedValue", typed_fn)?;

    let reference_fn = env.create_function_from_closure("__nsCreateReference", |ctx| {
        typed_value_cb(&ctx, "__nsCreateReference")
    })?;
    global.set_named_property("__nsCreateReference", reference_fn)?;

    let key_fn = env.create_function_from_closure("__nsPointerKey", |ctx| {
        if ctx.length < 1 {
            return Err(napi::Error::from_reason(
                "__nsPointerKey expects a pointer-like value".to_string(),
            ));
        }
        let value = ctx.get::<JsUnknown>(0)?;
        pointer_key(&ctx.env, &value)
    })?;
    global.set_named_property("__nsPointerKey", key_fn)?;

    let buf_ptr_fn = env.create_function_from_closure("__nsBufferToPointer", |ctx| {
        if ctx.length < 1 {
            return Err(napi::Error::from_reason(
                "__nsBufferToPointer expects an ArrayBuffer or ArrayBufferView".to_string(),
            ));
        }
        let value = ctx.get::<JsUnknown>(0)?;
        buffer_to_pointer(&ctx.env, &value)
    })?;
    global.set_named_property("__nsBufferToPointer", buf_ptr_fn)?;

    let register_fn = env.create_function_from_closure("__nsRegisterWinmd", |ctx| {
        register_winmd(&string_arg(&ctx, 0)).map_err(napi::Error::from_reason)
    })?;
    global.set_named_property("__nsRegisterWinmd", register_fn)?;

    let scan_fn = env.create_function_from_closure("__nsScanWinmdDir", |ctx| {
        Ok(scan_winmd_dir(&string_arg(&ctx, 0)) as u32)
    })?;
    global.set_named_property("__nsScanWinmdDir", scan_fn)?;

    let ab_fn = env.create_function_from_closure("__nsArrayBufferFromBuffer", |ctx| {
        if ctx.length < 1 {
            return Err(napi::Error::from_reason(
                "__nsArrayBufferFromBuffer expects a WinRT IBuffer".to_string(),
            ));
        }
        let value = ctx.get::<JsUnknown>(0)?;
        array_buffer_from_buffer(&ctx.env, &value)
    })?;
    global.set_named_property("__nsArrayBufferFromBuffer", ab_fn)?;

    let func_ctor: JsFunction = global.get_named_property("Function")?;
    let body = env.create_string(INTEROP_HELPERS_JS)?;
    let installer_obj = func_ctor.new_instance(&[body])?;
    let installer: JsFunction =
        unsafe { JsFunction::from_raw(env.raw(), installer_obj.raw()) }?;
    installer.call_without_args(None)?;
    Ok(())
}

/// The JS half of `interop.*`, mirroring the classic runtime's `NSWinRT.interop` block in
/// `global_fns.rs` HELPER_SOURCE — keep the two in sync. No BigInt literals or modern syntax
/// so it runs on every engine (the DateTime tick helpers fall back to Number without BigInt).
const INTEROP_HELPERS_JS: &str = r#"
'use strict';
var g = globalThis;
if (g.NSWinRT && g.NSWinRT.interop) { return; }

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
var hasBigInt = typeof BigInt === 'function';
var winRtUnixEpochOffsetTicks = hasBigInt ? BigInt('116444736000000000') : 116444736000000000;

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

    if (hasBigInt) {
        return BigInt(Math.trunc(ms)) * BigInt(10000) + winRtUnixEpochOffsetTicks;
    }
    return Math.trunc(ms) * 10000 + winRtUnixEpochOffsetTicks;
}

function fromWinRTDateTimeTicks(value) {
    if (value == null) {
        return new Date(Number.NaN);
    }

    if (hasBigInt) {
        var ticks = typeof value === 'bigint' ? value : BigInt(Math.trunc(Number(value)));
        return new Date(Number((ticks - winRtUnixEpochOffsetTicks) / BigInt(10000)));
    }
    return new Date((Number(value) - winRtUnixEpochOffsetTicks) / 10000);
}

var pointerBufferRegistry = new Map();

function pointerKey(value) {
    if (typeof g.__nsPointerKey !== 'function') {
        return null;
    }
    return g.__nsPointerKey(value);
}

function pointerFromBuffer(value) {
    var source = asBufferSource(value);
    if (source == null || typeof g.__nsBufferToPointer !== 'function') {
        return null;
    }
    return g.__nsBufferToPointer(source);
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

var interop = {
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
        if (typeof g.__nsArrayBufferFromBuffer !== 'function') {
            throw new Error('NSWinRT.interop.arrayBufferFromBuffer is unavailable in this runtime');
        }
        return g.__nsArrayBufferFromBuffer(buffer);
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
        if (typeof g.__nsCreateReference !== 'function') { return null; }
        return g.__nsCreateReference(typeName, value);
    },
    // Typed concrete-value helpers
    // These create a concrete typed IPropertyValue (not a nullable IReference)
    // so the WinRT runtime can pick the correct overload when a parameter is
    // typed as Object/IInspectable.  Pass the returned object directly to
    // any WinRT method that would otherwise receive an untyped JS number.
    float: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Single', +n) : null; },
    double: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Double', +n) : null; },
    int: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Int32', +n) : null; },
    uint: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('UInt32', +n) : null; },
    long: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Int64', +n) : null; },
    ulong: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('UInt64', +n) : null; },
    short: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Int16', +n) : null; },
    ushort: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('UInt16', +n) : null; },
    byte: function (n) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('UInt8', +n) : null; },
    char: function (c) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Char16', c) : null; },
    bool: function (v) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Boolean', !!v) : null; },
    // Date/time helpers
    timeSpan: function (ms) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('TimeSpan', +ms) : null; },
    dateTime: function (msOrDate) {
        var ms = (msOrDate instanceof Date) ? msOrDate.getTime() : +msOrDate;
        return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('DateTime', ms) : null;
    },
    guid: function (str) { return typeof g.__nsTypedValue === 'function' ? g.__nsTypedValue('Guid', String(str)) : null; },
    // napi additions (not on the classic interop object): UUID generation and
    // third-party winmd registration, over the same natives the addon exports.
    uuid: function () { return typeof g.__nsUUID === 'function' ? g.__nsUUID() : ''; },
    registerWinmd: function (path) {
        if (typeof g.__nsRegisterWinmd !== 'function') {
            throw new Error('NSWinRT.interop.registerWinmd is unavailable in this runtime');
        }
        return g.__nsRegisterWinmd(String(path));
    },
    scanWinmdDir: function (dir) {
        return typeof g.__nsScanWinmdDir === 'function' ? g.__nsScanWinmdDir(String(dir)) : 0;
    }
};

g.NSWinRT = g.NSWinRT || {};
g.NSWinRT.interop = interop;
// Top-level `interop` alias, matching the classic runtime; an existing global wins.
if (g.interop == null) {
    g.interop = interop;
}
"#;
