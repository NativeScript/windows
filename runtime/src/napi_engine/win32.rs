//! `NSWinRT.win32` for the napi engines.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::thread::ThreadId;

use libffi::low::{self, ffi_cif, ffi_closure, CodePtr};
use libffi::middle::{Cif, Type};
use napi::{
    sys, CallContext, Env, JsBoolean, JsFunction, JsNumber, JsObject, JsUnknown, NapiRaw,
    NapiValue, ValueType,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, RegisterClassExW, WM_NCDESTROY,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::napi_engine::value::{
    as_unknown, buffer_data, external_ptr_raw, js_to_rust_string, try_get_external_handle,
};
use crate::win32::{ffi_type_for, list_exports, Win32Result};
use crate::win32_call::{parse_signature, prepare, Kind, Prepared, TextArena, Words, MAX_ARGS};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

fn err(msg: impl Into<String>) -> napi::Error {
    napi::Error::from_reason(msg.into())
}

fn string_arg(ctx: &CallContext, index: usize) -> String {
    ctx.get::<JsUnknown>(index)
        .map(|v| js_to_rust_string(&ctx.env, &v))
        .unwrap_or_default()
}

fn bigint_word(env: &Env, value: &JsUnknown) -> u64 {
    let (mut signed, mut lossless) = (0i64, false);
    unsafe {
        if sys::napi_get_value_bigint_int64(env.raw(), value.raw(), &mut signed, &mut lossless)
            == sys::Status::napi_ok
            && lossless
        {
            return signed as u64;
        }
        let mut unsigned = 0u64;
        sys::napi_get_value_bigint_uint64(env.raw(), value.raw(), &mut unsigned, &mut lossless);
        unsigned
    }
}

/// Address of a buffer, pointer external or handle object.
fn pointer_of(env: &Env, value: &JsUnknown) -> napi::Result<Option<u64>> {
    if let Some((data, _)) = buffer_data(env, value) {
        return Ok(Some(data as u64));
    }
    match value.get_type()? {
        ValueType::External => Ok(external_ptr_raw(env, unsafe { value.raw() }).map(|p| p as u64)),
        ValueType::Object => {
            let obj: JsObject = unsafe { value.cast() };
            Ok(try_get_external_handle(env, &obj).map(|p| p as u64))
        }
        _ => Ok(None),
    }
}

fn text_arg(env: &Env, kind: Kind, value: &JsUnknown, arena: &mut TextArena) -> napi::Result<u64> {
    let (env, raw) = (env.raw(), unsafe { value.raw() });
    let mut len = 0usize;
    unsafe {
        if kind == Kind::WStr {
            if sys::napi_get_value_string_utf16(env, raw, std::ptr::null_mut(), 0, &mut len)
                != sys::Status::napi_ok
            {
                return Err(err("not a string"));
            }
            let (out, address) = arena.wide(len);
            let mut written = 0usize;
            sys::napi_get_value_string_utf16(env, raw, out.as_mut_ptr(), len + 1, &mut written);
            Ok(address)
        } else {
            if sys::napi_get_value_string_utf8(env, raw, std::ptr::null_mut(), 0, &mut len)
                != sys::Status::napi_ok
            {
                return Err(err("not a string"));
            }
            let (out, address) = arena.narrow(len);
            let mut written = 0usize;
            sys::napi_get_value_string_utf8(
                env,
                raw,
                out.as_mut_ptr().cast(),
                len + 1,
                &mut written,
            );
            Ok(address)
        }
    }
}

/// A null string becomes a null pointer.
fn arg_word(env: &Env, kind: Kind, value: &JsUnknown, arena: &mut TextArena) -> napi::Result<u64> {
    let ty = value.get_type()?;
    Ok(match ty {
        ValueType::Null | ValueType::Undefined => match kind {
            Kind::F64 if ty == ValueType::Undefined => f64::NAN.to_bits(),
            Kind::F32 if ty == ValueType::Undefined => f32::NAN.to_bits() as u64,
            _ => 0,
        },
        ValueType::Number => kind.word_from_f64(unsafe { value.cast::<JsNumber>() }.get_double()?),
        ValueType::Boolean => {
            kind.word_from_i64(unsafe { value.cast::<JsBoolean>() }.get_value()? as i64)
        }
        ValueType::BigInt => kind.word_from_i64(bigint_word(env, value) as i64),
        ValueType::String if kind.is_text() => text_arg(env, kind, value, arena)?,
        ValueType::String => kind.word_from_f64(
            unsafe { JsUnknown::from_raw_unchecked(env.raw(), value.raw()) }
                .coerce_to_number()?
                .get_double()?,
        ),
        _ => match pointer_of(env, value)? {
            Some(address) => kind.word_from_i64(address as i64),
            None if kind.is_text() || kind == Kind::Pointer => {
                return Err(err(format!("cannot pass this value as '{}'", kind.name())))
            }
            None => kind.word_from_f64(
                unsafe { JsUnknown::from_raw_unchecked(env.raw(), value.raw()) }
                    .coerce_to_number()?
                    .get_double()?,
            ),
        },
    })
}

fn number_or_bigint_i64(env: &Env, v: i64) -> napi::Result<JsUnknown> {
    if v.unsigned_abs() <= MAX_SAFE_INTEGER {
        Ok(as_unknown(env, env.create_double(v as f64)?))
    } else {
        env.create_bigint_from_i64(v)?.into_unknown()
    }
}

fn number_or_bigint_u64(env: &Env, v: u64) -> napi::Result<JsUnknown> {
    if v <= MAX_SAFE_INTEGER {
        Ok(as_unknown(env, env.create_double(v as f64)?))
    } else {
        env.create_bigint_from_u64(v)?.into_unknown()
    }
}

fn result_value(env: &Env, result: Win32Result) -> napi::Result<JsUnknown> {
    match result {
        Win32Result::Null => Ok(as_unknown(env, env.get_null()?)),
        Win32Result::Bool(b) => Ok(as_unknown(env, env.get_boolean(b)?)),
        Win32Result::I64(v) => number_or_bigint_i64(env, v),
        Win32Result::U64(v) => number_or_bigint_u64(env, v),
        Win32Result::F64(v) => Ok(as_unknown(env, env.create_double(v)?)),
    }
}

/// Calls `prepared` with the JS arguments at `first`, `first + stride`, ...
fn call_prepared(
    ctx: &CallContext,
    prepared: &Prepared,
    first: usize,
    stride: usize,
) -> napi::Result<JsUnknown> {
    let env: &Env = &ctx.env;
    let mut arena = TextArena::new();
    let mut words = Words::with_capacity(prepared.arity());
    for (i, kind) in prepared.args.iter().enumerate() {
        let value = ctx.get::<JsUnknown>(first + i * stride)?;
        words.push(arg_word(env, *kind, &value, &mut arena).map_err(|e| {
            err(format!(
                "{}() argument {}: {}",
                prepared.name,
                i + 1,
                e.reason
            ))
        })?);
    }
    result_value(env, unsafe { prepared.call(&words) })
}

/// `__nsWin32CallRaw(dll, fn, retType, type0, value0, ...)`
fn call(ctx: CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 3 {
        return Err(err("__nsWin32CallRaw(dll, fn, retType, ...typeValuePairs)"));
    }
    let dll = string_arg(&ctx, 0);
    let name = string_arg(&ctx, 1);
    let ret = string_arg(&ctx, 2);
    let mut types: smallvec::SmallVec<[String; MAX_ARGS]> = smallvec::SmallVec::new();
    let mut i = 3;
    while i + 1 < ctx.length {
        types.push(string_arg(&ctx, i));
        i += 2;
    }
    let (ret, kinds) = parse_signature(&ret, &types).map_err(err)?;
    let prepared = prepare(&dll, &name, ret, &kinds).map_err(err)?;
    call_prepared(&ctx, prepared, 4, 2)
}

/// `__nsWin32Bind(dll, fn, retType, argTypes)`: a function bound to one signature.
fn bind(ctx: CallContext) -> napi::Result<JsFunction> {
    let env: &Env = &ctx.env;
    let dll = string_arg(&ctx, 0);
    let name = string_arg(&ctx, 1);
    let ret = string_arg(&ctx, 2);
    let mut types: Vec<String> = Vec::new();
    if ctx.length > 3 {
        let list: JsObject = ctx.get(3)?;
        for i in 0..list.get_array_length()? {
            types.push(js_to_rust_string(env, &list.get_element::<JsUnknown>(i)?));
        }
    }
    let (ret, kinds) = parse_signature(&ret, &types).map_err(err)?;
    let prepared = prepare(&dll, &name, ret, &kinds).map_err(err)?;
    env.create_function_from_closure(&name, move |ctx| call_prepared(&ctx, prepared, 0, 1))
}

fn exports(ctx: CallContext) -> napi::Result<JsUnknown> {
    let names = list_exports(&string_arg(&ctx, 0)).map_err(err)?;
    let json = serde_json::to_string(&names).map_err(|e| err(e.to_string()))?;
    Ok(as_unknown(&ctx.env, ctx.env.create_string(&json)?))
}

struct Callback {
    env: sys::napi_env,
    func: sys::napi_ref,
    params: Vec<String>,
    ret: String,
    owner: ThreadId,
    // The closure points at it.
    cif: Box<Cif>,
    closure: *mut ffi_closure,
}

thread_local! {
    // Live callbacks, so a second release() is a no-op.
    static CALLBACKS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

/// Callbacks run outside a `CallContext`.
mod raw {
    use super::*;

    pub unsafe fn number(env: sys::napi_env, v: f64) -> sys::napi_value {
        let mut out = std::ptr::null_mut();
        sys::napi_create_double(env, v, &mut out);
        out
    }

    pub unsafe fn word(env: sys::napi_env, v: u64, signed: bool) -> sys::napi_value {
        let mut out = std::ptr::null_mut();
        if signed && (v as i64).unsigned_abs() <= MAX_SAFE_INTEGER {
            sys::napi_create_double(env, v as i64 as f64, &mut out);
        } else if !signed && v <= MAX_SAFE_INTEGER {
            sys::napi_create_double(env, v as f64, &mut out);
        } else if signed {
            sys::napi_create_bigint_int64(env, v as i64, &mut out);
        } else {
            sys::napi_create_bigint_uint64(env, v, &mut out);
        }
        out
    }

    pub unsafe fn boolean(env: sys::napi_env, v: bool) -> sys::napi_value {
        let mut out = std::ptr::null_mut();
        sys::napi_get_boolean(env, v, &mut out);
        out
    }

    pub unsafe fn null(env: sys::napi_env) -> sys::napi_value {
        let mut out = std::ptr::null_mut();
        sys::napi_get_null(env, &mut out);
        out
    }

    pub unsafe fn wide_string(env: sys::napi_env, p: *const u16) -> sys::napi_value {
        if p.is_null() {
            return null(env);
        }
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        let mut out = std::ptr::null_mut();
        sys::napi_create_string_utf16(env, p, len, &mut out);
        out
    }

    pub unsafe fn c_string(env: sys::napi_env, p: *const i8) -> sys::napi_value {
        if p.is_null() {
            return null(env);
        }
        let text = std::ffi::CStr::from_ptr(p).to_string_lossy();
        let mut out = std::ptr::null_mut();
        sys::napi_create_string_utf8(env, text.as_ptr().cast(), text.len(), &mut out);
        out
    }

    pub unsafe fn type_of(env: sys::napi_env, v: sys::napi_value) -> sys::napi_valuetype {
        let mut t = sys::ValueType::napi_undefined;
        sys::napi_typeof(env, v, &mut t);
        t
    }

    /// As (integer, float).
    pub unsafe fn result(env: sys::napi_env, v: sys::napi_value) -> (u64, f64) {
        match type_of(env, v) {
            sys::ValueType::napi_number => {
                let mut f = 0.0;
                sys::napi_get_value_double(env, v, &mut f);
                (f as i64 as u64, f)
            }
            sys::ValueType::napi_boolean => {
                let mut b = false;
                sys::napi_get_value_bool(env, v, &mut b);
                (b as u64, b as u8 as f64)
            }
            sys::ValueType::napi_bigint => {
                let (mut w, mut lossless) = (0i64, false);
                sys::napi_get_value_bigint_int64(env, v, &mut w, &mut lossless);
                (w as u64, w as f64)
            }
            _ => (0, 0.0),
        }
    }

    pub unsafe fn take_exception(env: sys::napi_env) {
        let mut exc = std::ptr::null_mut();
        if sys::napi_get_and_clear_last_exception(env, &mut exc) == sys::Status::napi_ok
            && !exc.is_null()
        {
            if let Some(msg) = crate::napi_engine::delegate::napi_value_to_rust_string(env, exc) {
                crate::store_last_js_error(msg);
            }
        }
    }
}

unsafe fn arg_value(env: sys::napi_env, ty: &str, slot: *const c_void) -> sys::napi_value {
    match ty {
        "i8" => raw::number(env, *(slot as *const i8) as f64),
        "i16" => raw::number(env, *(slot as *const i16) as f64),
        "i32" => raw::number(env, *(slot as *const i32) as f64),
        "i64" => raw::word(env, *(slot as *const i64) as u64, true),
        "u8" => raw::number(env, *(slot as *const u8) as f64),
        "u16" => raw::number(env, *(slot as *const u16) as f64),
        "u32" => raw::number(env, *(slot as *const u32) as f64),
        "u64" => raw::word(env, *(slot as *const u64), false),
        "f32" => raw::number(env, *(slot as *const f32) as f64),
        "f64" => raw::number(env, *(slot as *const f64)),
        "bool" => raw::boolean(env, *(slot as *const i32) != 0),
        "wstr" => raw::wide_string(env, *(slot as *const *const u16)),
        "str" => raw::c_string(env, *(slot as *const *const i8)),
        // "pointer"
        _ => raw::word(env, *(slot as *const usize) as u64, false),
    }
}

/// `None` when it threw.
unsafe fn invoke(cb: &Callback, args: *const *const c_void) -> Option<(u64, f64)> {
    let env = cb.env;
    let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
    if sys::napi_open_handle_scope(env, &mut scope) != sys::Status::napi_ok {
        return None;
    }
    let result = (|| {
        let mut func = std::ptr::null_mut();
        if sys::napi_get_reference_value(env, cb.func, &mut func) != sys::Status::napi_ok
            || func.is_null()
        {
            return None;
        }
        let argv: Vec<sys::napi_value> = cb
            .params
            .iter()
            .enumerate()
            .map(|(i, ty)| arg_value(env, ty, *args.add(i)))
            .collect();
        let mut recv = std::ptr::null_mut();
        sys::napi_get_undefined(env, &mut recv);
        let mut out = std::ptr::null_mut();
        if sys::napi_call_function(env, recv, func, argv.len(), argv.as_ptr(), &mut out)
            != sys::Status::napi_ok
        {
            raw::take_exception(env);
            return None;
        }
        Some(raw::result(env, out))
    })();
    sys::napi_close_handle_scope(env, scope);
    result
}

/// Widened to a full `ffi_arg`, as libffi expects.
unsafe fn write_result(result: *mut u64, ty: &str, (word, float): (u64, f64)) {
    match ty {
        "void" => {}
        "i8" => *result = word as i8 as i64 as u64,
        "i16" => *result = word as i16 as i64 as u64,
        "i32" | "bool" => *result = word as i32 as i64 as u64,
        "u8" => *result = word as u8 as u64,
        "u16" => *result = word as u16 as u64,
        "u32" => *result = word as u32 as u64,
        "f32" => *(result as *mut f32) = float as f32,
        "f64" => *result = float.to_bits(),
        // i64, u64, pointer
        _ => *result = word,
    }
}

unsafe extern "C" fn trampoline(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    cb: &Callback,
) {
    let result: *mut u64 = result;
    let value = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if std::thread::current().id() == cb.owner {
            return invoke(cb, args);
        }
        // napi needs the JS thread.
        if !crate::ui_dispatcher::is_ui_thread_id(cb.owner) {
            return None;
        }
        struct SendPtrs(*const Callback, *const *const c_void);
        unsafe impl Send for SendPtrs {}
        let ptrs = SendPtrs(cb, args);
        crate::ui_dispatcher::run_on_ui_thread_sync(move || {
            let ptrs = ptrs;
            invoke(&*ptrs.0, ptrs.1)
        })
        .flatten()
    }))
    .ok()
    .flatten()
    .unwrap_or((0, 0.0));
    write_result(result, &cb.ret, value);
}

