//! Node-API implementation of the .NET/BCL bridge exposed to JS as `NSWinRT.dotnet` (the classic
//! runtime exposes the same surface from `global_fns.rs`'s `HELPER_SOURCE`).
//!
//! `crate::dotnet` (the hostfxr-based CLR host, `call_dotnet` / `call_dotnet_binary`) has zero
//! `v8::` references and is reused completely unmodified here: see
//! `[[project_engine_framework_parity]]` for how that was confirmed. What this module supplies is
//! the engine-specific half: the same seven `__ns*` natives the classic engine registers in
//! `global_fns.rs`, reimplemented against Node-API instead of V8's C++ API, plus the JS half shared with
//! the classic engine through `runtime/src/js` ([`DOTNET_HELPERS_JS`]).
//!
//! Includes `__nsDotNetCreateJsSubclass` and `__nsWrapWinRTPointer`, which back JS subclasses of
//! .NET and WinRT classes (`class X extends Y`, `Y.extend(...)`, `new Interface({...})`; see the
//! "JS classes" parts of [`DOTNET_HELPERS_JS`]): `Bridge.Proxy.cs` emits a managed subclass at
//! runtime whose overridden members call back into the JS object. The older `NSWinRT.proxy`
//! surface (`createManagedSubclass`, `emit`/`compile`/`register`) is kept for API parity; its
//! ahead-of-time pipeline compiles but its output is never loaded.
//!
//! ## Binary wire protocol
//!
//! `__nsDotNetInvokeBin` and the JS-callback response both use the bridge's tagged binary format
//! (opcodes / tags match `DotNetBridge.dll` exactly, unmodified). Two writers exist because the
//! classic engine uses two different string-length-prefix widths depending on direction:
//! [`napi_bin_write_arg`] (outgoing method arguments, `u16` string length, AddRefs COM pointers
//! before handing them to managed code) and [`napi_bin_write_value`] (values flowing back to
//! managed, i.e. JS-callback return values; `u32` string length, adds array support, no AddRef). One
//! reader, [`napi_bin_read_value`], covers every response shape in both directions.
//!
//! ## JS-callback dispatch
//!
//! `crate::dotnet::ensure_dotnet_initialized` always registers exactly one C callback with the
//! managed bridge (`RegisterJsCallback`) so delegates/tasks/UI-thread hops can call back into JS.
//! The classic engine never registers one explicitly, so `crate::dotnet` falls back to its own
//! `global_fns::invoke_dotnet_js_callback` (isolate found via a thread-local). This module instead
//! calls `crate::dotnet::set_js_callback_dispatcher` during [`install_dotnet`]: before any JS can
//! reach `ensure_dotnet_initialized`: so [`napi_invoke_dotnet_js_callback`] wins for napi hosts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::Ordering;

use napi::{
    sys, CallContext, Env, JsBigInt, JsBoolean, JsFunction, JsNumber, JsObject, JsUnknown,
    NapiRaw, NapiValue, ValueType,
};
use windows::core::{IUnknown, Interface};

use crate::napi_engine::value::{as_unknown, js_to_rust_string};

extern "system" {
    fn LocalAlloc(uFlags: u32, uBytes: usize) -> *mut c_void;
}
const LMEM_FIXED: u32 = 0x0000;

// Pinned JS callbacks reachable from managed code: delegate/task-callback ids -> (owning env, a
// napi_ref keeping the function alive). A plain HashMap doesn't auto-release like the classic
// engine's `HashMap<i32, v8::Global<Function>>` does on removal: every remove site here must
// pair with an explicit `napi_delete_reference`.
thread_local!(static NAPI_DOTNET_CALLBACKS: RefCell<HashMap<i32, (sys::napi_env, sys::napi_ref)>> = RefCell::new(HashMap::new()));

//

fn napi_array_length(env: &Env, arg: &JsUnknown) -> Option<u32> {
    unsafe {
        let raw_env = env.raw();
        let raw = arg.raw();
        let mut is_arr = false;
        if sys::napi_is_array(raw_env, raw, &mut is_arr) != sys::Status::napi_ok || !is_arr {
            return None;
        }
        let mut len = 0u32;
        if sys::napi_get_array_length(raw_env, raw, &mut len) != sys::Status::napi_ok {
            return None;
        }
        Some(len)
    }
}

fn napi_array_get(env: &Env, arg: &JsUnknown, index: u32) -> Option<JsUnknown> {
    unsafe {
        let raw_env = env.raw();
        let raw = arg.raw();
        let mut el: sys::napi_value = std::ptr::null_mut();
        if sys::napi_get_element(raw_env, raw, index, &mut el) != sys::Status::napi_ok {
            return None;
        }
        Some(JsUnknown::from_raw_unchecked(raw_env, el))
    }
}

/// AddRef a raw WinRT COM pointer read off a `__native_ptr` BigInt: the outgoing arg is about to
/// be handed to managed code, which will wrap and `Release` it later (matches the rusty_v8
/// original's ownership contract for `__nsDotNetInvokeBin` request args exactly).
unsafe fn addref_native_ptr(ptr: u64) {
    let unknown = ManuallyDrop::new(IUnknown::from_raw(ptr as usize as *mut c_void));
    let vtable = unknown.vtable();
    ((*vtable).AddRef)(unknown.as_raw());
}

/// Pins `func` as a callback the bridge can invoke; returns its id.
fn pin_js_callback(env: &Env, func: sys::napi_value) -> Option<i32> {
    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    let status = unsafe { sys::napi_create_reference(env.raw(), func, 1, &mut func_ref) };
    if status != sys::Status::napi_ok || func_ref.is_null() {
        return None;
    }
    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    NAPI_DOTNET_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, (env.raw(), func_ref));
    });
    Some(cb_id)
}

/// Unpins the callbacks whose managed delegates the .NET GC has collected.
fn release_collected_js_callbacks() {
    let released = crate::dotnet::take_released_js_callbacks();
    if released.is_empty() {
        return;
    }
    NAPI_DOTNET_CALLBACKS.with(|m| {
        let mut m = m.borrow_mut();
        for id in released {
            if let Some((env, func_ref)) = m.remove(&id) {
                unsafe {
                    let _ = sys::napi_delete_reference(env, func_ref);
                }
            }
        }
    });
}

/// A plain JS object (`{ Width: 120, Height: 40 }`) as JSON, for a .NET struct/record parameter or
/// result (tag 0x0D). Mirrors `global_fns::plain_object_json`.
fn napi_plain_object_json(env: &Env, arg: &JsUnknown) -> Option<String> {
    if napi_array_length(env, arg).is_some() {
        return None;
    }
    let obj: JsObject = unsafe { arg.cast() };
    let ctor = obj.get_named_property::<JsUnknown>("constructor").ok()?;
    if ctor.get_type().ok()? != ValueType::Function {
        return None;
    }
    let ctor: JsObject = unsafe { ctor.cast() };
    let name = ctor.get_named_property::<JsUnknown>("name").ok()?;
    if js_to_rust_string(env, &name) != "Object" {
        return None;
    }
    let json: JsObject = env.get_global().ok()?.get_named_property("JSON").ok()?;
    let stringify: JsFunction = json.get_named_property("stringify").ok()?;
    let text = stringify.call(Some(&json), &[arg]).ok()?;
    if text.get_type().ok()? != ValueType::String {
        return None;
    }
    Some(js_to_rust_string(env, &text))
}

