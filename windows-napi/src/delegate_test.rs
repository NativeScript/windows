//! Verification hooks for the ported NapiDelegate COM bridge: create a delegate over a JS
//! function, fire it through the real COM vtable (exactly what a WinRT event source does),
//! and exercise refcounting. Test-only surface.

use napi::{Env, JsFunction};
use napi_derive::napi;
use windows_core::GUID;

use runtime::napi_engine::delegate::{
    invoke_delegate_raw, make_napi_void_delegate, release_delegate_raw,
};
use runtime::napi_engine::NativeType;

fn param_type(ty: &str) -> Option<NativeType> {
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
        "pointer" => NativeType::Pointer,
        _ => return None,
    })
}

/// Wrap `func` in a NapiDelegate COM object with the given Invoke parameter types.
/// Returns the COM pointer as f64 (refcount 1).
#[napi]
pub fn make_delegate(env: Env, func: JsFunction, param_types: Vec<String>) -> napi::Result<f64> {
    let types = param_types
        .iter()
        .map(|t| param_type(t))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| napi::Error::from_reason("unknown param type"))?;
    let guid = GUID::from_u128(0x11223344_5566_7788_99aa_bbccddeeff00);
    match make_napi_void_delegate(&env, &func, guid, types) {
        Some(ptr) => Ok(ptr as usize as f64),
        None => Err(napi::Error::from_reason("make_napi_delegate failed")),
    }
}

/// Fire the delegate through its COM vtable with up to three raw (usize-packed) args.
/// Returns the HRESULT.
#[napi]
pub fn invoke_delegate(ptr: f64, p0: f64, p1: f64, p2: f64) -> i32 {
    unsafe {
        invoke_delegate_raw(
            ptr as usize as *mut _,
            p0 as i64 as usize,
            p1 as i64 as usize,
            p2 as i64 as usize,
        )
    }
}

/// Release one COM reference; returns the remaining count.
#[napi]
pub fn release_delegate(ptr: f64) -> u32 {
    unsafe { release_delegate_raw(ptr as usize as *mut _) }
}