fn release_callback(address: usize) {
    if !CALLBACKS.with(|c| c.borrow_mut().remove(&address)) {
        return;
    }
    let cb = unsafe { Box::from_raw(address as *mut Callback) };
    unsafe {
        low::closure_free(cb.closure);
        sys::napi_delete_reference(cb.env, cb.func);
    }
}

fn callback(ctx: CallContext) -> napi::Result<JsObject> {
    let env: &Env = &ctx.env;
    let ret = string_arg(&ctx, 0);
    let params_arr: JsObject = ctx.get(1)?;
    let func: JsFunction = ctx.get(2)?;
    let mut params = Vec::new();
    for i in 0..params_arr.get_array_length()? {
        params.push(js_to_rust_string(
            env,
            &params_arr.get_element::<JsUnknown>(i)?,
        ));
    }
    let arg_types: Vec<Type> = params
        .iter()
        .map(|t| ffi_type_for(t))
        .collect::<Result<_, _>>()
        .map_err(err)?;
    let ret_type = ffi_type_for(&ret).map_err(err)?;

    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    if unsafe { sys::napi_create_reference(env.raw(), func.raw(), 1, &mut func_ref) }
        != sys::Status::napi_ok
    {
        return Err(err("win32.callback: could not reference the function"));
    }
    let cb = Box::new(Callback {
        env: env.raw(),
        func: func_ref,
        params,
        ret,
        owner: std::thread::current().id(),
        cif: Box::new(Cif::new(arg_types, ret_type)),
        closure: std::ptr::null_mut(),
    });
    let (closure, code): (*mut ffi_closure, CodePtr) = low::closure_alloc();
    if closure.is_null() {
        unsafe { sys::napi_delete_reference(env.raw(), func_ref) };
        return Err(err("win32.callback: could not allocate a closure"));
    }
    let cb = Box::into_raw(cb);
    let prepared = unsafe {
        low::prep_closure(
            closure,
            (*cb).cif.as_raw_ptr(),
            trampoline,
            cb as *const Callback,
            code,
        )
    };
    if prepared.is_err() {
        unsafe {
            low::closure_free(closure);
            sys::napi_delete_reference(env.raw(), func_ref);
            drop(Box::from_raw(cb));
        }
        return Err(err("win32.callback: could not prepare the closure"));
    }
    unsafe { (*cb).closure = closure };
    let address = cb as usize;
    CALLBACKS.with(|c| c.borrow_mut().insert(address));

    let mut obj = env.create_object()?;
    obj.set_named_property("pointer", number_or_bigint_u64(env, code.as_ptr() as u64)?)?;
    let release = env.create_function_from_closure("release", move |ctx| {
        release_callback(address);
        ctx.env.get_undefined()
    })?;
    obj.set_named_property("release", release)?;
    Ok(obj)
}