fn napi_json_parse(env: &Env, text: &str) -> napi::Result<JsUnknown> {
    let json: JsObject = env.get_global()?.get_named_property("JSON")?;
    let parse: JsFunction = json.get_named_property("parse")?;
    parse.call(Some(&json), &[env.create_string(text)?])
}

/// A BigInt as a .NET long (tag 0x10), or ulong (0x11) when only that fits. Read through sign and
/// words: the QuickJS shim doesn't report `lossless`.
fn napi_bin_write_bigint(env: &Env, buf: &mut Vec<u8>, arg: &JsUnknown) -> bool {
    let mut sign = 0i32;
    let mut count = 2usize;
    let mut words = [0u64; 2];
    let status = unsafe { sys::napi_get_value_bigint_words(env.raw(), arg.raw(), &mut sign, &mut count, words.as_mut_ptr()) };
    if status != sys::Status::napi_ok {
        return false;
    }
    let magnitude = if count == 0 { 0 } else { words[0] };
    if sign != 0 {
        // Values below long.MinValue wrap, as they do in a C# unchecked cast.
        buf.push(0x10);
        buf.extend_from_slice(&(magnitude as i64).wrapping_neg().to_le_bytes());
    } else if magnitude <= i64::MAX as u64 {
        buf.push(0x10);
        buf.extend_from_slice(&(magnitude as i64).to_le_bytes());
    } else {
        buf.push(0x11);
        buf.extend_from_slice(&magnitude.to_le_bytes());
    }
    true
}

/// A typed array, ArrayBuffer or DataView as tag 0x12 (see `global_fns::bin_write_v8_bytes`).
fn napi_bin_write_bytes(env: &Env, buf: &mut Vec<u8>, arg: &JsUnknown) -> bool {
    let (env_raw, value) = (env.raw(), unsafe { arg.raw() });
    let mut is = false;
    let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut len = 0usize;
    let mut kind = 0u8;
    unsafe {
        if sys::napi_is_typedarray(env_raw, value, &mut is) == sys::Status::napi_ok && is {
            let mut ty: sys::napi_typedarray_type = 0;
            let mut count = 0usize;
            let mut buffer: sys::napi_value = std::ptr::null_mut();
            let mut offset = 0usize;
            if sys::napi_get_typedarray_info(env_raw, value, &mut ty, &mut count, &mut data, &mut buffer, &mut offset) != sys::Status::napi_ok {
                return false;
            }
            let (k, size) = match ty {
                0 => (1, 1),
                3 => (2, 2),
                4 => (3, 2),
                5 => (4, 4),
                6 => (5, 4),
                7 => (6, 4),
                8 => (7, 8),
                9 => (8, 8),
                10 => (9, 8),
                _ => (0, 1),
            };
            kind = k;
            len = count * size;
        } else if sys::napi_is_dataview(env_raw, value, &mut is) == sys::Status::napi_ok && is {
            let mut buffer: sys::napi_value = std::ptr::null_mut();
            let mut offset = 0usize;
            if sys::napi_get_dataview_info(env_raw, value, &mut len, &mut data, &mut buffer, &mut offset) != sys::Status::napi_ok {
                return false;
            }
        } else if sys::napi_is_arraybuffer(env_raw, value, &mut is) == sys::Status::napi_ok && is {
            if sys::napi_get_arraybuffer_info(env_raw, value, &mut data, &mut len) != sys::Status::napi_ok {
                return false;
            }
        } else {
            return false;
        }
    }
    buf.push(0x12);
    buf.push(kind);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    if data.is_null() || len == 0 {
        buf.resize(buf.len() + len, 0);
    } else {
        buf.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const u8, len) });
    }
    true
}

/// Bytes from the bridge (tag 0x12) as a typed array of `kind` over a new ArrayBuffer.
fn napi_typed_array(env: &Env, kind: u8, bytes: &[u8]) -> Result<JsUnknown, String> {
    let env_raw = env.raw();
    let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut buffer: sys::napi_value = std::ptr::null_mut();
    unsafe {
        if sys::napi_create_arraybuffer(env_raw, bytes.len(), &mut data, &mut buffer) != sys::Status::napi_ok {
            return Err("could not create an ArrayBuffer".into());
        }
        if !data.is_null() && !bytes.is_empty() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data as *mut u8, bytes.len());
        }
        let (ty, size): (sys::napi_typedarray_type, usize) = match kind {
            1 => (0, 1),
            2 => (3, 2),
            3 => (4, 2),
            4 => (5, 4),
            5 => (6, 4),
            6 => (7, 4),
            7 => (8, 8),
            8 => (9, 8),
            9 => (10, 8),
            _ => (1, 1),
        };
        let mut typed: sys::napi_value = std::ptr::null_mut();
        if sys::napi_create_typedarray(env_raw, ty, bytes.len() / size, buffer, 0, &mut typed) != sys::Status::napi_ok {
            return Ok(JsUnknown::from_raw_unchecked(env_raw, buffer));
        }
        Ok(JsUnknown::from_raw_unchecked(env_raw, typed))
    }
}

/// A length-prefixed (u16 or u32, per `prefix`) UTF-8 string at `pos`, advancing past it.
fn bin_take_str<'b>(bytes: &'b [u8], pos: &mut usize, prefix: usize) -> Option<&'b str> {
    let len_bytes = bytes.get(*pos..*pos + prefix)?;
    let len = if prefix == 2 {
        u16::from_le_bytes(len_bytes.try_into().ok()?) as usize
    } else {
        u32::from_le_bytes(len_bytes.try_into().ok()?) as usize
    };
    let text = std::str::from_utf8(bytes.get(*pos + prefix..*pos + prefix + len)?).ok()?;
    *pos += prefix + len;
    Some(text)
}

