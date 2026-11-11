//! `NSWinRT.win32.callback` and `NSWinRT.win32.messageWindow` for the classic engine.
//!
//! Native calls from another thread reach the callback's thread through the UI dispatcher.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::thread::ThreadId;

use libffi::low::{self, ffi_cif, ffi_closure, CodePtr};
use libffi::middle::Cif;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, RegisterClassExW, WM_NCDESTROY,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::throw_js_error;
use crate::win32_call::{parse_signature, Kind};

struct Callback {
    /// The creating isolate's `v8::Isolate*`.
    isolate: *mut c_void,
    func: v8::Global<v8::Function>,
    params: Box<[Kind]>,
    ret: Kind,
    owner: ThreadId,
    // The closure points at it.
    cif: Box<Cif>,
    closure: *mut ffi_closure,
}

thread_local! {
    // Live callbacks, so a second release() is a no-op.
    static CALLBACKS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

/// For re-entering the isolate from native code.
fn raw_isolate(scope: &v8::Isolate) -> *mut c_void {
    unsafe { std::mem::transmute(scope.as_raw_isolate_ptr()) }
}

fn with_js<R>(
    isolate: *mut c_void,
    f: impl FnOnce(&mut v8::PinScope<'_, '_>, v8::Local<v8::Context>) -> R,
) -> Option<R> {
    if isolate.is_null() {
        return None;
    }
    // A non-owning handle: the runtime owns the isolate.
    let mut isolate = unsafe { v8::Isolate::from_raw_ptr_unchecked(isolate as *mut _) };
    v8::scope!(scope, &mut isolate);
    let context = scope.get_slot::<v8::Global<v8::Context>>().cloned()?;
    let context = v8::Local::new(scope, &context);
    let scope = &mut v8::ContextScope::new(scope, context);
    Some(f(scope, context))
}

unsafe fn arg_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    kind: Kind,
    slot: *const c_void,
) -> v8::Local<'s, v8::Value> {
    let number = |scope: &mut v8::PinScope<'s, '_>, n: f64| -> v8::Local<'s, v8::Value> {
        v8::Number::new(scope, n).into()
    };
    unsafe {
        match kind {
            Kind::I8 => number(scope, *(slot as *const i8) as f64),
            Kind::I16 => number(scope, *(slot as *const i16) as f64),
            Kind::I32 => number(scope, *(slot as *const i32) as f64),
            Kind::I64 => number(scope, *(slot as *const i64) as f64),
            Kind::U8 => number(scope, *(slot as *const u8) as f64),
            Kind::U16 => number(scope, *(slot as *const u16) as f64),
            Kind::U32 => number(scope, *(slot as *const u32) as f64),
            Kind::U64 => number(scope, *(slot as *const u64) as f64),
            Kind::F32 => number(scope, *(slot as *const f32) as f64),
            Kind::F64 => number(scope, *(slot as *const f64)),
            Kind::Bool => v8::Boolean::new(scope, *(slot as *const i32) != 0).into(),
            Kind::WStr => {
                let p = *(slot as *const *const u16);
                if p.is_null() {
                    return v8::null(scope).into();
                }
                let mut len = 0;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let units = std::slice::from_raw_parts(p, len);
                v8::String::new_from_two_byte(scope, units, v8::NewStringType::Normal)
                    .map_or_else(|| v8::null(scope).into(), |s| s.into())
            }
            Kind::Str => {
                let p = *(slot as *const *const i8);
                if p.is_null() {
                    return v8::null(scope).into();
                }
                let text = std::ffi::CStr::from_ptr(p).to_string_lossy();
                v8::String::new(scope, &text).map_or_else(|| v8::null(scope).into(), |s| s.into())
            }
            Kind::Void | Kind::Pointer => number(scope, *(slot as *const usize) as f64),
        }
    }
}

