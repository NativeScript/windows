//! Node-API implementation of the JS <-> `NativeValue` marshaling used by the WinRT call paths.
//!
//! `NativeValue` (a `#[repr(C)]` union consumed by libffi via `as_arg`) is engine-neutral, so
//! these parsers must produce exactly the same native representation as the rusty_v8 parsers in
//! `crate::value`. Coercion semantics — including V8's integer *truncation* quirks (e.g. a
//! JS `300` marshaled to `u8` yields `44`, matching `v8::Uint32::value() as u8`) — are
//! replicated bit-for-bit so the two backends are interchangeable.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

// NB: do not glob-import `napi::bindgen_prelude::*` — it shadows `Result` with napi's own
// alias (`Result<T, E> = std::result::Result<T, napi::Error<E>>`), which silently rewraps our
// `Result<_, AnyError>` return types. Import concrete items instead.
use napi::sys;
use napi::{
    Env, JsBigInt, JsBoolean, JsFunction, JsNumber, JsObject, JsString, JsUnknown,
    NapiRaw, NapiValue, ValueType,
};
use windows::core::{IUnknown, Interface, GUID, HSTRING};
use windows::Foundation::PropertyValue;

use crate::dotnet::call_dotnet;
use crate::error::{type_error, AnyError};
use crate::value::{
    parse_guid_str, NativeType, NativeValue, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, OUT_PARAM_MARKER,
};
use crate::DeclarationFFI;

// 

#[inline]
pub(crate) fn value_type(v: &JsUnknown) -> Result<ValueType, AnyError> {
    v.get_type().map_err(|e| type_error(e.to_string()))
}

/// The JS value as an `f64`, or a type error, with no coercion — mirrors
/// `v8::Local::<v8::Number>::try_from`, which only succeeds on an actual Number.
#[inline]
fn require_number(v: &JsUnknown, msg: &'static str) -> Result<f64, AnyError> {
    if value_type(v)? != ValueType::Number {
        return Err(type_error(msg));
    }
    let n: JsNumber = unsafe { v.cast() };
    n.get_double().map_err(|e| type_error(e.to_string()))
}

/// Wrap any napi value type back into a `JsUnknown` handle (napi-rs has no blanket
/// `into_unknown`, so go through the raw env+value pointers we already hold).
#[inline]
pub(crate) fn as_unknown<T: NapiRaw>(env: &Env, v: T) -> JsUnknown {
    unsafe { JsUnknown::from_raw_unchecked(env.raw(), v.raw()) }
}

// 

#[inline]
pub fn napi_parse_bool(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    // `v8::Boolean::try_from` accepts only a real boolean — no truthiness coercion.
    if value_type(arg)? != ValueType::Boolean {
        return Err(type_error("Invalid FFI u8 type, expected boolean"));
    }
    let b: JsBoolean = unsafe { arg.cast() };
    Ok(NativeValue {
        bool_value: b.get_value().map_err(|e| type_error(e.to_string()))?,
    })
}

#[inline]
pub fn napi_parse_u8(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI u8 type, expected unsigned integer")?;
    // v8 path: Uint32 (any non-negative int <= u32::MAX) truncated to u8; Number fallback
    // range-checked to u8. The Uint32 branch subsumes the fallback, so: accept a non-negative
    // integer in u32 range and truncate.
    if f.fract() == 0.0 && f >= 0.0 && f <= u32::MAX as f64 {
        return Ok(NativeValue {
            u8_value: (f as u32) as u8,
        });
    }
    Err(type_error("Invalid FFI u8 type, expected unsigned integer"))
}

#[inline]
pub fn napi_parse_i8(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI i8 type, expected integer")?;
    // v8 path: Int32 truncated to i8; Number fallback range-checked. Int32 branch subsumes.
    if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
        return Ok(NativeValue {
            i8_value: (f as i32) as i8,
        });
    }
    Err(type_error("Invalid FFI i8 type, expected integer"))
}

#[inline]
pub fn napi_parse_u16(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI u16 type, expected unsigned integer")?;
    if f.fract() == 0.0 && f >= 0.0 && f <= u32::MAX as f64 {
        return Ok(NativeValue {
            u16_value: (f as u32) as u16,
        });
    }
    Err(type_error("Invalid FFI u16 type, expected unsigned integer"))
}

#[inline]
pub fn napi_parse_i16(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI i16 type, expected integer")?;
    if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
        return Ok(NativeValue {
            i16_value: (f as i32) as i16,
        });
    }
    Err(type_error("Invalid FFI i16 type, expected integer"))
}

#[inline]
pub fn napi_parse_u32(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI u32 type, expected unsigned integer")?;
    if f.fract() == 0.0 && f >= 0.0 && f <= u32::MAX as f64 {
        return Ok(NativeValue { u32_value: f as u32 });
    }
    Err(type_error("Invalid FFI u32 type, expected unsigned integer"))
}

#[inline]
pub fn napi_parse_i32(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI i32 type, expected integer")?;
    if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
        return Ok(NativeValue { i32_value: f as i32 });
    }
    Err(type_error("Invalid FFI i32 type, expected integer"))
}

/// ToInteger-style truncation matching V8's `Number::integer_value` (trunc toward zero,
/// saturating at i64 bounds; NaN -> 0). Rust's `f as i64` is saturating since 1.45.
#[inline]
fn number_to_i64(f: f64) -> i64 {
    f as i64
}

#[inline]
pub fn napi_parse_u64(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let u64_value: u64 = match value_type(arg)? {
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { arg.cast() };
            b.get_u64().map_err(|e| type_error(e.to_string()))?.0
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            number_to_i64(n.get_double().map_err(|e| type_error(e.to_string()))?) as u64
        }
        _ => return Err(type_error("Invalid FFI u64 type, expected unsigned integer")),
    };
    Ok(NativeValue { u64_value })
}

#[inline]
pub fn napi_parse_i64(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let i64_value: i64 = match value_type(arg)? {
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { arg.cast() };
            b.get_i64().map_err(|e| type_error(e.to_string()))?.0
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            number_to_i64(n.get_double().map_err(|e| type_error(e.to_string()))?)
        }
        _ => return Err(type_error("Invalid FFI i64 type, expected integer")),
    };
    Ok(NativeValue { i64_value })
}

#[inline]
pub fn napi_parse_usize(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let usize_value: usize = match value_type(arg)? {
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { arg.cast() };
            b.get_u64().map_err(|e| type_error(e.to_string()))?.0 as usize
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            number_to_i64(n.get_double().map_err(|e| type_error(e.to_string()))?) as usize
        }
        _ => return Err(type_error("Invalid FFI usize type, expected integer")),
    };
    Ok(NativeValue { usize_value })
}

#[inline]
pub fn napi_parse_isize(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let isize_value: isize = match value_type(arg)? {
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { arg.cast() };
            b.get_i64().map_err(|e| type_error(e.to_string()))?.0 as isize
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { arg.cast() };
            number_to_i64(n.get_double().map_err(|e| type_error(e.to_string()))?) as isize
        }
        _ => return Err(type_error("Invalid FFI isize type, expected integer")),
    };
    Ok(NativeValue { isize_value })
}