/// Throws the Error for a bridge error (0xFF), see `global_fns::bin_error_value`, and returns the
/// message; the caller's Err leaves that Error pending.
fn napi_throw_bridge_error(env: &Env, bytes: &[u8]) -> String {
    let mut pos = 1usize;
    let message = bin_take_str(bytes, &mut pos, 4).unwrap_or("the .NET call failed").to_string();
    let error = (|| -> napi::Result<JsObject> {
        let mut error: JsObject = unsafe {
            let msg = env.create_string(&message)?;
            let mut raw: sys::napi_value = std::ptr::null_mut();
            sys::napi_create_error(env.raw(), std::ptr::null_mut(), msg.raw(), &mut raw);
            JsObject::from_raw_unchecked(env.raw(), raw)
        };
        let type_name = bin_take_str(bytes, &mut pos, 2);
        let stack = bin_take_str(bytes, &mut pos, 4);
        let handle = bytes.get(pos..pos + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()));
        if let (Some(type_name), Some(stack)) = (type_name, stack) {
            error.set_named_property("dotnetType", env.create_string(type_name)?)?;
            error.set_named_property("dotnetStack", env.create_string(stack)?)?;
            if let Some(id) = handle.filter(|id| *id >= 0) {
                let mut native = env.create_object()?;
                native.set_named_property("__handle", env.create_int32(id)?)?;
                native.set_named_property("__type", env.create_string(type_name)?)?;
                error.set_named_property("nativeException", native)?;
            }
        }
        Ok(error)
    })();
    if let Ok(error) = error {
        unsafe { sys::napi_throw(env.raw(), error.raw()) };
    }
    message
}

/// Encode one outgoing method-call argument (`u16` string-length prefix; AddRefs `__native_ptr`).
/// Mirrors `global_fns::bin_write_v8_arg`.
fn napi_bin_write_arg(env: &Env, buf: &mut Vec<u8>, arg: &JsUnknown) {
    let vt = arg.get_type().unwrap_or(ValueType::Undefined);
    if (vt == ValueType::BigInt && napi_bin_write_bigint(env, buf, arg)) || (vt == ValueType::Object && napi_bin_write_bytes(env, buf, arg)) {
        return;
    }
    match vt {
        ValueType::Null | ValueType::Undefined => buf.push(0x00),
        ValueType::Boolean => {
            let b: JsBoolean = unsafe { arg.cast() };
            buf.push(if b.get_value().unwrap_or(false) { 0x02 } else { 0x01 });
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            let f = n.get_double().unwrap_or(0.0);
            if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
                buf.push(0x03);
                buf.extend_from_slice(&(f as i32).to_le_bytes());
            } else {
                buf.push(0x04);
                buf.extend_from_slice(&f.to_bits().to_le_bytes());
            }
        }
        ValueType::String => {
            let s = js_to_rust_string(env, arg);
            buf.push(0x05);
            crate::dotnet::bin_write_str16(buf, s.as_bytes());
        }
        ValueType::Object | ValueType::Function => {
            // A JS array: tag 0x07, a u32 count, then each item encoded like an argument (see
            // `global_fns::bin_write_v8_arg`).
            if let Some(len) = napi_array_length(env, arg) {
                buf.push(0x07);
                buf.extend_from_slice(&len.to_le_bytes());
                for i in 0..len {
                    match napi_array_get(env, arg, i) {
                        Some(item) => napi_bin_write_arg(env, buf, &item),
                        None => buf.push(0x00),
                    }
                }
                return;
            }
            let obj: JsObject = unsafe { arg.cast() };
            if let Ok(hval) = obj.get_named_property::<JsUnknown>("__handle") {
                if let Ok(ValueType::Number) = hval.get_type() {
                    let n: JsNumber = unsafe { hval.cast() };
                    buf.push(0x06);
                    buf.extend_from_slice(&(n.get_double().unwrap_or(0.0) as i32).to_le_bytes());
                    return;
                }
            }
            if let Ok(pval) = obj.get_named_property::<JsUnknown>("__native_ptr") {
                if let Ok(ValueType::BigInt) = pval.get_type() {
                    let b: JsBigInt = unsafe { pval.cast() };
                    if let Ok((ptr, _)) = b.get_u64() {
                        if ptr != 0 {
                            unsafe { addref_native_ptr(ptr) };
                            buf.push(0x0A);
                            buf.extend_from_slice(&ptr.to_le_bytes());
                            return;
                        }
                    }
                }
            }
            // A JS function where .NET expects a delegate: a callback id the bridge wraps in a
            // delegate of the parameter's type (see `global_fns::bin_write_v8_arg`).
            if vt == ValueType::Function {
                if let Some(cb_id) = pin_js_callback(env, unsafe { arg.raw() }) {
                    buf.push(0x0C);
                    buf.extend_from_slice(&cb_id.to_le_bytes());
                    return;
                }
            }
            if let Some(json) = napi_plain_object_json(env, arg) {
                buf.push(0x0D);
                crate::dotnet::bin_write_str32(buf, json.as_bytes());
                return;
            }
            let s = js_to_rust_string(env, arg);
            buf.push(0x05);
            crate::dotnet::bin_write_str16(buf, s.as_bytes());
        }
        _ => buf.push(0x00),
    }
}

/// Encode a value flowing back to managed code (`u32` string-length prefix; array support; no
/// AddRef on `__native_ptr`). Mirrors `global_fns::bin_write_v8_value`.
fn napi_bin_write_value(env: &Env, buf: &mut Vec<u8>, arg: &JsUnknown) {
    let vt = arg.get_type().unwrap_or(ValueType::Undefined);
    if (vt == ValueType::BigInt && napi_bin_write_bigint(env, buf, arg)) || (vt == ValueType::Object && napi_bin_write_bytes(env, buf, arg)) {
        return;
    }
    match vt {
        ValueType::Null | ValueType::Undefined => buf.push(0x00),
        ValueType::Boolean => {
            let b: JsBoolean = unsafe { arg.cast() };
            buf.push(if b.get_value().unwrap_or(false) { 0x02 } else { 0x01 });
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            let f = n.get_double().unwrap_or(0.0);
            if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
                buf.push(0x03);
                buf.extend_from_slice(&(f as i32).to_le_bytes());
            } else {
                buf.push(0x04);
                buf.extend_from_slice(&f.to_bits().to_le_bytes());
            }
        }
        ValueType::String => {
            let s = js_to_rust_string(env, arg);
            buf.push(0x05);
            crate::dotnet::bin_write_str32(buf, s.as_bytes());
        }
        ValueType::Object | ValueType::Function => {
            if let Some(len) = napi_array_length(env, arg) {
                buf.push(0x07);
                buf.extend_from_slice(&len.to_le_bytes());
                for i in 0..len {
                    match napi_array_get(env, arg, i) {
                        Some(item) => napi_bin_write_value(env, buf, &item),
                        None => buf.push(0x00),
                    }
                }
                return;
            }
            let obj: JsObject = unsafe { arg.cast() };
            if let Ok(hval) = obj.get_named_property::<JsUnknown>("__handle") {
                if let Ok(ValueType::Number) = hval.get_type() {
                    let n: JsNumber = unsafe { hval.cast() };
                    buf.push(0x06);
                    buf.extend_from_slice(&(n.get_double().unwrap_or(0.0) as i32).to_le_bytes());
                    return;
                }
            }
            if let Ok(pval) = obj.get_named_property::<JsUnknown>("__native_ptr") {
                if let Ok(ValueType::BigInt) = pval.get_type() {
                    let b: JsBigInt = unsafe { pval.cast() };
                    if let Ok((ptr, _)) = b.get_u64() {
                        if ptr != 0 {
                            buf.push(0x0A);
                            buf.extend_from_slice(&ptr.to_le_bytes());
                            return;
                        }
                    }
                }
            }
            if let Some(json) = napi_plain_object_json(env, arg) {
                buf.push(0x0D);
                crate::dotnet::bin_write_str32(buf, json.as_bytes());
                return;
            }
            let s = js_to_rust_string(env, arg);
            buf.push(0x05);
            crate::dotnet::bin_write_str32(buf, s.as_bytes());
        }
        _ => buf.push(0x00),
    }
}

