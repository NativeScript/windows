//! Node-API implementation of the .NET/BCL bridge exposed to JS as `NSWinRT.dotnet` (the classic
//! runtime exposes the same surface from `global_fns.rs`'s `HELPER_SOURCE`).
//!
//! `crate::dotnet` (the hostfxr-based CLR host, `call_dotnet` / `call_dotnet_binary`) has zero
//! `v8::` references and is reused completely unmodified here: see
//! `[[project_engine_framework_parity]]` for how that was confirmed. What this module supplies is
//! the engine-specific half: the same seven `__ns*` natives the classic engine registers in
//! `global_fns.rs`, reimplemented against Node-API instead of V8's C++ API, plus the JS half
//! mirrored verbatim (native names match exactly, so the JS needed no changes) into
//! [`DOTNET_HELPERS_JS`]. Keep the two JS copies in sync.
//!
//! Includes `__nsDotNetCreateJsSubclass` / `NSWinRT.proxy.createManagedSubclass` and the JS-side
//! managed-subclass system it belongs to (`makeManagedConstructor`, `__extends_winrt`,
//! `CSharpProxy`, `Interfaces`, proxy `emit`/`compile`/`register`): this lets JS both extend a C#
//! class (`Object.extend`/`Function.prototype.extend`/TS `extends`) and implement a WinRT
//! interface, the same as the classic engine, via `Bridge.Proxy.cs`'s `System.Reflection.Emit`
//! dynamic-subclass path (or a build-time `sbg`-generated static proxy when one exists: both
//! funnel through the same `CreateJsSubclass`, unaffected by which JS engine called it). The
//! `emit`/`compile`/`register` trio is the separate, still-dead ahead-of-time pipeline
//! (`hostCanLoadAssemblies: false`: compiles but its output is never loaded), ported for JS
//! API-surface parity only, not because it works.
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

/// Encode one outgoing method-call argument (`u16` string-length prefix; AddRefs `__native_ptr`).
/// Mirrors `global_fns::bin_write_v8_arg`.
fn napi_bin_write_arg(env: &Env, buf: &mut Vec<u8>, arg: &JsUnknown) {
    let vt = arg.get_type().unwrap_or(ValueType::Undefined);
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

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => napi_bin_read_response(env, &response).map_err(napi::Error::from_reason),
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

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => napi_bin_read_response(env, &response).map_err(napi::Error::from_reason),
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

    match crate::dotnet::call_dotnet_binary(&req) {
        Ok(response) => napi_bin_read_response(env, &response).map_err(napi::Error::from_reason),
        Err(e) => Err(napi::Error::from_reason(e)),
    }
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
                    crate::store_last_js_error(msg);
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

/// The JS half of `NSWinRT.dotnet`, copied verbatim from the classic runtime's
/// `global_fns.rs` `HELPER_SOURCE` (the native names match exactly, so nothing needed adapting):
/// keep the two in sync. Requires the dotnet-bridge project to be published into
/// `<app-root>/dotnet-bridge/publish/DotNetBridge.dll`; a no-op if `__nsDotNetInvokeBin` is absent.
const DOTNET_HELPERS_JS: &str = r#"
'use strict';
(function () {
    // Managed-subclass / proxy system, ported verbatim from the classic engine's
    // `HELPER_SOURCE` (the giant bootstrap IIFE spanning `var proxyExtensions = []` through the
    // `NSWinRT.proxy = {...}` object literal): see the module doc comment. Runs unconditionally,
    // same as classic: `Function.prototype.extend`/`Object.extend`/pure-JS `makeExtendedConstructor`
    // fallback are useful even without a .NET bridge (e.g. WinRT-only class extension). Only
    // `createManagedSubclass` itself requires `__nsDotNetCreateJsSubclass`, which by the time this
    // whole script runs is guaranteed to already be registered (both are installed together, see
    // `install_dotnet` below): so no extra guard is needed here.
    globalThis.NSWinRT = globalThis.NSWinRT || {};

    var proxyExtensions = [];
    var proxyInstances = new Map();
    var nextProxyId = 1;

    function ctorName(ctor) {
        return (ctor && (ctor.__winrtProxyName__ || ctor.__typeName__ || ctor.name)) || 'Object';
    }

    // Wraps a raw tagged handle value into a live DotNet instance proxy exactly like the
    // NSWinRT.dotnet block below's own private `_wrap`: reused here via the public `wrap` alias
    // that block exposes on `NSWinRT.dotnet` (see below), since this code has no direct access to
    // that other IIFE's closure. Both IIFEs run synchronously during the same `install_dotnet`
    // call, so by the time createManagedSubclass is actually CALLED, NSWinRT.dotnet.wrap exists.
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

    // @Interfaces([IFoo, IBar]): attach WinRT interface list to a class.
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
            // prefixed property overrides: matching the literal CLR accessor names the
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
})();

(function () {
    if (typeof globalThis.__nsDotNetInvokeBin !== 'function') return;
    // installDotnet() is independently callable (unlike the classic engine's single monolithic
    // HELPER_SOURCE, where some earlier block always creates NSWinRT first): don't assume
    // install_interop ran first.
    globalThis.NSWinRT = globalThis.NSWinRT || {};

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
                // Write-only: has setter but no getter: reading it is an error.
                if (i.writeonlyProperties && i.writeonlyProperties.indexOf(prop) >= 0)
                    throw new TypeError('Cannot read write-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                if (i.properties && i.properties.indexOf(prop) >= 0)
                    return _wrap(_invoke({ handle: handle, method: 'get_' + prop, args: [] }));
                // Not a native property: return a callable for method dispatch.
                return function () {
                    var args = Array.prototype.slice.call(arguments).map(_unwrap);
                    return _wrap(_invoke({ handle: handle, method: prop, args: args }));
                };
            },
            set: function (_, prop, value) {
                if (typeof prop === 'symbol') return true;
                var i = _typeInfoCache[typeName] || _emptyInfo;
                // Read-only: has getter but no setter: assignment is an error.
                if (i.readonlyProperties && i.readonlyProperties.indexOf(prop) >= 0)
                    throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + typeName + '\'');
                // Writable (read-write or write-only): invoke the setter.
                if ((i.properties && i.properties.indexOf(prop) >= 0) ||
                    (i.writeonlyProperties && i.writeonlyProperties.indexOf(prop) >= 0))
                    _invoke({ handle: handle, method: 'set_' + prop, args: [_unwrap(value)] });
                // Not a native property: don't intercept, let JS do its thing.
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
                // Write-only static property: reading it is an error.
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
                // Read-only static property: assignment is an error.
                if (info.readonlyStaticProperties && info.readonlyStaticProperties.indexOf(prop) >= 0)
                    throw new TypeError('Cannot assign to read-only property \'' + prop + '\' of .NET type \'' + path + '\'');
                // Writable (read-write or write-only): invoke the setter.
                if ((info.staticProperties && info.staticProperties.indexOf(prop) >= 0) ||
                    (info.writeonlyStaticProperties && info.writeonlyStaticProperties.indexOf(prop) >= 0))
                    _invoke({ assembly: assembly, typeName: path, method: 'set_' + prop, args: [_unwrap(value)] });
                // Not a native property: don't intercept.
                return true;
            },
            apply: function (_, _this, args) {
                var lastDot  = path.lastIndexOf('.');
                // No dot means this is a top-level name being called directly:
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
"#;