#[inline]
pub fn napi_parse_f32(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI f32 type, expected number")?;
    Ok(NativeValue {
        f32_value: f as f32,
    })
}

#[inline]
pub fn napi_parse_f64(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let f = require_number(arg, "Invalid FFI f64 type, expected number")?;
    Ok(NativeValue { f64_value: f })
}

#[inline]
pub fn napi_parse_string(arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    // `v8::String::try_from` requires an actual string (no ToString coercion). The rusty_v8
    // `__hstring_ptr` fast path is dead code (its producer is unused), so this implementation
    // intentionally omits it.
    if value_type(arg)? != ValueType::String {
        return Err(type_error("Invalid FFI String type, expected String"));
    }
    let s: JsString = unsafe { arg.cast() };
    let utf16 = s.into_utf16().map_err(|e| type_error(e.to_string()))?;
    // napi's UTF-16 slice includes the trailing NUL terminator; the rusty_v8 path used
    // `s.length()` code units (no terminator), so drop it to keep HSTRING content identical.
    let slice = utf16.as_slice();
    let slice = slice.strip_suffix(&[0u16]).unwrap_or(slice);
    // Copy UTF-16 straight across (V8 strings and HSTRING are both UTF-16), no transcode.
    let hstring = HSTRING::from_wide(slice);
    Ok(NativeValue {
        string: ManuallyDrop::new(hstring),
    })
}

// 

/// Converts a `NativeValue` to a JS value for `native_type` — the napi counterpart of
/// `NativeValue::to_v8`, following the same union-to-JS mapping. `native_type` must match the
/// union's active field.
///
/// # Safety
/// The caller guarantees `native_type` corresponds to the initialized union variant.
pub unsafe fn native_value_to_napi(
    env: &Env,
    value: &NativeValue,
    native_type: &NativeType,
) -> Result<JsUnknown, AnyError> {
    let map = |e: napi::Error| type_error(e.to_string());
    let out = match native_type {
        NativeType::Void => as_unknown(env, env.get_undefined().map_err(map)?),
        NativeType::Bool => as_unknown(env, env.get_boolean(value.bool_value).map_err(map)?),
        NativeType::U8 => as_unknown(env, env.create_uint32(value.u8_value as u32).map_err(map)?),
        NativeType::I8 => as_unknown(env, env.create_int32(value.i8_value as i32).map_err(map)?),
        NativeType::U16 => as_unknown(env, env.create_uint32(value.u16_value as u32).map_err(map)?),
        NativeType::I16 => as_unknown(env, env.create_int32(value.i16_value as i32).map_err(map)?),
        NativeType::U32 => as_unknown(env, env.create_uint32(value.u32_value).map_err(map)?),
        NativeType::I32 => as_unknown(env, env.create_int32(value.i32_value).map_err(map)?),
        NativeType::U64 => {
            let v = value.u64_value;
            if v > MAX_SAFE_INTEGER as u64 {
                as_unknown(env, env.create_bigint_from_u64(v).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(v as f64).map_err(map)?)
            }
        }
        NativeType::I64 => {
            let v = value.i64_value;
            if v > MAX_SAFE_INTEGER as i64 || v < MIN_SAFE_INTEGER as i64 {
                as_unknown(env, env.create_bigint_from_i64(v).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(v as f64).map_err(map)?)
            }
        }
        NativeType::USize => {
            let v = value.usize_value;
            if v > MAX_SAFE_INTEGER as usize {
                as_unknown(env, env.create_bigint_from_u64(v as u64).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(v as f64).map_err(map)?)
            }
        }
        NativeType::ISize => {
            let v = value.isize_value;
            if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&v) {
                as_unknown(env, env.create_bigint_from_i64(v as i64).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(v as f64).map_err(map)?)
            }
        }
        NativeType::F32 => as_unknown(env, env.create_double(value.f32_value as f64).map_err(map)?),
        NativeType::F64 => as_unknown(env, env.create_double(value.f64_value).map_err(map)?),
        NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
            if value.pointer.is_null() {
                as_unknown(env, env.get_null().map_err(map)?)
            } else {
                // Opaque native pointer handed back to JS as an external (V8 used v8::External).
                let ptr = value.pointer as usize;
                as_unknown(env, env.create_external(ptr, None).map_err(map)?)
            }
        }
        NativeType::Struct(_) => as_unknown(env, env.get_null().map_err(map)?),
        NativeType::String => {
            // HSTRING derefs to [u16]; `&*` yields the UTF-16 slice (as in `NativeValue::to_v8`).
            let js = env.create_string_utf16(&value.string).map_err(map)?;
            as_unknown(env, js)
        }
    };
    Ok(out)
}

/// Dispatch a JS value to the right parser for `native_type`. Mirrors the call sites in
/// `method_call`/`property_call` that fan out on the parameter's `NativeType`.
pub fn napi_parse_arg(
    env: &Env,
    arg: &JsUnknown,
    native_type: &NativeType,
) -> Result<NativeValue, AnyError> {
    match native_type {
        NativeType::Bool => napi_parse_bool(arg),
        NativeType::U8 => napi_parse_u8(arg),
        NativeType::I8 => napi_parse_i8(arg),
        NativeType::U16 => napi_parse_u16(arg),
        NativeType::I16 => napi_parse_i16(arg),
        NativeType::U32 => napi_parse_u32(arg),
        NativeType::I32 => napi_parse_i32(arg),
        NativeType::U64 => napi_parse_u64(arg),
        NativeType::I64 => napi_parse_i64(arg),
        NativeType::USize => napi_parse_usize(arg),
        NativeType::ISize => napi_parse_isize(arg),
        NativeType::F32 => napi_parse_f32(arg),
        NativeType::F64 => napi_parse_f64(arg),
        NativeType::String => napi_parse_string(arg),
        NativeType::Pointer => napi_parse_pointer(env, arg),
        NativeType::Buffer => napi_parse_buffer(env, arg),
        NativeType::Function => napi_parse_function(env, arg),
        NativeType::Struct(_) => napi_parse_struct(env, arg),
        NativeType::Void => Err(type_error("napi_parse_arg: Void is not a parameter type")),
    }
}

// 

/// Re-wrap the same napi handle as a fresh `JsUnknown` (napi-rs coercions consume `self`).
#[inline]
pub(crate) fn dup(env: &Env, v: &JsUnknown) -> JsUnknown {
    unsafe { JsUnknown::from_raw_unchecked(env.raw(), v.raw()) }
}

/// Clear any pending JS exception (napi-rs 2.x doesn't wrap this `node_api.h` call).
#[inline]
pub(crate) fn clear_pending_exception(env: &Env) {
    unsafe {
        let mut result: sys::napi_value = std::ptr::null_mut();
        let _ = sys::napi_get_and_clear_last_exception(env.raw(), &mut result);
    }
}

/// Throw a JS Error with `msg`.
pub fn throw_js_error(env: &Env, msg: &str) {
    let cmsg = std::ffi::CString::new(msg).unwrap_or_default();
    unsafe {
        let _ = sys::napi_throw_error(env.raw(), std::ptr::null(), cmsg.as_ptr());
    }
}