/// Decode one tagged value (response or JS-callback argument). Mirrors `global_fns::bin_read_value`:
/// the classic engine shares this same reader between both call sites, so this does too.
fn napi_bin_read_value(env: &Env, bytes: &[u8], pos: &mut usize) -> Result<JsUnknown, String> {
    let map_err = |e: napi::Error| e.to_string();
    let tag = *bytes.get(*pos).ok_or("response truncated")?;
    *pos += 1;

    match tag {
        0x00 => Ok(as_unknown(env, env.get_null().map_err(map_err)?)),
        0x01 => Ok(as_unknown(env, env.get_boolean(false).map_err(map_err)?)),
        0x02 => Ok(as_unknown(env, env.get_boolean(true).map_err(map_err)?)),

        0x03 => {
            let v = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "i32 read")?);
            *pos += 4;
            Ok(as_unknown(env, env.create_int32(v).map_err(map_err)?))
        }

        0x04 => {
            let bits =
                u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().map_err(|_| "f64 read")?);
            *pos += 8;
            Ok(as_unknown(
                env,
                env.create_double(f64::from_bits(bits)).map_err(map_err)?,
            ))
        }

        0x05 => {
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "str len")?)
                as usize;
            *pos += 4;
            let s = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
            *pos += len;
            Ok(as_unknown(env, env.create_string(s).map_err(map_err)?))
        }

        0x10 => {
            let v = i64::from_le_bytes(bytes.get(*pos..*pos + 8).ok_or("i64 read")?.try_into().map_err(|_| "i64 read")?);
            *pos += 8;
            Ok(as_unknown(env, env.create_bigint_from_i64(v).map_err(map_err)?.into_unknown().map_err(map_err)?))
        }

        0x11 => {
            let v = u64::from_le_bytes(bytes.get(*pos..*pos + 8).ok_or("u64 read")?.try_into().map_err(|_| "u64 read")?);
            *pos += 8;
            Ok(as_unknown(env, env.create_bigint_from_u64(v).map_err(map_err)?.into_unknown().map_err(map_err)?))
        }

        0x12 => {
            let kind = *bytes.get(*pos).ok_or("bytes kind")?;
            let len = u32::from_le_bytes(bytes.get(*pos + 1..*pos + 5).ok_or("bytes len")?.try_into().map_err(|_| "bytes len")?) as usize;
            *pos += 5;
            let data = bytes.get(*pos..*pos + len).ok_or("bytes")?;
            *pos += len;
            napi_typed_array(env, kind, data)
        }

        0x0D => {
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "json len")?)
                as usize;
            *pos += 4;
            let text = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
            *pos += len;
            napi_json_parse(env, text).map_err(map_err)
        }

        0x06 | 0x0C => {
            let id = i32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "handle id")?);
            *pos += 4;
            let type_len =
                u16::from_le_bytes(bytes[*pos..*pos + 2].try_into().map_err(|_| "type len")?)
                    as usize;
            *pos += 2;
            let type_name =
                std::str::from_utf8(&bytes[*pos..*pos + type_len]).map_err(|_| "utf8")?;
            *pos += type_len;

            let mut obj = env.create_object().map_err(map_err)?;
            obj.set_named_property("__handle", env.create_int32(id).map_err(map_err)?)
                .map_err(map_err)?;
            obj.set_named_property("__type", env.create_string(type_name).map_err(map_err)?)
                .map_err(map_err)?;
            if tag == 0x0C {
                obj.set_named_property("__isTask", env.get_boolean(true).map_err(map_err)?)
                    .map_err(map_err)?;
            }

            if bytes.len() - *pos >= 1 {
                let flag = bytes[*pos];
                *pos += 1;
                if flag != 0 && bytes.len() - *pos >= 8 {
                    let raw = i64::from_le_bytes(
                        bytes[*pos..*pos + 8].try_into().map_err(|_| "i64 read")?,
                    );
                    *pos += 8;
                    let bi = if raw >= 0 {
                        env.create_bigint_from_u64(raw as u64).map_err(map_err)?
                    } else {
                        env.create_bigint_from_i64(raw).map_err(map_err)?
                    };
                    obj.set_named_property("__native_ptr", bi).map_err(map_err)?;
                }
            }
            Ok(as_unknown(env, obj))
        }

        0x07 => {
            let count =
                u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "arr count")?)
                    as usize;
            *pos += 4;
            let mut arr = env.create_array_with_length(count).map_err(map_err)?;
            for i in 0..count {
                let item = napi_bin_read_value(env, bytes, pos)?;
                arr.set_element(i as u32, item).map_err(map_err)?;
            }
            Ok(as_unknown(env, arr))
        }

        0x08 => {
            let mut obj = env.create_object().map_err(map_err)?;
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
                    bytes[*pos..*pos + 2].try_into().map_err(|_| "member count")?,
                ) as usize;
                *pos += 2;
                let mut arr = env.create_array_with_length(count).map_err(map_err)?;
                for i in 0..count {
                    let len = u16::from_le_bytes(
                        bytes[*pos..*pos + 2].try_into().map_err(|_| "str len")?,
                    ) as usize;
                    *pos += 2;
                    let s = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
                    *pos += len;
                    arr.set_element(i as u32, env.create_string(s).map_err(map_err)?)
                        .map_err(map_err)?;
                }
                obj.set_named_property(key, arr).map_err(map_err)?;
            }
            // Extra named lists (events, nested types, ...): u16 count, then a key and a list each.
            let extra = match bytes.get(*pos..*pos + 2) {
                Some(b) => u16::from_le_bytes(b.try_into().unwrap()) as usize,
                None => 0,
            };
            if extra > 0 {
                *pos += 2;
            }
            for _ in 0..extra {
                let key = bin_take_str(bytes, pos, 2).ok_or("member list key")?.to_string();
                let count = u16::from_le_bytes(bytes.get(*pos..*pos + 2).ok_or("member count")?.try_into().unwrap()) as usize;
                *pos += 2;
                let mut arr = env.create_array_with_length(count).map_err(map_err)?;
                for i in 0..count {
                    let s = bin_take_str(bytes, pos, 2).ok_or("member name")?;
                    arr.set_element(i as u32, env.create_string(s).map_err(map_err)?).map_err(map_err)?;
                }
                obj.set_named_property(&key, arr).map_err(map_err)?;
            }
            Ok(as_unknown(env, obj))
        }

        0xFF => {
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().map_err(|_| "err len")?)
                as usize;
            *pos += 4;
            let msg = std::str::from_utf8(&bytes[*pos..*pos + len]).map_err(|_| "utf8")?;
            Err(msg.to_string())
        }

        t => Err(format!("Unknown binary response tag 0x{t:02X}")),
    }
}