const CLASS_NAME: PCWSTR = windows::core::w!("NsWin32MessageWindow");

struct Listener {
    env: sys::napi_env,
    func: sys::napi_ref,
}

thread_local! {
    // Keyed by HWND.
    static LISTENERS: RefCell<HashMap<isize, Listener>> = RefCell::new(HashMap::new());
}

fn deliver(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
    let target = LISTENERS.with(|l| l.borrow().get(&hwnd).map(|l| (l.env, l.func)));
    let Some((env, func)) = target else {
        return;
    };
    unsafe {
        let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
        if sys::napi_open_handle_scope(env, &mut scope) != sys::Status::napi_ok {
            return;
        }
        let mut f = std::ptr::null_mut();
        if sys::napi_get_reference_value(env, func, &mut f) == sys::Status::napi_ok && !f.is_null()
        {
            let argv = [
                raw::number(env, msg as f64),
                raw::word(env, wparam as u64, false),
                raw::word(env, lparam as i64 as u64, true),
            ];
            let mut recv = std::ptr::null_mut();
            sys::napi_get_undefined(env, &mut recv);
            let mut out = std::ptr::null_mut();
            if sys::napi_call_function(env, recv, f, argv.len(), argv.as_ptr(), &mut out)
                != sys::Status::napi_ok
            {
                raw::take_exception(env);
            }
        }
        sys::napi_close_handle_scope(env, scope);
    }
}