/// ToNumber coercion, mirroring `v8::Value::number_value` (None on failure, e.g. Symbol).
/// A failed coercion leaves a pending JS exception in the env; clear it so later napi calls
/// don't observe it — rusty_v8 callers relied on an outer TryCatch for the same effect.
#[inline]
fn coerce_f64(env: &Env, v: &JsUnknown) -> Option<f64> {
    match dup(env, v).coerce_to_number().and_then(|n| n.get_double()) {
        Ok(d) => Some(d),
        Err(_) => {
            clear_pending_exception(env);
            None
        }
    }
}

/// ToBoolean coercion, mirroring `v8::Value::boolean_value` (never throws per spec).
#[inline]
pub(crate) fn coerce_bool(env: &Env, v: &JsUnknown) -> bool {
    dup(env, v)
        .coerce_to_bool()
        .and_then(|b| b.get_value())
        .unwrap_or(false)
}

/// UTF-16 code units of a JS string handle, with napi's trailing NUL stripped.
#[inline]
fn string_utf16(s: JsString) -> Result<Vec<u16>, AnyError> {
    let utf16 = s.into_utf16().map_err(|e| type_error(e.to_string()))?;
    let slice = utf16.as_slice();
    let slice = slice.strip_suffix(&[0u16]).unwrap_or(slice);
    Ok(slice.to_vec())
}

/// ToString coercion then HSTRING, mirroring `hstring_from_v8_value` (empty on failure).
#[inline]
fn hstring_from_js_value(env: &Env, v: &JsUnknown) -> HSTRING {
    match dup(env, v).coerce_to_string() {
        Ok(s) => match string_utf16(s) {
            Ok(units) => HSTRING::from_wide(&units),
            Err(_) => HSTRING::new(),
        },
        Err(_) => {
            clear_pending_exception(env);
            HSTRING::new()
        }
    }
}

/// ToString coercion to a Rust String, mirroring `to_rust_string_lossy`.
#[inline]
pub(crate) fn js_to_rust_string(env: &Env, v: &JsUnknown) -> String {
    match dup(env, v).coerce_to_string() {
        Ok(s) => s
            .into_utf8()
            .ok()
            .and_then(|u| u.as_str().ok().map(|s| s.to_owned()))
            .unwrap_or_default(),
        Err(_) => {
            clear_pending_exception(env);
            String::new()
        }
    }
}

/// Read a JS value as i64, accepting Number or BigInt. i64 struct fields (DateTime.UniversalTime)
/// exceed 2^53, so callers pass a BigInt for full precision.
fn js_to_i64(env: &Env, v: &JsUnknown) -> i64 {
    if let Ok(ValueType::BigInt) = v.get_type() {
        let b: JsBigInt = unsafe { v.cast() };
        if let Ok((val, _)) = b.get_i64() {
            return val;
        }
        return 0;
    }
    coerce_f64(env, v).unwrap_or(0.0) as i64
}

/// Read an i64 field (Number or BigInt) off a JS object; 0 if absent.
fn read_i64_field(env: &Env, obj_val: &JsUnknown, field: &str) -> i64 {
    let vt = obj_val.get_type().ok();
    if vt == Some(ValueType::Object) || vt == Some(ValueType::Function) {
        let obj: JsObject = unsafe { obj_val.cast() };
        if let Ok(v) = obj.get_named_property::<JsUnknown>(field) {
            return js_to_i64(env, &v);
        }
    }
    0
}

// 

// Convention: raw native pointers cross into JS as a napi external boxing a `usize`. Every
// external this engine creates or reads goes through these two helpers so the tag type is
// uniform (napi-rs type-checks externals against the Rust type they were created with).

#[inline]
pub fn external_from_ptr(env: &Env, ptr: *mut c_void) -> Result<JsUnknown, AnyError> {
    let ext = env
        .create_external(ptr as usize, None)
        .map_err(|e| type_error(e.to_string()))?;
    Ok(as_unknown(env, ext))
}

/// Layout of the payload napi-rs boxes behind every `env.create_external` value. It mirrors
/// napi 2.16.17 `src/js_values/tagged_object.rs` (`TaggedObject<T>`), whose fields are private
/// to napi-rs, so the same shape is declared here to read the payload back through one raw
/// `napi_get_value_external` plus a `TypeId` compare instead of `env.get_value_external`
/// (which allocates an Error String on every type mismatch).
#[repr(C)]
pub(crate) struct Tagged<T> {
    type_id: std::any::TypeId,
    object: Option<T>,
}

/// The `T` payload of a napi-rs external whose raw data pointer is `raw`, if the external was
/// created for exactly that type.
///
/// # Safety
/// `raw` must be the data pointer of a live external created by napi-rs `create_external`.
#[inline]
pub(crate) unsafe fn tagged_payload<'a, T: 'static>(raw: *mut c_void) -> Option<&'a T> {
    if raw.is_null() {
        return None;
    }
    if *(raw as *const std::any::TypeId) != std::any::TypeId::of::<T>() {
        return None;
    }
    (*(raw as *const Tagged<T>)).object.as_ref()
}

/// NUL-terminated property key for the `handle` slot every wrapper carries; passing a static
/// key to `napi_get_named_property` avoids the per-call `CString` that napi-rs's
/// `get_named_property` allocates.
pub(crate) const HANDLE_KEY: &[u8] = b"handle\0";

/// `obj[key]` through `napi_get_named_property` with a static NUL-terminated key. `None` when
/// the read fails (a pending exception from a throwing getter is left for the caller's usual
/// error handling, exactly as with napi-rs's `get_named_property`).
#[inline]
pub(crate) fn get_named_property_raw(
    env: &Env,
    obj: sys::napi_value,
    key: &'static [u8],
) -> Option<sys::napi_value> {
    debug_assert_eq!(key.last(), Some(&0u8));
    let mut out: sys::napi_value = std::ptr::null_mut();
    let st = unsafe {
        sys::napi_get_named_property(
            env.raw(),
            obj,
            key.as_ptr() as *const std::ffi::c_char,
            &mut out,
        )
    };
    if st != sys::Status::napi_ok || out.is_null() {
        return None;
    }
    Some(out)
}

/// `napi_typeof` on a raw value; `None` when the call fails.
#[inline]
pub(crate) fn raw_typeof(env: &Env, v: sys::napi_value) -> Option<sys::napi_valuetype> {
    let mut vt: sys::napi_valuetype = 0;
    let st = unsafe { sys::napi_typeof(env.raw(), v, &mut vt) };
    (st == sys::Status::napi_ok).then_some(vt)
}

/// Raw data pointer of an external; the caller guarantees `typeof v === External`.
#[inline]
pub(crate) fn external_data_raw(env: &Env, v: sys::napi_value) -> Option<*mut c_void> {
    let mut out: *mut c_void = std::ptr::null_mut();
    let st = unsafe { sys::napi_get_value_external(env.raw(), v, &mut out) };
    if st != sys::Status::napi_ok || out.is_null() {
        return None;
    }
    Some(out)
}