fn napi_bin_read_response(env: &Env, bytes: &[u8]) -> Result<JsUnknown, String> {
    if bytes.is_empty() {
        return Ok(as_unknown(env, env.get_null().map_err(|e| e.to_string())?));
    }
    if bytes[0] == 0xFF {
        return Err(napi_throw_bridge_error(env, bytes));
    }
    let mut pos = 0usize;
    napi_bin_read_value(env, bytes, &mut pos)
}

//

fn native_invoke(ctx: &CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 1 {
        return Err(napi::Error::from_reason(
            "__nsDotNetInvoke: expected a JSON string argument",
        ));
    }
    let env = &ctx.env;
    let json = js_to_rust_string(env, &ctx.get::<JsUnknown>(0)?);
    match crate::dotnet::call_dotnet(&json) {
        Ok(result) => Ok(as_unknown(env, env.create_string(&result)?)),
        Err(e) => Err(napi::Error::from_reason(e)),
    }
}

/// Binary-protocol bridge: builds a compact request packet from structured napi arguments, calls
/// the C# `InvokeBinary` entry point, and converts the binary response directly into a napi
/// value: no JSON on either side. Argument convention matches the JS `_invoke` wrapper: `args[0]`
/// handle (i32, or null/undefined for a static call), `args[1]` typeName, `args[2]` assembly,
/// `args[3]` method, `args[4..]` the method's own arguments.
fn native_invoke_bin(ctx: &CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 4 {
        return Err(napi::Error::from_reason(
            "__nsDotNetInvokeBin: expected (handle, typeName, assembly, method, ...args)",
        ));
    }
    let env = &ctx.env;

    let handle_v = ctx.get::<JsUnknown>(0)?;
    let handle: i32 = match handle_v.get_type()? {
        ValueType::Number => {
            let n: JsNumber = unsafe { handle_v.cast() };
            n.get_double()? as i32
        }
        _ => -1,
    };

    let type_name = js_to_rust_string(env, &ctx.get::<JsUnknown>(1)?);
    let assembly = js_to_rust_string(env, &ctx.get::<JsUnknown>(2)?);
    let method = js_to_rust_string(env, &ctx.get::<JsUnknown>(3)?);

    let mut req: Vec<u8> = Vec::with_capacity(64);

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
            crate::dotnet::bin_write_str16(&mut req, type_name.as_bytes());
            crate::dotnet::bin_write_str16(&mut req, assembly.as_bytes());
        }
    }

    // A release gives back the exports its proxy took; without a count the handle goes outright.
    if op == 0x04 && ctx.length > 4 {
        let count_v = ctx.get::<JsUnknown>(4)?;
        let count = match count_v.get_type()? {
            ValueType::Number => unsafe { count_v.cast::<JsNumber>() }.get_double()? as i32,
            _ => -1,
        };
        req.extend_from_slice(&count.to_le_bytes());
    }

    if op == 0x01 || op == 0x02 {
        crate::dotnet::bin_write_str16(&mut req, method.as_bytes());
    }

    if matches!(op, 0x01 | 0x02 | 0x03) {
        let arg_count = (ctx.length - 4) as u8;
        req.push(arg_count);
        for i in 4..4 + arg_count as usize {
            let a = ctx.get::<JsUnknown>(i)?;
            napi_bin_write_arg(env, &mut req, &a);
        }
    }

    release_collected_js_callbacks();
    match crate::dotnet::call_dotnet_binary_with(&req, |response| napi_bin_read_response(env, response)) {
        Ok(result) => result.map_err(napi::Error::from_reason),
        Err(e) => Err(napi::Error::from_reason(e)),
    }
}

/// Registers a JS function as a typed .NET delegate; returns the managed handle. Args:
/// `typeName` (string, `""` -> `System.Action`), `fn`.
fn native_create_delegate(ctx: &CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 2 {
        return Err(napi::Error::from_reason(
            "__nsDotNetCreateDelegate(typeName, fn): expected 2 arguments",
        ));
    }
    let env = &ctx.env;
    let type_name = js_to_rust_string(env, &ctx.get::<JsUnknown>(0)?);
    let func = ctx.get::<JsFunction>(1).map_err(|_| {
        napi::Error::from_reason("__nsDotNetCreateDelegate: second argument must be a function")
    })?;

    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    let status = unsafe { sys::napi_create_reference(env.raw(), func.raw(), 1, &mut func_ref) };
    if status != sys::Status::napi_ok || func_ref.is_null() {
        return Err(napi::Error::from_reason(
            "__nsDotNetCreateDelegate: failed to pin callback",
        ));
    }

    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    NAPI_DOTNET_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, (env.raw(), func_ref));
    });

    let mut req: Vec<u8> = Vec::with_capacity(32);
    req.push(0x09);
    crate::dotnet::bin_write_str16(&mut req, type_name.as_bytes());
    req.extend_from_slice(&cb_id.to_le_bytes());

    match crate::dotnet::call_dotnet_binary_with(&req, |response| napi_bin_read_response(env, response)) {
        Ok(result) => result.map_err(napi::Error::from_reason),
        Err(e) => Err(napi::Error::from_reason(e)),
    }
}

/// Reads a JS array of strings into a `Vec<String>`; a non-array (or a missing argument) yields
/// an empty vec rather than an error, matching the tolerant style of `napi_array_length`/`_get`.
fn napi_read_string_array(env: &Env, arg: &JsUnknown) -> Vec<String> {
    match napi_array_length(env, arg) {
        Some(len) => (0..len)
            .filter_map(|i| napi_array_get(env, arg, i))
            .map(|v| js_to_rust_string(env, &v))
            .collect(),
        None => Vec::new(),
    }
}