fn forget(hwnd: isize) {
    if let Some(listener) = LISTENERS.with(|l| l.borrow_mut().remove(&hwnd)) {
        unsafe { sys::napi_delete_reference(listener.env, listener.func) };
        crate::napi_engine::event_loop::release();
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let key = hwnd.0 as isize;
    if msg == WM_NCDESTROY {
        forget(key);
    } else if LISTENERS.with(|l| l.borrow().contains_key(&key)) {
        // Off the UI thread, the listener runs on the window's own thread.
        let (w, l) = (wparam.0, lparam.0);
        if !crate::ui_dispatcher::is_ui_thread()
            || !crate::ui_dispatcher::enqueue_on_ui_thread(move || deliver(key, msg, w, l))
        {
            deliver(key, msg, w, l);
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_class_once() -> napi::Result<()> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| unsafe {
            let hinstance =
                GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                Err("RegisterClassExW returned 0".to_string())
            } else {
                Ok(())
            }
        })
        .clone()
        .map_err(err)
}

/// Top-level, not message-only, so it gets broadcasts such as `TaskbarCreated`.
fn message_window(ctx: CallContext) -> napi::Result<JsUnknown> {
    let env: &Env = &ctx.env;
    let listener: JsFunction = ctx.get(0)?;
    register_class_once()?;
    let hinstance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| err(format!("GetModuleHandleW failed: {e}")))?;
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS_NAME,
            windows::core::w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| err(format!("CreateWindowExW failed: {e}")))?;
    let mut func: sys::napi_ref = std::ptr::null_mut();
    if unsafe { sys::napi_create_reference(env.raw(), listener.raw(), 1, &mut func) }
        != sys::Status::napi_ok
    {
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(err("win32.messageWindow: could not reference the listener"));
    }
    LISTENERS.with(|l| {
        l.borrow_mut().insert(
            hwnd.0 as isize,
            Listener {
                env: env.raw(),
                func,
            },
        )
    });
    // Keeps a standalone runtime's event loop running while the window is open.
    crate::napi_engine::event_loop::retain();
    number_or_bigint_u64(env, hwnd.0 as u64)
}