/// The COM/handle pointer carried by an external the caller has already identified as such
/// (`typeof === External`). One `napi_get_value_external`, then a `TypeId` dispatch: host
/// instances carry a `HostHandle` (checked first, the hot case), everything else boxes a
/// `usize`. Never constructs a napi-rs Error.
#[inline]
pub(crate) fn external_ptr_raw(env: &Env, v: sys::napi_value) -> Option<*mut c_void> {
    let data = external_data_raw(env, v)?;
    unsafe {
        if let Some(hh) = tagged_payload::<crate::napi_engine::ns_hostobject::HostHandle>(data) {
            return Some(hh.ptr());
        }
        tagged_payload::<usize>(data).map(|p| *p as *mut c_void)
    }
}

/// The `HostHandle` carried by an external the caller has already identified as such.
#[inline]
pub(crate) fn host_handle_from_external<'a>(
    env: &Env,
    v: sys::napi_value,
) -> Option<&'a crate::napi_engine::ns_hostobject::HostHandle> {
    let data = external_data_raw(env, v)?;
    unsafe { tagged_payload::<crate::napi_engine::ns_hostobject::HostHandle>(data) }
}

#[inline]
pub fn ptr_from_external(env: &Env, v: &JsUnknown) -> Option<*mut c_void> {
    if v.get_type().ok()? != ValueType::External {
        return None;
    }
    external_ptr_raw(env, unsafe { v.raw() })
}

/// null → JS null, else a pointer external.
#[inline]
fn external_or_null(env: &Env, ptr: *mut c_void) -> Result<JsUnknown, AnyError> {
    if ptr.is_null() {
        Ok(as_unknown(
            env,
            env.get_null().map_err(|e| type_error(e.to_string()))?,
        ))
    } else {
        external_from_ptr(env, ptr)
    }
}

// 

/// Interpret a JS value as a raw native pointer: External, null, BigInt, or Number.
/// Mirrors the acceptance set of the `handle` branches in the rusty_v8 original
/// (undefined and anything else → None, NOT a null pointer).
fn js_value_as_ptr(env: &Env, v: &JsUnknown) -> Option<*mut c_void> {
    match v.get_type().ok()? {
        ValueType::External => external_ptr_raw(env, unsafe { v.raw() }),
        ValueType::Null => Some(std::ptr::null_mut()),
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { v.cast() };
            b.get_u64().ok().map(|(u, _)| u as *mut c_void)
        }
        ValueType::Number => {
            let n: JsNumber = unsafe { v.cast() };
            // v8 used `integer_value` (ToInteger: trunc toward zero, saturating, NaN→0);
            // `f64 as i64` has identical semantics for an actual Number.
            n.get_double().ok().map(|d| d as i64 as isize as *mut c_void)
        }
        _ => None,
    }
}