/// Create a managed subclass instance backed by JS overrides. Args: assembly (string, `""` for
/// none), typeName (string), interfaceNames (string[]), memberNames (string[]), fn (callback that
/// receives dispatched virtual/interface-member invocations). Mirrors the classic engine's
/// `handle_dotnet_create_js_subclass` exactly, including its "never cleans up the callback ref on
/// error" leak profile: parity, not a fix, matches this module's other natives.
fn native_create_js_subclass(ctx: &CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 5 {
        return Err(napi::Error::from_reason(
            "__nsDotNetCreateJsSubclass(assembly, typeName, interfaceNames, memberNames, fn): expected 5 arguments",
        ));
    }
    let env = &ctx.env;
    let assembly = js_to_rust_string(env, &ctx.get::<JsUnknown>(0)?);
    let type_name = js_to_rust_string(env, &ctx.get::<JsUnknown>(1)?);
    let interface_names = napi_read_string_array(env, &ctx.get::<JsUnknown>(2)?);
    let member_names = napi_read_string_array(env, &ctx.get::<JsUnknown>(3)?);

    let func = ctx.get::<JsFunction>(4).map_err(|_| {
        napi::Error::from_reason("__nsDotNetCreateJsSubclass: fifth argument must be a function")
    })?;

    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    let status = unsafe { sys::napi_create_reference(env.raw(), func.raw(), 1, &mut func_ref) };
    if status != sys::Status::napi_ok || func_ref.is_null() {
        return Err(napi::Error::from_reason(
            "__nsDotNetCreateJsSubclass: failed to pin callback",
        ));
    }

    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    NAPI_DOTNET_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, (env.raw(), func_ref));
    });

    let mut req: Vec<u8> = Vec::with_capacity(64);
    req.push(0x0A);
    crate::dotnet::bin_write_str16(&mut req, assembly.as_bytes());
    crate::dotnet::bin_write_str16(&mut req, type_name.as_bytes());
    req.extend_from_slice(&(interface_names.len() as i32).to_le_bytes());
    for name in &interface_names {
        crate::dotnet::bin_write_str16(&mut req, name.as_bytes());
    }
    req.extend_from_slice(&(member_names.len() as i32).to_le_bytes());
    for name in &member_names {
        crate::dotnet::bin_write_str16(&mut req, name.as_bytes());
    }
    req.extend_from_slice(&cb_id.to_le_bytes());
    // Constructor arguments (optional 6th argument, an array) for the base constructor.
    if ctx.length > 5 {
        let ctor_args = ctx.get::<JsUnknown>(5)?;
        if let Some(len) = napi_array_length(env, &ctor_args) {
            let len = len.min(255);
            req.push(len as u8);
            for k in 0..len {
                match napi_array_get(env, &ctor_args, k) {
                    Some(v) => napi_bin_write_arg(env, &mut req, &v),
                    None => req.push(0x00),
                }
            }
        }
    }

    release_collected_js_callbacks();
    match crate::dotnet::call_dotnet_binary_with(&req, |response| napi_bin_read_response(env, response)) {
        Ok(result) => result.map_err(napi::Error::from_reason),
        Err(e) => Err(napi::Error::from_reason(e)),
    }
}

/// `__nsWrapWinRTPointer(ptr, typeName)`: the WinRT wrapper for the composed instance behind a JS
/// subclass of a WinRT class (created by the bridge). Takes its own reference. Mirrors the classic
/// engine's `handle_wrap_winrt_pointer`.
fn native_wrap_winrt_pointer(ctx: &CallContext) -> napi::Result<JsUnknown> {
    let env = &ctx.env;
    let ptr_val = ctx.get::<JsUnknown>(0)?;
    let ptr = match ptr_val.get_type()? {
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { ptr_val.cast() };
            b.get_u64()?.0 as usize
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { ptr_val.cast() };
            n.get_double()? as usize
        }
        _ => 0,
    };
    if ptr == 0 {
        return Err(napi::Error::from_reason("__nsWrapWinRTPointer: null pointer"));
    }
    let type_name = js_to_rust_string(env, &ctx.get::<JsUnknown>(1)?);
    let declaration = metadata::meta_data_reader::MetadataReader::find_by_name(&type_name)
        .ok_or_else(|| napi::Error::from_reason(format!("__nsWrapWinRTPointer: unknown WinRT type {type_name}")))?;
    let instance: IUnknown = unsafe {
        let borrowed = ManuallyDrop::new(IUnknown::from_raw(ptr as *mut c_void));
        (*borrowed).clone()
    };
    let obj = crate::napi_engine::ns_proxy::create_instance_proxy(env, &type_name, declaration, instance.clone())?;
    crate::napi_engine::ns_proxy::mark_js_subclass_instance(&instance);
    Ok(as_unknown(env, obj))
}

fn native_await_task(ctx: &CallContext) -> napi::Result<()> {
    if ctx.length < 3 {
        return Err(napi::Error::from_reason(
            "__nsDotNetAwaitTask(handleId, resolve, reject): expected 3 arguments",
        ));
    }
    let env = &ctx.env;

    let handle_v = ctx.get::<JsUnknown>(0)?;
    let handle_id: i32 = match handle_v.get_type()? {
        ValueType::Number => {
            let n: JsNumber = unsafe { handle_v.cast() };
            n.get_double()? as i32
        }
        _ => {
            return Err(napi::Error::from_reason(
                "__nsDotNetAwaitTask: first argument must be a handle id (integer)",
            ));
        }
    };

    let resolve_fn = ctx.get::<JsFunction>(1).map_err(|_| {
        napi::Error::from_reason("__nsDotNetAwaitTask: second argument must be a resolve function")
    })?;
    let reject_fn = ctx.get::<JsFunction>(2).map_err(|_| {
        napi::Error::from_reason("__nsDotNetAwaitTask: third argument must be a reject function")
    })?;

    let mut resolve_ref: sys::napi_ref = std::ptr::null_mut();
    let mut reject_ref: sys::napi_ref = std::ptr::null_mut();
    unsafe {
        let _ = sys::napi_create_reference(env.raw(), resolve_fn.raw(), 1, &mut resolve_ref);
        let _ = sys::napi_create_reference(env.raw(), reject_fn.raw(), 1, &mut reject_ref);
    }
    if resolve_ref.is_null() || reject_ref.is_null() {
        return Err(napi::Error::from_reason(
            "__nsDotNetAwaitTask: failed to pin resolve/reject callbacks",
        ));
    }

    let resolve_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    let reject_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    NAPI_DOTNET_CALLBACKS.with(|m| {
        let mut map = m.borrow_mut();
        map.insert(resolve_id, (env.raw(), resolve_ref));
        map.insert(reject_id, (env.raw(), reject_ref));
    });

    // Binary instance call: 0x01 | handle(i32) | "__dotnet_await__"(str16) | 2 | i32 resolveId | i32 rejectId
    let mut req: Vec<u8> = Vec::with_capacity(32);
    req.push(0x01u8);
    req.extend_from_slice(&handle_id.to_le_bytes());
    crate::dotnet::bin_write_str16(&mut req, b"__dotnet_await__");
    req.push(2u8);
    req.push(0x03u8);
    req.extend_from_slice(&resolve_id.to_le_bytes());
    req.push(0x03u8);
    req.extend_from_slice(&reject_id.to_le_bytes());

    if let Err(e) = crate::dotnet::call_dotnet_binary(&req) {
        NAPI_DOTNET_CALLBACKS.with(|m| {
            let mut map = m.borrow_mut();
            map.remove(&resolve_id);
            map.remove(&reject_id);
        });
        unsafe {
            let _ = sys::napi_delete_reference(env.raw(), resolve_ref);
            let _ = sys::napi_delete_reference(env.raw(), reject_ref);
        }
        return Err(napi::Error::from_reason(e));
    }
    Ok(())
}

