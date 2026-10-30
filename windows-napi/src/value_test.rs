//! Verification hooks for the ported `napi_engine::value` marshaling.
//!
//! Each hook exercises one ported unit end-to-end so `value-test.js` / `value-test2.js` can
//! assert the exact semantics (including V8's coercion/truncation quirks) match the rusty_v8
//! originals. Test-only surface.

use napi::{Env, JsObject, JsUnknown};
use napi_derive::napi;

use runtime::napi_engine::value::{
    append_struct_field_bytes, box_as_typed_value, external_from_ptr, napi_parse_arg,
    napi_parse_buffer_with_length, napi_parse_pointer, napi_parse_struct, native_value_to_napi,
    read_value_from_ptr, set_out_param_value, try_unwrap_out_param, write_js_value_to_ptr,
};
use runtime::napi_engine::NativeType;

fn native_type(ty: &str) -> Option<NativeType> {
    Some(match ty {
        "bool" => NativeType::Bool,
        "u8" => NativeType::U8,
        "i8" => NativeType::I8,
        "u16" => NativeType::U16,
        "i16" => NativeType::I16,
        "u32" => NativeType::U32,
        "i32" => NativeType::I32,
        "u64" => NativeType::U64,
        "i64" => NativeType::I64,
        "usize" => NativeType::USize,
        "isize" => NativeType::ISize,
        "f32" => NativeType::F32,
        "f64" => NativeType::F64,
        "string" => NativeType::String,
        "pointer" => NativeType::Pointer,
        "function" => NativeType::Function,
        "buffer" => NativeType::Buffer,
        _ => return None,
    })
}

fn reason(e: impl ToString) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Parse `value` as `ty` into a `NativeValue`, then marshal it back to JS. Throws on parse
/// errors so the JS test can assert rejection.
#[napi]
pub fn ffi_roundtrip(env: Env, value: JsUnknown, ty: String) -> napi::Result<JsUnknown> {
    let nt = native_type(&ty).ok_or_else(|| reason(format!("unknown type {ty}")))?;
    let nv = napi_parse_arg(&env, &value, &nt).map_err(reason)?;
    // SAFETY: `nt` selects the union field `napi_parse_arg` just initialized.
    unsafe { native_value_to_napi(&env, &nv, &nt) }.map_err(reason)
}

/// Parse `value` as a Pointer arg and return the raw pointer as f64 (user-space addresses
/// fit in 2^53).
#[napi]
pub fn pointer_value(env: Env, value: JsUnknown) -> napi::Result<f64> {
    let nv = napi_parse_pointer(&env, &value).map_err(reason)?;
    Ok(unsafe { nv.pointer } as usize as f64)
}

/// Create a native-pointer external carrying `addr` (for pointer round-trip tests).
#[napi]
pub fn make_external(env: Env, addr: f64) -> napi::Result<JsUnknown> {
    external_from_ptr(&env, addr as usize as *mut std::ffi::c_void).map_err(reason)
}

/// Parse `value` as a Buffer arg; returns `[dataPtr, byteLength]` as f64s.
#[napi]
pub fn buffer_info(env: Env, value: JsUnknown) -> napi::Result<Vec<f64>> {
    let (nv, len) = napi_parse_buffer_with_length(&env, &value).map_err(reason)?;
    Ok(vec![unsafe { nv.pointer } as usize as f64, len as f64])
}

/// Parse `value` as a Struct arg of `size` bytes (default 0); returns the data pointer as f64.
#[napi]
pub fn struct_ptr(env: Env, value: JsUnknown, size: Option<u32>) -> napi::Result<f64> {
    let nv = napi_parse_struct(&env, &value, size.unwrap_or(0) as usize).map_err(reason)?;
    Ok(unsafe { nv.pointer } as usize as f64)
}

/// Write `value` into a native scratch slot as `ty`, then read it back to JS — exercises
/// `write_js_value_to_ptr` + `read_value_from_ptr` (including HSTRING ownership hand-off).
#[napi]
pub fn write_read_ptr(env: Env, value: JsUnknown, ty: String) -> napi::Result<JsUnknown> {
    let nt = native_type(&ty).ok_or_else(|| reason(format!("unknown type {ty}")))?;
    let mut slot = [0u8; 32];
    write_js_value_to_ptr(&env, &value, slot.as_mut_ptr() as *mut _, &nt).map_err(reason)?;
    // SAFETY: the slot was just initialized for `nt`; String reads consume the HSTRING we
    // wrote, balancing its ownership.
    unsafe { read_value_from_ptr(&env, slot.as_ptr() as *const _, &nt) }.map_err(reason)
}

/// If `value` is an `interop.out(...)` wrapper, return its wrapped value; else return the
/// string `"<none>"`.
#[napi]
pub fn out_param_value(env: Env, value: JsUnknown) -> napi::Result<JsUnknown> {
    match try_unwrap_out_param(&env, &value) {
        Some((_, inner)) => Ok(inner),
        None => Ok(env.create_string("<none>")?.into_unknown()),
    }
}

/// Set the `value` slot of an out-param wrapper; returns whether the set succeeded.
#[napi]
pub fn set_out_param(mut wrapper: JsObject, new_value: JsUnknown) -> napi::Result<bool> {
    Ok(set_out_param_value(&mut wrapper, new_value))
}

/// Serialize `value` as a struct field of type `ty`; returns the little-endian bytes.
#[napi]
pub fn struct_field_bytes(env: Env, value: JsUnknown, ty: String) -> napi::Result<Vec<u32>> {
    let nt = native_type(&ty).ok_or_else(|| reason(format!("unknown type {ty}")))?;
    let mut buf = Vec::new();
    append_struct_field_bytes(&env, &mut buf, &value, &nt);
    Ok(buf.into_iter().map(|b| b as u32).collect())
}

/// Box `value` as a WinRT `IPropertyValue` of `ty`; returns the raw IInspectable pointer as
/// f64 (0 when boxing is not possible). Requires WinRT initialized (call init() first).
#[napi]
pub fn box_typed(env: Env, value: JsUnknown, ty: String) -> napi::Result<f64> {
    Ok(match box_as_typed_value(&env, &value, &ty) {
        Some(nv) => (unsafe { nv.pointer }) as usize as f64,
        None => 0.0,
    })
}