fn message_window_close(ctx: CallContext) -> napi::Result<JsUnknown> {
    let env: &Env = &ctx.env;
    if let Some(hwnd) = crate::napi_engine::xaml_host::hwnd_from_js(env, &ctx.get::<JsUnknown>(0)?)?
    {
        if LISTENERS.with(|l| l.borrow().contains_key(&(hwnd.0 as isize)))
            && unsafe { IsWindow(Some(hwnd)) }.as_bool()
        {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }
    Ok(as_unknown(env, env.get_undefined()?))
}

pub fn install_win32(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;
    global.set_named_property(
        "__nsWin32Bind",
        env.create_function_from_closure("__nsWin32Bind", bind)?,
    )?;
    let natives: [(&str, fn(CallContext) -> napi::Result<JsUnknown>); 4] = [
        ("__nsWin32CallRaw", call),
        ("__nsWin32Exports", exports),
        ("__nsWin32MessageWindow", message_window),
        ("__nsWin32MessageWindowClose", message_window_close),
    ];
    for (name, f) in natives {
        global.set_named_property(name, env.create_function_from_closure(name, f)?)?;
    }
    global.set_named_property(
        "__nsWin32Callback",
        env.create_function_from_closure("__nsWin32Callback", callback)?,
    )?;

    let func_ctor: JsFunction = global.get_named_property("Function")?;
    let installer_obj = func_ctor.new_instance(&[env.create_string(WIN32_JS)?])?;
    let installer: JsFunction = unsafe { JsFunction::from_raw(env.raw(), installer_obj.raw()) }?;
    installer.call_without_args(None)?;
    Ok(())
}

/// Mirrors the classic `NSWinRT.win32`; ES5 for every engine.
const WIN32_JS: &str = r#"
'use strict';
var g = globalThis;
g.NSWinRT = g.NSWinRT || {};
if (g.NSWinRT.win32) return;
var raw = g.__nsWin32CallRaw;
var bindNative = g.__nsWin32Bind;

function autoType(v) {
    if (v === null || v === undefined) return { type: 'pointer', value: 0 };
    if (typeof v === 'object' && typeof v.type === 'string' && 'value' in v) return v;
    if (typeof v === 'string') return { type: 'wstr', value: v };
    if (typeof v === 'boolean') return { type: 'bool', value: v };
    if (typeof v === 'number') return { type: Number.isInteger(v) ? 'i32' : 'f64', value: v };
    if (typeof v === 'bigint') return { type: 'i64', value: v };
    // buffers and pointer objects
    return { type: 'pointer', value: v };
}

function invoke(dll, name, ret, descriptors) {
    var args = [dll, name, ret];
    for (var i = 0; i < descriptors.length; i++) args.push(descriptors[i].type, descriptors[i].value);
    return raw.apply(null, args);
}

// Binds a native function on the first call, typed from its values. Forwards by position:
// apply with `arguments` would block inlining.
function lazyBound(dll, name, ret, onBound) {
    var bound = null;
    var tried = typeof bindNative !== 'function';
    return function (a, b, c, d, e, f, g, h) {
        if (bound === null && !tried) {
            tried = true;
            var types = [];
            for (var i = 0; i < arguments.length; i++) types.push(autoType(arguments[i]).type);
            try { bound = bindNative(dll, name, ret, types); } catch (e) { bound = null; }
            if (bound !== null && onBound) onBound(bound);
        }
        if (bound === null) {
            return invoke(dll, name, ret, Array.prototype.map.call(arguments, autoType));
        }
        switch (arguments.length) {
            case 0: return bound();
            case 1: return bound(a);
            case 2: return bound(a, b);
            case 3: return bound(a, b, c);
            case 4: return bound(a, b, c, d);
            case 5: return bound(a, b, c, d, e);
            case 6: return bound(a, b, c, d, e, f);
            case 7: return bound(a, b, c, d, e, f, g);
            default: return bound(a, b, c, d, e, f, g, h);
        }
    };
}

var win32 = {
    call: function (dll, name, ret) {
        return invoke(dll, name, ret, Array.prototype.slice.call(arguments, 3));
    },

    /** The first call's argument types lock in. */
    bind: function (dll, ret) {
        ret = ret || 'i32';
        var fns = Object.create(null);
        return new Proxy({}, {
            get: function (_, name) {
                if (typeof name !== 'string') return undefined;
                if (!fns[name]) fns[name] = lazyBound(dll, name, ret, function (bound) { fns[name] = bound; });
                return fns[name];
            },
        });
    },

    /** signatures: { Name: argTypes | { args, ret } } */
    define: function (dll, signatures, defaultRet) {
        var api = {};
        Object.keys(signatures).forEach(function (name) {
            var sig = signatures[name];
            var argTypes = Array.isArray(sig) ? sig : sig.args || [];
            var ret = (!Array.isArray(sig) && sig.ret) || defaultRet || 'i32';
            // Unresolved functions report the error when called.
            var bound = null;
            try { bound = bindNative(dll, name, ret, argTypes); } catch (e) { bound = null; }
            api[name] = bound !== null ? bound : function () {
                var descriptors = Array.prototype.map.call(arguments, function (v, i) {
                    return argTypes[i] ? { type: argTypes[i], value: v } : autoType(v);
                });
                return invoke(dll, name, ret, descriptors);
            };
        });
        return api;
    },

    import: function (dll, ret) {
        var bound = win32.bind(dll, ret);
        JSON.parse(g.__nsWin32Exports(dll)).forEach(function (name) {
            if (!(name in g)) {
                Object.defineProperty(g, name, { value: bound[name], writable: true, configurable: true });
            }
        });
    },

    /** Release only once native code can no longer call it. */
    callback: function (ret, argTypes, fn) {
        if (typeof fn !== 'function') throw new TypeError('win32.callback: fn must be a function');
        return g.__nsWin32Callback(ret, argTypes || [], fn);
    },

    messageWindow: function (listener) {
        if (typeof listener !== 'function') throw new TypeError('win32.messageWindow: listener must be a function');
        var hwnd = g.__nsWin32MessageWindow(listener);
        return {
            hwnd: hwnd,
            close: function () { g.__nsWin32MessageWindowClose(hwnd); },
        };
    },
};

g.NSWinRT.win32 = win32;
"#;