fn native_run_on_ui_thread(ctx: &CallContext) -> napi::Result<()> {
    if ctx.length < 1 {
        return Err(napi::Error::from_reason(
            "__nsRunOnUIThread(fn): expected a function argument",
        ));
    }
    let env = &ctx.env;
    let func = ctx
        .get::<JsFunction>(0)
        .map_err(|_| napi::Error::from_reason("__nsRunOnUIThread: argument must be a function"))?;

    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    let status = unsafe { sys::napi_create_reference(env.raw(), func.raw(), 1, &mut func_ref) };
    if status != sys::Status::napi_ok || func_ref.is_null() {
        return Err(napi::Error::from_reason(
            "__nsRunOnUIThread: failed to pin callback",
        ));
    }

    let cb_id = crate::DOTNET_NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
    NAPI_DOTNET_CALLBACKS.with(|m| {
        m.borrow_mut().insert(cb_id, (env.raw(), func_ref));
    });

    crate::ui_dispatcher::post_to_ui_thread(move || {
        let entry = NAPI_DOTNET_CALLBACKS.with(|m| m.borrow_mut().remove(&cb_id));
        let Some((env_raw, func_ref)) = entry else {
            return;
        };
        unsafe {
            let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
            if sys::napi_open_handle_scope(env_raw, &mut scope) != sys::Status::napi_ok {
                return;
            }
            let mut func: sys::napi_value = std::ptr::null_mut();
            if sys::napi_get_reference_value(env_raw, func_ref, &mut func) == sys::Status::napi_ok
                && !func.is_null()
            {
                let mut recv: sys::napi_value = std::ptr::null_mut();
                let _ = sys::napi_get_undefined(env_raw, &mut recv);
                let mut result: sys::napi_value = std::ptr::null_mut();
                let status = sys::napi_call_function(
                    env_raw,
                    recv,
                    func,
                    0,
                    std::ptr::null(),
                    &mut result,
                );
                if status != sys::Status::napi_ok {
                    let mut exc: sys::napi_value = std::ptr::null_mut();
                    if sys::napi_get_and_clear_last_exception(env_raw, &mut exc)
                        == sys::Status::napi_ok
                        && !exc.is_null()
                    {
                        if let Some(msg) = stringify_exception(env_raw, exc) {
                            crate::store_last_js_error(msg);
                        }
                    }
                }
            }
            let _ = sys::napi_delete_reference(env_raw, func_ref);
            let _ = sys::napi_close_handle_scope(env_raw, scope);
        }
    });
    Ok(())
}

unsafe fn stringify_exception(env: sys::napi_env, exc: sys::napi_value) -> Option<String> {
    let mut coerced: sys::napi_value = std::ptr::null_mut();
    if sys::napi_coerce_to_string(env, exc, &mut coerced) != sys::Status::napi_ok {
        return None;
    }
    let mut len = 0usize;
    if sys::napi_get_value_string_utf8(env, coerced, std::ptr::null_mut(), 0, &mut len)
        != sys::Status::napi_ok
    {
        return None;
    }
    let mut buf = vec![0u8; len + 1];
    let mut written = 0usize;
    if sys::napi_get_value_string_utf8(env, coerced, buf.as_mut_ptr() as *mut _, buf.len(), &mut written)
        != sys::Status::napi_ok
    {
        return None;
    }
    buf.truncate(written);
    String::from_utf8(buf).ok()
}

/// The `stack` of a thrown Error, when it has one.
unsafe fn exception_stack(env: sys::napi_env, exc: sys::napi_value) -> Option<String> {
    let mut ty: sys::napi_valuetype = 0;
    if sys::napi_typeof(env, exc, &mut ty) != sys::Status::napi_ok || ty != sys::ValueType::napi_object {
        return None;
    }
    let mut stack: sys::napi_value = std::ptr::null_mut();
    if sys::napi_get_named_property(env, exc, c"stack".as_ptr(), &mut stack) != sys::Status::napi_ok {
        return None;
    }
    if sys::napi_typeof(env, stack, &mut ty) != sys::Status::napi_ok || ty != sys::ValueType::napi_string {
        return None;
    }
    stringify_exception(env, stack)
}

/// `[count: u8] [tagged values...]` -> raw napi_values ready for `napi_call_function`.
fn parse_callback_args(env: &Env, bytes: &[u8]) -> Vec<sys::napi_value> {
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
        match napi_bin_read_value(env, bytes, &mut pos) {
            Ok(v) => result.push(unsafe { v.raw() }),
            Err(_) => break,
        }
    }
    result
}

/// Reports to the managed caller that a callback could not run (tag 0x0F + message).
unsafe fn write_error_response(message: &str, resp_ptr: *mut *mut u8, resp_len: *mut i32) {
    let mut buf = vec![0x0Fu8];
    crate::dotnet::bin_write_str32(&mut buf, message.as_bytes());
    let p = LocalAlloc(LMEM_FIXED, buf.len());
    if !resp_ptr.is_null() {
        *resp_ptr = if p.is_null() { std::ptr::null_mut() } else { p as *mut u8 };
    }
    if !p.is_null() {
        std::ptr::copy_nonoverlapping(buf.as_ptr(), p as *mut u8, buf.len());
    }
    if !resp_len.is_null() {
        *resp_len = if p.is_null() { 0 } else { buf.len() as i32 };
    }
}