/// The JS result as (integer, float).
fn result_of(scope: &mut v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> (u64, f64) {
    if v.is_boolean() {
        let b = v.is_true();
        return (b as u64, b as u8 as f64);
    }
    if v.is_big_int() {
        let w = unsafe { v8::Local::<v8::BigInt>::cast_unchecked(v) }
            .i64_value()
            .0;
        return (w as u64, w as f64);
    }
    if v.is_number() {
        let f = unsafe { v8::Local::<v8::Number>::cast_unchecked(v) }.value();
        return (f as i64 as u64, f);
    }
    if v.is_null_or_undefined() {
        return (0, 0.0);
    }
    let f = v.number_value(scope).unwrap_or(0.0);
    (f as i64 as u64, f)
}

/// `None` when it threw.
unsafe fn invoke(cb: &Callback, args: *const *const c_void) -> Option<(u64, f64)> {
    with_js(cb.isolate, |scope, context| {
        v8::tc_scope!(tc, scope);
        let func = v8::Local::new(tc, &cb.func);
        let argv: Vec<v8::Local<v8::Value>> = cb
            .params
            .iter()
            .enumerate()
            .map(|(i, kind)| unsafe { arg_value(tc, *kind, *args.add(i)) })
            .collect();
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let result = func.call(tc, recv, &argv);
        let value = match result {
            Some(v) if !tc.has_caught() => Some(result_of(tc, v)),
            _ => {
                if let Some(message) = tc
                    .exception()
                    .and_then(|e| e.to_string(tc))
                    .map(|s| s.to_rust_string_lossy(tc))
                {
                    crate::store_last_js_error(message);
                }
                tc.reset();
                None
            }
        };
        if !crate::defer_microtask_drain() {
            tc.perform_microtask_checkpoint();
        }
        let _ = context;
        value
    })
    .flatten()
}

/// Widened to a full `ffi_arg`, as libffi expects.
unsafe fn write_result(result: *mut u64, kind: Kind, (word, float): (u64, f64)) {
    unsafe {
        match kind {
            Kind::Void => {}
            Kind::I8 => *result = word as i8 as i64 as u64,
            Kind::I16 => *result = word as i16 as i64 as u64,
            Kind::I32 | Kind::Bool => *result = word as i32 as i64 as u64,
            Kind::U8 => *result = word as u8 as u64,
            Kind::U16 => *result = word as u16 as u64,
            Kind::U32 => *result = word as u32 as u64,
            Kind::F32 => *(result as *mut f32) = float as f32,
            Kind::F64 => *result = float.to_bits(),
            Kind::I64 | Kind::U64 | Kind::Pointer | Kind::WStr | Kind::Str => *result = word,
        }
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
            return unsafe { invoke(cb, args) };
        }
        // JS runs on the thread that owns the isolate.
        if !crate::ui_dispatcher::is_ui_thread_id(cb.owner) {
            return None;
        }
        struct SendPtrs(*const Callback, *const *const c_void);
        unsafe impl Send for SendPtrs {}
        let ptrs = SendPtrs(cb, args);
        crate::ui_dispatcher::run_on_ui_thread_sync(move || {
            let ptrs = ptrs;
            unsafe { invoke(&*ptrs.0, ptrs.1) }
        })
        .flatten()
    }))
    .ok()
    .flatten()
    .unwrap_or((0, 0.0));
    unsafe { write_result(result, cb.ret, value) };
}

fn release_callback(address: usize) {
    if !CALLBACKS.with(|c| c.borrow_mut().remove(&address)) {
        return;
    }
    let cb = unsafe { Box::from_raw(address as *mut Callback) };
    unsafe { low::closure_free(cb.closure) };
}

fn handle_release(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let ext = unsafe { v8::Local::<v8::External>::cast_unchecked(args.data()) };
    release_callback(ext.value() as usize);
}

fn strings_of(scope: &mut v8::PinScope<'_, '_>, list: v8::Local<v8::Value>) -> Option<Vec<String>> {
    let Ok(array) = v8::Local::<v8::Array>::try_from(list) else {
        return Some(Vec::new());
    };
    let mut out = Vec::with_capacity(array.length() as usize);
    for i in 0..array.length() {
        let item = array.get_index(scope, i)?;
        out.push(crate::global_fns::value_to_string(scope, item)?);
    }
    Some(out)
}