/// Extract a native COM/handle pointer from a JS object, trying in order:
/// 1. `handle` property — accessor function result or direct External/null/BigInt/Number.
/// 2. Wrapped `DeclarationFFI` (rusty_v8 kept it in internal field 0; here ns_proxy wraps
///    instance objects via `env.wrap`, read back with `env.unwrap`).
/// 3. `__native_ptr` property — hex/decimal string, BigInt, Number, or External.
/// 4. `__handle` managed-bridge id — resolved via `Bridge.GetNativePtrForHandle`.
pub(crate) fn try_get_external_handle(env: &Env, obj: &JsObject) -> Option<*mut c_void> {
    if let Some(handle_raw) = get_named_property_raw(env, unsafe { obj.raw() }, HANDLE_KEY) {
        // Host instances and pointer externals: the hot case. One typeof, one raw external
        // read, no napi-rs Error construction (the JSC shim's napi_get_value_external assumes
        // an object, so the typeof guard must stay in front of it).
        if raw_typeof(env, handle_raw) == Some(sys::ValueType::napi_external) {
            if let Some(p) = external_ptr_raw(env, handle_raw) {
                return Some(p);
            }
        }
        let handle = unsafe { JsUnknown::from_raw_unchecked(env.raw(), handle_raw) };
        if let Ok(ValueType::Function) = handle.get_type() {
            // Managed wrappers commonly expose the handle behind a small accessor; calling it
            // is the explicit bridge contract. `this` = the object so typical getters work.
            let func: JsFunction = unsafe { handle.cast() };
            let no_args: [JsUnknown; 0] = [];
            match func.call(Some(obj), &no_args) {
                Ok(ret) => {
                    if let Some(p) = js_value_as_ptr(env, &ret) {
                        return Some(p);
                    }
                }
                Err(_) => {
                    clear_pending_exception(env);
                }
            }
        } else if let Some(p) = js_value_as_ptr(env, &handle) {
            return Some(p);
        }
    }

    if let Ok(dec) = env.unwrap::<DeclarationFFI>(obj) {
        if let Some(ref instance) = dec.instance {
            return Some(instance.as_raw() as *mut c_void);
        }
        // Struct objects have no COM instance; return a pointer to the raw byte buffer so
        // WinRT setters receive the struct data by reference.
        if let Some((ref buf, _)) = dec.struct_instance {
            return Some(buf.as_ptr() as *mut c_void);
        }
        return Some(std::ptr::null_mut());
    }

    if let Ok(val) = obj.get_named_property::<JsUnknown>("__native_ptr") {
        if let Ok(ValueType::String) = val.get_type() {
            let s = js_to_rust_string(env, &val);
            let s_trim = s.trim_start();
            let parsed = if s_trim.starts_with("0x") || s_trim.starts_with("0X") {
                usize::from_str_radix(&s_trim[2..], 16).ok()
            } else {
                s_trim.parse::<usize>().ok()
            };
            if let Some(u) = parsed {
                return Some(u as *mut c_void);
            }
        }
        if let Some(p) = js_value_as_ptr(env, &val) {
            // BigInt / Number / External forms of __native_ptr (null is not meaningful here,
            // but the original accepted numeric zero the same way).
            return Some(p);
        }
    }

    // Managed handle id → ask the bridge for the canonical native pointer.
    if let Ok(val) = obj.get_named_property::<JsUnknown>("__handle") {
        let mut handle_id: Option<i32> = None;
        match val.get_type() {
            Ok(ValueType::Number) => {
                let n: JsNumber = unsafe { val.cast() };
                handle_id = n.get_double().ok().map(|d| d as i64 as i32);
            }
            Ok(ValueType::BigInt) => {
                let b: JsBigInt = unsafe { val.cast() };
                handle_id = b.get_u64().ok().map(|(u, _)| u as i32);
            }
            Ok(ValueType::Object) => {
                // Some bridges nest the handle in an inner __handle property.
                let inner_obj: JsObject = unsafe { val.cast() };
                if let Ok(inner) = inner_obj.get_named_property::<JsUnknown>("__handle") {
                    match inner.get_type() {
                        Ok(ValueType::Number) => {
                            let n: JsNumber = unsafe { inner.cast() };
                            handle_id = n.get_double().ok().map(|d| d as i64 as i32);
                        }
                        Ok(ValueType::BigInt) => {
                            let b: JsBigInt = unsafe { inner.cast() };
                            handle_id = b.get_u64().ok().map(|(u, _)| u as i32);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if let Some(id) = handle_id {
            let req = format!(
                "{{\"assembly\":null,\"typeName\":\"NativeScriptBridge.Bridge\",\"method\":\"GetNativePtrForHandle\",\"args\":[{}]}}",
                id
            );
            if let Ok(resp) = call_dotnet(&req) {
                let trimmed = resp.trim();
                if !trimmed.is_empty() && trimmed != "null" {
                    if let Ok(n) = trimmed.parse::<i64>() {
                        if n != 0 {
                            return Some(n as usize as *mut c_void);
                        }
                    } else {
                        let s = trimmed.trim_matches('"');
                        let s_trim = s.trim_start();
                        if s_trim.starts_with("0x") || s_trim.starts_with("0X") {
                            if let Ok(u) = usize::from_str_radix(&s_trim[2..], 16) {
                                return Some(u as *mut c_void);
                            }
                        } else if let Ok(u) = s_trim.parse::<usize>() {
                            if u != 0 {
                                return Some(u as *mut c_void);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

// 

/// Box a JS value as a concrete WinRT `IPropertyValue` for the given WinRT type name.
/// See `crate::value::box_as_typed_value` for the full contract; semantics are identical.
pub fn box_as_typed_value(env: &Env, arg: &JsUnknown, type_name: &str) -> Option<NativeValue> {
    let vt = arg.get_type().ok()?;

    // Already-boxed value → pass its handle straight through (avoids double-boxing).
    if vt == ValueType::Object || vt == ValueType::Function {
        let obj: JsObject = unsafe { arg.cast() };
        if let Some(ptr) = try_get_external_handle(env, &obj) {
            if !ptr.is_null() {
                return Some(NativeValue { pointer: ptr });
            }
        }
    }

    macro_rules! box_insp {
        ($expr:expr) => {{
            let v = $expr.ok()?;
            let ptr = v.as_raw() as *mut c_void;
            // WinRT callees AddRef when storing; leak our reference so the raw pointer stays
            // valid through the FFI call (same contract as the rusty_v8 original).
            std::mem::forget(v);
            Some(NativeValue { pointer: ptr })
        }};
    }
    macro_rules! box_num {
        ($create:expr) => {{
            let n = coerce_f64(env, arg)?;
            box_insp!($create(n))
        }};
    }

    // Accept both short names ("TimeSpan") and fully-qualified ("Windows.Foundation.TimeSpan").
    let type_name = type_name.trim();
    let type_name = type_name
        .strip_prefix("Windows.Foundation.")
        .unwrap_or(type_name);
    match type_name {
        "Double" => box_num!(|n: f64| PropertyValue::CreateDouble(n)),
        "Single" => box_num!(|n: f64| PropertyValue::CreateSingle(n as f32)),
        "Int32" | "IntI32" => box_num!(|n: f64| PropertyValue::CreateInt32(n as i32)),
        "UInt32" => box_num!(|n: f64| PropertyValue::CreateUInt32(n as u32)),
        // Int64/UInt64 may exceed 2^53 — read via the BigInt-aware helper, not ToNumber.
        "Int64" => box_insp!(PropertyValue::CreateInt64(js_to_i64(env, arg))),
        "UInt64" => box_insp!(PropertyValue::CreateUInt64(js_to_i64(env, arg) as u64)),
        "Int16" => box_num!(|n: f64| PropertyValue::CreateInt16(n as i16)),
        "UInt16" => box_num!(|n: f64| PropertyValue::CreateUInt16(n as u16)),
        "UInt8" | "Uint8" | "Byte" => box_num!(|n: f64| PropertyValue::CreateUInt8(n as u8)),
        // Char16: accept a JS string (first UTF-16 code unit) or a number.
        "Char16" | "Char" => {
            let ch: u16 = if vt == ValueType::String {
                let s: JsString = unsafe { arg.cast() };
                string_utf16(s).ok()?.first().copied().unwrap_or(0)
            } else {
                coerce_f64(env, arg)? as u16
            };
            box_insp!(PropertyValue::CreateChar16(ch))
        }
        "Boolean" => {
            let b = coerce_bool(env, arg);
            box_insp!(PropertyValue::CreateBoolean(b))
        }
        "String" => {
            let hs = hstring_from_js_value(env, arg);
            box_insp!(PropertyValue::CreateString(&hs))
        }
        // TimeSpan: milliseconds number, or struct { Duration: <100ns ticks> } (Number/BigInt).
        "TimeSpan" => {
            let ticks = if vt == ValueType::Object {
                read_i64_field(env, arg, "Duration")
            } else {
                (coerce_f64(env, arg)? * 10_000.0) as i64
            };
            let ts = windows::Foundation::TimeSpan { Duration: ticks };
            box_insp!(PropertyValue::CreateTimeSpan(ts))
        }
        // DateTime: JS ms since Unix epoch, or struct { UniversalTime: <ticks since 1601> }.
        "DateTime" => {
            let universal_time = if vt == ValueType::Object {
                read_i64_field(env, arg, "UniversalTime")
            } else {
                const EPOCH_DIFF_TICKS: i64 = 11_644_473_600_000 * 10_000;
                let ms = coerce_f64(env, arg)? as i64;
                ms * 10_000 + EPOCH_DIFF_TICKS
            };
            let dt = windows::Foundation::DateTime {
                UniversalTime: universal_time,
            };
            box_insp!(PropertyValue::CreateDateTime(dt))
        }
        "Guid" => {
            let s = js_to_rust_string(env, arg);
            let guid = parse_guid_str(s.trim())?;
            box_insp!(PropertyValue::CreateGuid(guid))
        }
        _ => None,
    }
}

/// Alias used by method_call / property_call for IReference<T> params.
#[inline]
pub fn box_as_ireference(env: &Env, arg: &JsUnknown, inner_type: &str) -> Option<NativeValue> {
    box_as_typed_value(env, arg, inner_type)
}

// 

pub fn napi_parse_pointer(env: &Env, arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let vt = value_type(arg)?;

    if vt == ValueType::Object || vt == ValueType::Function {
        let obj: JsObject = unsafe { arg.cast() };
        if let Some(pointer) = try_get_external_handle(env, &obj) {
            return Ok(NativeValue { pointer });
        }
    }

    // Box primitive JS values as WinRT IPropertyValue (IInspectable) so they can be passed to
    // Object-typed parameters like Header, Content, or IVector<Object>.Append.
    if vt == ValueType::String {
        let s: JsString = unsafe { arg.cast() };
        let hstring = HSTRING::from_wide(&string_utf16(s)?);
        if let Ok(inspectable) = PropertyValue::CreateString(&hstring) {
            let ptr = inspectable.as_raw() as *mut c_void;
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    if vt == ValueType::Number {
        let n: JsNumber = unsafe { arg.cast() };
        let n = n.get_double().unwrap_or(0.0);
        // Untyped Object parameters: Int32 for whole numbers, Double otherwise. Callers that
        // need a specific IReference<T> call box_as_ireference() first.
        if n == n.trunc() && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
            if let Ok(inspectable) = PropertyValue::CreateInt32(n as i32) {
                let ptr = inspectable.as_raw() as *mut c_void;
                std::mem::forget(inspectable);
                return Ok(NativeValue { pointer: ptr });
            }
        } else if let Ok(inspectable) = PropertyValue::CreateDouble(n) {
            let ptr = inspectable.as_raw() as *mut c_void;
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    if vt == ValueType::Boolean {
        let b: JsBoolean = unsafe { arg.cast() };
        if let Ok(inspectable) = PropertyValue::CreateBoolean(b.get_value().unwrap_or(false)) {
            let ptr = inspectable.as_raw() as *mut c_void;
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    let pointer = if vt == ValueType::External {
        ptr_from_external(env, arg).ok_or_else(|| {
            type_error("Invalid FFI pointer type, expected null, External, or { handle: External|null }")
        })?
    } else if vt == ValueType::Null || vt == ValueType::Undefined {
        std::ptr::null_mut()
    } else {
        return Err(type_error(
            "Invalid FFI pointer type, expected null, External, or { handle: External|null }",
        ));
    };
    Ok(NativeValue { pointer })
}

pub fn napi_parse_function(env: &Env, arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let vt = value_type(arg)?;
    if vt == ValueType::Object || vt == ValueType::Function {
        let obj: JsObject = unsafe { arg.cast() };
        if let Some(pointer) = try_get_external_handle(env, &obj) {
            return Ok(NativeValue { pointer });
        }
    }

    let pointer = if vt == ValueType::External {
        ptr_from_external(env, arg).ok_or_else(|| {
            type_error("Invalid FFI function type, expected null, External, or { handle: External|null }")
        })?
    } else if vt == ValueType::Null || vt == ValueType::Undefined {
        std::ptr::null_mut()
    } else {
        return Err(type_error(
            "Invalid FFI function type, expected null, External, or { handle: External|null }",
        ));
    };
    Ok(NativeValue { pointer })
}

/// Extract the object's COM pointer and QI it to `iid`. Returns the queried pointer plus its
/// owning IUnknown (kept alive by the caller).
pub fn napi_parse_query_interface(
    env: &Env,
    arg: &JsUnknown,
    iid: &GUID,
) -> Result<(NativeValue, Option<IUnknown>), AnyError> {
    let vt = value_type(arg)?;
    if vt == ValueType::Null || vt == ValueType::Undefined {
        return Ok((
            NativeValue {
                pointer: std::ptr::null_mut(),
            },
            None,
        ));
    }

    if vt == ValueType::Object || vt == ValueType::Function {
        let obj: JsObject = unsafe { arg.cast() };
        if let Some(pointer) = try_get_external_handle(env, &obj) {
            if pointer.is_null() {
                return Ok((
                    NativeValue {
                        pointer: std::ptr::null_mut(),
                    },
                    None,
                ));
            }

            let unknown = ManuallyDrop::new(unsafe { IUnknown::from_raw(pointer) });
            let vtable = unknown.vtable();
            let mut queried: *mut c_void = std::ptr::null_mut();

            let result = unsafe {
                ((*vtable).QueryInterface)(
                    unknown.as_raw(),
                    iid,
                    &mut queried as *mut _ as *mut *mut c_void,
                )
            };

            if result.is_ok() && !queried.is_null() {
                let queried = unsafe { IUnknown::from_raw(queried) };
                let pointer = queried.as_raw() as *mut c_void;
                return Ok((NativeValue { pointer }, Some(queried)));
            }

            return Err(type_error(
                "Invalid FFI interface argument for expected WinRT type",
            ));
        }
    }

    Ok((napi_parse_pointer(env, arg)?, None))
}

// 

/// Data pointer + byte length of an ArrayBuffer / TypedArray / DataView, or None when the
/// value is none of those. Uses raw `node_api.h` calls; for views, napi already returns the
/// data pointer adjusted by byte_offset.
pub(crate) fn buffer_data(env: &Env, arg: &JsUnknown) -> Option<(*mut c_void, usize)> {
    let raw_env = env.raw();
    let raw = unsafe { arg.raw() };
    unsafe {
        let mut is = false;
        if sys::napi_is_arraybuffer(raw_env, raw, &mut is) == sys::Status::napi_ok && is {
            let mut data: *mut c_void = std::ptr::null_mut();
            let mut len = 0usize;
            if sys::napi_get_arraybuffer_info(raw_env, raw, &mut data, &mut len)
                == sys::Status::napi_ok
            {
                return Some((data, len));
            }
            return None;
        }

        let mut is = false;
        if sys::napi_is_typedarray(raw_env, raw, &mut is) == sys::Status::napi_ok && is {
            let mut ty: sys::napi_typedarray_type = 0;
            let mut length = 0usize;
            let mut data: *mut c_void = std::ptr::null_mut();
            let mut ab: sys::napi_value = std::ptr::null_mut();
            let mut offset = 0usize;
            if sys::napi_get_typedarray_info(
                raw_env, raw, &mut ty, &mut length, &mut data, &mut ab, &mut offset,
            ) == sys::Status::napi_ok
            {
                // napi reports length in elements; v8's byte_length is in bytes.
                let elem = match ty {
                    0..=2 => 1usize,       // int8, uint8, uint8_clamped
                    3 | 4 => 2,            // int16, uint16
                    5..=7 => 4,            // int32, uint32, float32
                    _ => 8,                // float64, bigint64, biguint64
                };
                return Some((data, length * elem));
            }
            return None;
        }

        let mut is = false;
        if sys::napi_is_dataview(raw_env, raw, &mut is) == sys::Status::napi_ok && is {
            let mut byte_len = 0usize;
            let mut data: *mut c_void = std::ptr::null_mut();
            let mut ab: sys::napi_value = std::ptr::null_mut();
            let mut offset = 0usize;
            if sys::napi_get_dataview_info(
                raw_env, raw, &mut byte_len, &mut data, &mut ab, &mut offset,
            ) == sys::Status::napi_ok
            {
                return Some((data, byte_len));
            }
            return None;
        }
    }
    None
}

pub fn napi_parse_buffer(env: &Env, arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    let (value, _) = napi_parse_buffer_with_length(env, arg)?;
    Ok(value)
}

pub fn napi_parse_buffer_with_length(
    env: &Env,
    arg: &JsUnknown,
) -> Result<(NativeValue, u32), AnyError> {
    if let Some((pointer, byte_len)) = buffer_data(env, arg) {
        return Ok((NativeValue { pointer }, byte_len as u32));
    }
    if value_type(arg)? == ValueType::Null {
        return Ok((
            NativeValue {
                pointer: std::ptr::null_mut(),
            },
            0,
        ));
    }
    Err(type_error(
        "Invalid FFI buffer type, expected null, ArrayBuffer, or ArrayBufferView",
    ))
}

pub fn napi_parse_struct(env: &Env, arg: &JsUnknown) -> Result<NativeValue, AnyError> {
    if let Some((pointer, _)) = buffer_data(env, arg) {
        if pointer.is_null() {
            return Err(type_error(
                "Invalid FFI ArrayBuffer, expected data in buffer",
            ));
        }
        return Ok(NativeValue { pointer });
    }
    Err(type_error(
        "Invalid FFI struct type, expected ArrayBuffer, or ArrayBufferView",
    ))
}

/// Write a single struct field value into a byte buffer in little-endian order (plain JS object
/// like `{A:255, R:0}` → WinRT struct bytes).
pub fn append_struct_field_bytes(
    env: &Env,
    buf: &mut Vec<u8>,
    value: &JsUnknown,
    native_type: &NativeType,
) {
    let num = coerce_f64(env, value).unwrap_or(0.0);
    match native_type {
        NativeType::F64 => buf.extend_from_slice(&num.to_le_bytes()),
        NativeType::F32 => buf.extend_from_slice(&(num as f32).to_le_bytes()),
        NativeType::I32 => buf.extend_from_slice(&(num as i32).to_le_bytes()),
        NativeType::U32 => buf.extend_from_slice(&(num as u32).to_le_bytes()),
        // i64/u64 fields can exceed 2^53 — read via the BigInt-aware helper, not f64.
        NativeType::I64 => buf.extend_from_slice(&js_to_i64(env, value).to_le_bytes()),
        NativeType::U64 => buf.extend_from_slice(&(js_to_i64(env, value) as u64).to_le_bytes()),
        NativeType::I16 => buf.extend_from_slice(&(num as i16).to_le_bytes()),
        NativeType::U16 => buf.extend_from_slice(&(num as u16).to_le_bytes()),
        NativeType::I8 => buf.extend_from_slice(&(num as i8).to_le_bytes()),
        NativeType::U8 => buf.push(num as u8),
        NativeType::Bool => buf.push(if coerce_bool(env, value) { 1u8 } else { 0u8 }),
        _ => buf.extend(std::iter::repeat(0u8).take(native_type.size())),
    }
}

// 

/// Read a native value from a raw pointer and convert it to a JS value. `ptr` must point to
/// storage holding the native representation for `native_type`.
pub unsafe fn read_value_from_ptr(
    env: &Env,
    ptr: *const c_void,
    native_type: &NativeType,
) -> Result<JsUnknown, AnyError> {
    use std::ptr::read_unaligned;
    let map = |e: napi::Error| type_error(e.to_string());
    Ok(match native_type {
        NativeType::Void => as_unknown(env, env.get_undefined().map_err(map)?),
        NativeType::Bool => {
            let b = read_unaligned(ptr as *const u8) != 0u8;
            as_unknown(env, env.get_boolean(b).map_err(map)?)
        }
        NativeType::U8 => {
            let v = read_unaligned(ptr as *const u8);
            as_unknown(env, env.create_uint32(v as u32).map_err(map)?)
        }
        NativeType::I8 => {
            let v = read_unaligned(ptr as *const i8);
            as_unknown(env, env.create_int32(v as i32).map_err(map)?)
        }
        NativeType::U16 => {
            let v = read_unaligned(ptr as *const u16);
            as_unknown(env, env.create_uint32(v as u32).map_err(map)?)
        }
        NativeType::I16 => {
            let v = read_unaligned(ptr as *const i16);
            as_unknown(env, env.create_int32(v as i32).map_err(map)?)
        }
        NativeType::U32 => {
            let v = read_unaligned(ptr as *const u32);
            as_unknown(env, env.create_uint32(v).map_err(map)?)
        }
        NativeType::I32 => {
            let v = read_unaligned(ptr as *const i32);
            as_unknown(env, env.create_int32(v).map_err(map)?)
        }
        NativeType::U64 => {
            let ret = read_unaligned(ptr as *const u64);
            if ret > MAX_SAFE_INTEGER as u64 {
                as_unknown(env, env.create_bigint_from_u64(ret).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(ret as f64).map_err(map)?)
            }
        }
        NativeType::I64 => {
            let ret = read_unaligned(ptr as *const i64);
            if ret > MAX_SAFE_INTEGER as i64 || ret < MIN_SAFE_INTEGER as i64 {
                as_unknown(env, env.create_bigint_from_i64(ret).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(ret as f64).map_err(map)?)
            }
        }
        NativeType::USize => {
            let ret = read_unaligned(ptr as *const usize);
            if ret > MAX_SAFE_INTEGER as usize {
                as_unknown(env, env.create_bigint_from_u64(ret as u64).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(ret as f64).map_err(map)?)
            }
        }
        NativeType::ISize => {
            let ret = read_unaligned(ptr as *const isize);
            if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&ret) {
                as_unknown(env, env.create_bigint_from_i64(ret as i64).map_err(map)?)
            } else {
                as_unknown(env, env.create_double(ret as f64).map_err(map)?)
            }
        }
        NativeType::F32 => {
            let bits = read_unaligned(ptr as *const u32);
            as_unknown(
                env,
                env.create_double(f32::from_bits(bits) as f64).map_err(map)?,
            )
        }
        NativeType::F64 => {
            let bits = read_unaligned(ptr as *const u64);
            as_unknown(env, env.create_double(f64::from_bits(bits)).map_err(map)?)
        }
        NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
            let p = read_unaligned(ptr as *const *mut c_void);
            external_or_null(env, p)?
        }
        NativeType::Struct(_) => {
            // Expose as an external pointing to the struct bytes.
            if ptr.is_null() {
                as_unknown(env, env.get_null().map_err(map)?)
            } else {
                external_from_ptr(env, ptr as *mut c_void)?
            }
        }
        NativeType::String => {
            if ptr.is_null() {
                as_unknown(env, env.get_undefined().map_err(map)?)
            } else {
                // Take ownership of the callee-written HSTRING handle, then drop it to
                // release the WinRT string buffer (same ownership contract as the original).
                let raw_usize = read_unaligned(ptr as *const usize);
                let hstring: HSTRING = std::mem::transmute(raw_usize);
                // HSTRING and JS strings are both UTF-16: hand the code units straight to the
                // engine instead of transcoding through a Rust String.
                let js = env.create_string_utf16(&hstring).map_err(map)?;
                drop(hstring);
                as_unknown(env, js)
            }
        }
    })
}

/// Converts a WinRT call result to the JS return value, using the same slot contract as the
/// classic runtime: for numerics/bool/string `value` points AT the return slot; for
/// Pointer/Buffer/Function/Struct `value` IS the returned pointer.
///
/// The Pointer branch does not resolve a typed proxy (`ns_proxy::try_wrap_inspectable_pointer`)
/// the way delegate invocation does; reference-type returns currently surface as plain externals.
pub unsafe fn read_return_value(
    env: &Env,
    value: *mut c_void,
    native_type: &NativeType,
) -> Result<JsUnknown, AnyError> {
    match native_type {
        NativeType::Pointer | NativeType::Buffer | NativeType::Function | NativeType::Struct(_) => {
            external_or_null(env, value)
        }
        _ => read_value_from_ptr(env, value, native_type),
    }
}

// 

/// Initialize caller-allocated out-slot storage from a JS value; returns the parse type the
/// slot should be treated as for later string-cloning logic (same contract as the original).
pub fn write_js_value_to_ptr(
    env: &Env,
    arg: &JsUnknown,
    dst: *mut c_void,
    native_type: &NativeType,
) -> Result<Option<NativeType>, AnyError> {
    use std::ptr::{copy_nonoverlapping, write_unaligned};
    match native_type {
        NativeType::Bool => {
            let nv = napi_parse_bool(arg)?;
            unsafe { write_unaligned(dst as *mut u8, nv.bool_value as u8) };
            Ok(Some(NativeType::Bool))
        }
        NativeType::U8 => {
            let nv = napi_parse_u8(arg)?;
            unsafe { write_unaligned(dst as *mut u8, nv.u8_value) };
            Ok(Some(NativeType::U8))
        }
        NativeType::I8 => {
            let nv = napi_parse_i8(arg)?;
            unsafe { write_unaligned(dst as *mut i8, nv.i8_value) };
            Ok(Some(NativeType::I8))
        }
        NativeType::U16 => {
            let nv = napi_parse_u16(arg)?;
            unsafe { write_unaligned(dst as *mut u16, nv.u16_value) };
            Ok(Some(NativeType::U16))
        }
        NativeType::I16 => {
            let nv = napi_parse_i16(arg)?;
            unsafe { write_unaligned(dst as *mut i16, nv.i16_value) };
            Ok(Some(NativeType::I16))
        }
        NativeType::U32 => {
            let nv = napi_parse_u32(arg)?;
            unsafe { write_unaligned(dst as *mut u32, nv.u32_value) };
            Ok(Some(NativeType::U32))
        }
        NativeType::I32 => {
            let nv = napi_parse_i32(arg)?;
            unsafe { write_unaligned(dst as *mut i32, nv.i32_value) };
            Ok(Some(NativeType::I32))
        }
        NativeType::U64 => {
            let nv = napi_parse_u64(arg)?;
            unsafe { write_unaligned(dst as *mut u64, nv.u64_value) };
            Ok(Some(NativeType::U64))
        }
        NativeType::I64 => {
            let nv = napi_parse_i64(arg)?;
            unsafe { write_unaligned(dst as *mut i64, nv.i64_value) };
            Ok(Some(NativeType::I64))
        }
        NativeType::USize => {
            let nv = napi_parse_usize(arg)?;
            unsafe { write_unaligned(dst as *mut usize, nv.usize_value) };
            Ok(Some(NativeType::USize))
        }
        NativeType::ISize => {
            let nv = napi_parse_isize(arg)?;
            unsafe { write_unaligned(dst as *mut isize, nv.isize_value) };
            Ok(Some(NativeType::ISize))
        }
        NativeType::F32 => {
            let nv = napi_parse_f32(arg)?;
            unsafe { write_unaligned(dst as *mut f32, nv.f32_value) };
            Ok(Some(NativeType::F32))
        }
        NativeType::F64 => {
            let nv = napi_parse_f64(arg)?;
            unsafe { write_unaligned(dst as *mut f64, nv.f64_value) };
            Ok(Some(NativeType::F64))
        }
        NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
            let nv = napi_parse_pointer(env, arg)?;
            unsafe { write_unaligned(dst as *mut *mut c_void, nv.pointer) };
            Ok(Some(NativeType::Pointer))
        }
        NativeType::Struct(_) => {
            // Copy struct bytes from an ArrayBuffer/ArrayBufferView source.
            let nv = napi_parse_struct(env, arg)?;
            let src = unsafe { nv.pointer as *const u8 };
            let size = native_type.size();
            unsafe { copy_nonoverlapping(src, dst as *mut u8, size) };
            Ok(Some(native_type.clone()))
        }
        NativeType::String => {
            // JS string -> HSTRING, storing the raw handle (ownership transferred to the slot).
            // The rusty_v8 __hstring_ptr fast path is dead code (its producer is unused), so
            // this implementation intentionally omits it.
            let nv = napi_parse_string(arg)?;
            let h = ManuallyDrop::into_inner(unsafe { nv.string });
            let raw_usize: usize = unsafe { std::mem::transmute(h) };
            unsafe { write_unaligned(dst as *mut usize, raw_usize) };
            Ok(Some(NativeType::String))
        }
        NativeType::Void => Ok(None),
    }
}

// 

/// Detects an `NSWinRT.interop.out(...)` wrapper and returns its object plus the wrapped
/// `value` that should initialize the native byref slot.
pub fn try_unwrap_out_param(env: &Env, arg: &JsUnknown) -> Option<(JsObject, JsUnknown)> {
    let vt = arg.get_type().ok()?;
    if vt != ValueType::Object && vt != ValueType::Function {
        return None;
    }
    let obj: JsObject = unsafe { arg.cast() };
    let marker = obj.get_named_property::<JsUnknown>(OUT_PARAM_MARKER).ok()?;
    if !coerce_bool(env, &marker) {
        return None;
    }
    let value = obj.get_named_property::<JsUnknown>("value").ok()?;
    Some((obj, value))
}

pub fn set_out_param_value(wrapper: &mut JsObject, value: JsUnknown) -> bool {
    wrapper.set_named_property("value", value).is_ok()
}

/// Unbox a WinRT boxed primitive (`IPropertyValue` / `IReference<T>`, e.g. what
/// `PropertySet.Lookup` returns for a value inserted as a JS number/string/bool) into the
/// corresponding JS primitive. `raw` is borrowed. Returns `None` for anything that is not a
/// boxed primitive so callers can fall through to proxy/external wrapping.
pub fn try_unbox_property_value(env: &Env, raw: *mut c_void) -> Option<JsUnknown> {
    use windows::Foundation::{IPropertyValue, PropertyType};
    if raw.is_null() {
        return None;
    }
    let borrowed = ManuallyDrop::new(unsafe { IUnknown::from_raw(raw) });
    let pv: IPropertyValue = (*borrowed).cast().ok()?;
    let ty = unsafe { pv.Type() }.ok()?;
    let num = |v: f64| env.create_double(v).ok().map(|n| as_unknown(env, n));
    match ty {
        PropertyType::UInt8 => num(unsafe { pv.GetUInt8() }.ok()? as f64),
        PropertyType::Int16 => num(unsafe { pv.GetInt16() }.ok()? as f64),
        PropertyType::UInt16 => num(unsafe { pv.GetUInt16() }.ok()? as f64),
        PropertyType::Int32 => num(unsafe { pv.GetInt32() }.ok()? as f64),
        PropertyType::UInt32 => num(unsafe { pv.GetUInt32() }.ok()? as f64),
        PropertyType::Int64 => num(unsafe { pv.GetInt64() }.ok()? as f64),
        PropertyType::UInt64 => num(unsafe { pv.GetUInt64() }.ok()? as f64),
        PropertyType::Single => num(unsafe { pv.GetSingle() }.ok()? as f64),
        PropertyType::Double => num(unsafe { pv.GetDouble() }.ok()?),
        PropertyType::Boolean => {
            let b = unsafe { pv.GetBoolean() }.ok()?;
            env.get_boolean(b).ok().map(|v| as_unknown(env, v))
        }
        PropertyType::String => {
            let s = unsafe { pv.GetString() }.ok()?;
            env.create_string_utf16(&s).ok().map(|v| as_unknown(env, v))
        }
        PropertyType::Guid => {
            let g = unsafe { pv.GetGuid() }.ok()?;
            env.create_string(&format!("{g:?}")).ok().map(|v| as_unknown(env, v))
        }
        _ => None,
    }
}