/// Called by the managed bridge (via the stored function pointer) when a .NET delegate or task
/// fires. Runs on the JS thread: same pattern as the napi `JsDelegate` COM bridge
/// (`napi_engine::delegate`). Wire format for `args_ptr`: `[count: u8] [tagged values...]`.
///
/// # Safety
/// Must only be invoked by the managed bridge with the exact signature `crate::dotnet::FnJsCallback`
/// describes; `args_ptr`/`resp_ptr`/`resp_len` follow that contract.
pub(crate) unsafe extern "C" fn napi_invoke_dotnet_js_callback(
    callback_id: i32,
    args_ptr: *const u8,
    args_len: i32,
    resp_ptr: *mut *mut u8,
    resp_len: *mut i32,
) {
    let entry = NAPI_DOTNET_CALLBACKS.with(|m| m.borrow().get(&callback_id).copied());
    let Some((env_raw, func_ref)) = entry else {
        // A managed delegate, event, Task continuation or overridden member running on a thread
        // other than the JS thread: hand the call to the JS (UI) thread and wait for it, so the
        // callback runs and its result reaches the managed caller.
        if !crate::ui_dispatcher::is_ui_thread() {
            let (args, resp, len) = (args_ptr as usize, resp_ptr as usize, resp_len as usize);
            let delivered = crate::ui_dispatcher::run_on_ui_thread_sync(move || {
                napi_invoke_dotnet_js_callback(callback_id, args as *const u8, args_len, resp as *mut *mut u8, len as *mut i32)
            });
            if delivered.is_none() {
                write_error_response("", resp_ptr, resp_len);
            }
        }
        return;
    };
    if env_raw.is_null() {
        return;
    }

    let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
    if sys::napi_open_handle_scope(env_raw, &mut scope) != sys::Status::napi_ok {
        return;
    }

    let out_buf: Vec<u8> = (|| -> Vec<u8> {
        let mut func: sys::napi_value = std::ptr::null_mut();
        if sys::napi_get_reference_value(env_raw, func_ref, &mut func) != sys::Status::napi_ok
            || func.is_null()
        {
            return vec![0x00u8];
        }

        let env = Env::from_raw(env_raw);
        let args_slice = if args_len > 0 {
            std::slice::from_raw_parts(args_ptr, args_len as usize)
        } else {
            &[]
        };
        let js_args = parse_callback_args(&env, args_slice);

        let mut recv: sys::napi_value = std::ptr::null_mut();
        let _ = sys::napi_get_undefined(env_raw, &mut recv);
        let mut call_result: sys::napi_value = std::ptr::null_mut();
        let status = sys::napi_call_function(
            env_raw,
            recv,
            func,
            js_args.len(),
            js_args.as_ptr(),
            &mut call_result,
        );

        if status != sys::Status::napi_ok {
            let mut exc: sys::napi_value = std::ptr::null_mut();
            if sys::napi_get_and_clear_last_exception(env_raw, &mut exc) == sys::Status::napi_ok
                && !exc.is_null()
            {
                if let Some(msg) = stringify_exception(env_raw, exc) {
                    crate::store_last_js_error(msg.clone());
                    // 0x0F: the callback threw. Message, then JS stack (a JsException in .NET).
                    let mut buf = vec![0x0Fu8];
                    crate::dotnet::bin_write_str32(&mut buf, msg.as_bytes());
                    if let Some(stack) = exception_stack(env_raw, exc) {
                        crate::dotnet::bin_write_str32(&mut buf, stack.as_bytes());
                    }
                    return buf;
                }
            }
            return vec![0x00u8];
        }

        let mut buf = Vec::new();
        if call_result.is_null() {
            buf.push(0x00u8);
        } else {
            let unknown = JsUnknown::from_raw_unchecked(env_raw, call_result);
            napi_bin_write_value(&env, &mut buf, &unknown);
        }
        buf
    })();

    let _ = sys::napi_close_handle_scope(env_raw, scope);

    if out_buf.is_empty() {
        if !resp_ptr.is_null() {
            *resp_ptr = std::ptr::null_mut();
        }
        if !resp_len.is_null() {
            *resp_len = 0;
        }
        return;
    }

    // Managed frees this via Marshal.FreeHGlobal, so allocate with LocalAlloc for compatibility
    // (matches `global_fns::invoke_dotnet_js_callback` exactly).
    let size = out_buf.len();
    let p = LocalAlloc(LMEM_FIXED, size);
    if p.is_null() {
        if !resp_ptr.is_null() {
            *resp_ptr = std::ptr::null_mut();
        }
        if !resp_len.is_null() {
            *resp_len = 0;
        }
    } else {
        let dest = p as *mut u8;
        std::ptr::copy_nonoverlapping(out_buf.as_ptr(), dest, size);
        if !resp_ptr.is_null() {
            *resp_ptr = dest;
        }
        if !resp_len.is_null() {
            *resp_len = size as i32;
        }
    }
}

//

/// Install the `__nsDotNet*` / `__nsRunOnUIThread` natives + the `NSWinRT.dotnet` JS layer,
/// shared by the Node addon and every standalone engine. Idempotent. Registers this module's
/// `napi_invoke_dotnet_js_callback` as the dispatcher the managed bridge calls back into: see the
/// module doc comment.
pub fn install_dotnet(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;

    let has_bridge = matches!(
        global
            .get_named_property::<JsUnknown>("__nsDotNetInvokeBin")
            .and_then(|v| v.get_type()),
        Ok(ValueType::Function)
    );
    if has_bridge {
        return Ok(());
    }

    let invoke_fn =
        env.create_function_from_closure("__nsDotNetInvoke", |ctx: CallContext| {
            native_invoke(&ctx)
        })?;
    global.set_named_property("__nsDotNetInvoke", invoke_fn)?;

    let invoke_bin_fn =
        env.create_function_from_closure("__nsDotNetInvokeBin", |ctx: CallContext| {
            native_invoke_bin(&ctx)
        })?;
    global.set_named_property("__nsDotNetInvokeBin", invoke_bin_fn)?;

    let create_delegate_fn = env.create_function_from_closure(
        "__nsDotNetCreateDelegate",
        |ctx: CallContext| native_create_delegate(&ctx),
    )?;
    global.set_named_property("__nsDotNetCreateDelegate", create_delegate_fn)?;

    let create_js_subclass_fn = env.create_function_from_closure(
        "__nsDotNetCreateJsSubclass",
        |ctx: CallContext| native_create_js_subclass(&ctx),
    )?;
    global.set_named_property("__nsDotNetCreateJsSubclass", create_js_subclass_fn)?;

    let wrap_winrt_pointer_fn = env.create_function_from_closure(
        "__nsWrapWinRTPointer",
        |ctx: CallContext| native_wrap_winrt_pointer(&ctx),
    )?;
    global.set_named_property("__nsWrapWinRTPointer", wrap_winrt_pointer_fn)?;

    let await_task_fn =
        env.create_function_from_closure("__nsDotNetAwaitTask", |ctx: CallContext| {
            native_await_task(&ctx)
        })?;
    global.set_named_property("__nsDotNetAwaitTask", await_task_fn)?;

    let run_on_ui_fn =
        env.create_function_from_closure("__nsRunOnUIThread", |ctx: CallContext| {
            native_run_on_ui_thread(&ctx)
        })?;
    global.set_named_property("__nsRunOnUIThread", run_on_ui_fn)?;

    crate::dotnet::set_js_callback_dispatcher(napi_invoke_dotnet_js_callback);

    let func_ctor: JsFunction = global.get_named_property("Function")?;
    let body = env.create_string(DOTNET_HELPERS_JS)?;
    let installer_obj = func_ctor.new_instance(&[body])?;
    let installer: JsFunction = unsafe { JsFunction::from_raw(env.raw(), installer_obj.raw()) }?;
    installer.call_without_args(None)?;
    Ok(())
}

/// The JS half of `NSWinRT.dotnet` and the managed-subclass system, shared with the classic
/// engine (`runtime/src/js`). A no-op if `__nsDotNetInvokeBin` is absent.
const DOTNET_HELPERS_JS: &str = concat!(
    "'use strict';\n(function () {\nglobalThis.NSWinRT = globalThis.NSWinRT || {};\n",
    include_str!("../js/dotnet_subclass.js"),
    "})();\n",
    include_str!("../js/dotnet.js"),
);