/// `__nsWin32Callback(retType, argTypes, fn)`: `{ pointer, release() }`.
pub(crate) fn handle_win32_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let Some(ret) = crate::global_fns::value_to_string(scope, args.get(0)) else {
        throw_js_error(scope, "win32.callback: retType must be a string");
        return;
    };
    let Some(types) = strings_of(scope, args.get(1)) else {
        throw_js_error(
            scope,
            "win32.callback: argTypes must be an array of type names",
        );
        return;
    };
    let Ok(func) = v8::Local::<v8::Function>::try_from(args.get(2)) else {
        throw_js_error(scope, "win32.callback: fn must be a function");
        return;
    };
    let (ret, params) = match parse_signature(&ret, &types) {
        Ok(sig) => sig,
        Err(e) => {
            throw_js_error(scope, &format!("win32.callback: {e}"));
            return;
        }
    };
    let cif = Box::new(Cif::new(
        params.iter().map(|k| k.ffi_type()),
        ret.ffi_type(),
    ));
    let isolate = raw_isolate(scope);
    let cb = Box::into_raw(Box::new(Callback {
        isolate,
        func: v8::Global::new(scope, func),
        params: params.into_iter().collect(),
        ret,
        owner: std::thread::current().id(),
        cif,
        closure: std::ptr::null_mut(),
    }));
    let (closure, code): (*mut ffi_closure, CodePtr) = low::closure_alloc();
    let prepared = !closure.is_null()
        && unsafe {
            low::prep_closure(
                closure,
                (*cb).cif.as_raw_ptr(),
                trampoline,
                cb as *const Callback,
                code,
            )
        }
        .is_ok();
    if !prepared {
        unsafe {
            if !closure.is_null() {
                low::closure_free(closure);
            }
            drop(Box::from_raw(cb));
        }
        throw_js_error(scope, "win32.callback: could not prepare the closure");
        return;
    }
    unsafe { (*cb).closure = closure };
    let address = cb as usize;
    CALLBACKS.with(|c| c.borrow_mut().insert(address));

    let obj = v8::Object::new(scope);
    let pointer_key = v8::String::new(scope, "pointer").unwrap();
    let pointer = v8::Number::new(scope, code.as_ptr() as usize as f64);
    obj.set(scope, pointer_key.into(), pointer.into());
    let release_key = v8::String::new(scope, "release").unwrap();
    let data = v8::External::new(scope, address as *mut c_void);
    if let Some(release) = v8::Function::builder(handle_release)
        .data(data.into())
        .build(scope)
    {
        obj.set(scope, release_key.into(), release.into());
    }
    retval.set(obj.into());
}

const CLASS_NAME: PCWSTR = windows::core::w!("NsWin32MessageWindowClassic");

struct Listener {
    isolate: *mut c_void,
    func: v8::Global<v8::Function>,
}

thread_local! {
    // Keyed by HWND.
    static LISTENERS: RefCell<HashMap<isize, Listener>> = RefCell::new(HashMap::new());
}

fn deliver(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
    let target = LISTENERS.with(|l| l.borrow().get(&hwnd).map(|l| (l.isolate, l.func.clone())));
    let Some((isolate, func)) = target else {
        return;
    };
    with_js(isolate, |scope, _context| {
        v8::tc_scope!(tc, scope);
        let func = v8::Local::new(tc, &func);
        let argv: [v8::Local<v8::Value>; 3] = [
            v8::Number::new(tc, msg as f64).into(),
            v8::Number::new(tc, wparam as f64).into(),
            v8::Number::new(tc, lparam as f64).into(),
        ];
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let _ = func.call(tc, recv, &argv);
        if tc.has_caught() {
            if let Some(message) = tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc))
            {
                crate::store_last_js_error(message);
            }
            tc.reset();
        }
        if !crate::defer_microtask_drain() {
            tc.perform_microtask_checkpoint();
        }
    });
}

fn forget(hwnd: isize) {
    LISTENERS.with(|l| l.borrow_mut().remove(&hwnd));
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
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn register_class_once() -> Result<(), String> {
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
}

/// `__nsWin32MessageWindow(listener)`: a hidden window's HWND. Not message-only, so it gets
/// broadcasts such as `TaskbarCreated`.
pub(crate) fn handle_win32_message_window(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let Ok(listener) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_js_error(scope, "win32.messageWindow: listener must be a function");
        return;
    };
    if let Err(e) = register_class_once() {
        throw_js_error(scope, &format!("win32.messageWindow: {e}"));
        return;
    }
    let hwnd = unsafe {
        GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW failed: {e}"))
            .and_then(|hinstance| {
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
                .map_err(|e| format!("CreateWindowExW failed: {e}"))
            })
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            throw_js_error(scope, &format!("win32.messageWindow: {e}"));
            return;
        }
    };
    LISTENERS.with(|l| {
        l.borrow_mut().insert(
            hwnd.0 as isize,
            Listener {
                isolate: raw_isolate(scope),
                func: v8::Global::new(scope, listener),
            },
        )
    });
    retval.set_double(hwnd.0 as isize as f64);
}

/// `__nsWin32MessageWindowClose(hwnd)`: destroys a window `messageWindow` created.
pub(crate) fn handle_win32_message_window_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue,
) {
    let v = args.get(0);
    let raw = if v.is_big_int() {
        unsafe { v8::Local::<v8::BigInt>::cast_unchecked(v) }
            .i64_value()
            .0 as isize
    } else {
        v.number_value(scope).unwrap_or(0.0) as i64 as isize
    };
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if LISTENERS.with(|l| l.borrow().contains_key(&raw))
        && unsafe { IsWindow(Some(hwnd)) }.as_bool()
    {
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}
