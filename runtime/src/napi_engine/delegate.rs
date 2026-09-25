//! Node-API implementation of the JsDelegate COM bridge: wraps a JS function reference
//! (`napi_ref`) inside a minimal COM object so it can be passed to WinRT event-add methods and
//! delegate-typed parameters. Every delegate type shares one vtable; the per-instance GUID makes
//! QueryInterface answer correctly for each concrete delegate type.
//!
//! Differences from the rusty_v8 JsDelegate (`lib.rs` ~6956-7200), by design:
//! - No isolate pointer / CallbackScope re-entrancy machinery: napi handle scopes nest, and
//!   `napi_call_function` is legal from within an active callback.
//! - No explicit microtask checkpoint: the host (Node/Bun/engine shim) owns draining.
//! - Same threading envelope as that implementation: Invoke and the final Release must happen
//!   on the JS thread (WinRT events fire on the registering apartment's thread, which is ours).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use napi::{sys, CallContext, Env, JsFunction, JsUnknown, NapiRaw, NapiValue, ValueType};
use windows::core::{IUnknown, Interface, GUID, HRESULT};

use crate::napi_engine::ns_proxy::Decl;
use crate::value::NativeType;

/// A delegate parameter's declared class when that class is sealed: the argument's runtime
/// class is then exactly this one, so the wrapper is built from the declaration without asking
/// the object (see [`crate::sealed_class_for_type_name`]).
pub(crate) type SealedParam = Option<(Arc<str>, Decl)>;

#[repr(C)]
pub(crate) struct NapiDelegateVtbl {
    query_interface:
        unsafe extern "system" fn(*mut NapiDelegate, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut NapiDelegate) -> u32,
    release: unsafe extern "system" fn(*mut NapiDelegate) -> u32,
    // Declared with 4 usize params so the same slot works for delegates with 0-4 pointer-sized
    // arguments; extras land in dead registers and are never read (guarded by param_types.len()).
    invoke: unsafe extern "system" fn(*mut NapiDelegate, usize, usize, usize, usize) -> HRESULT,
}

pub(crate) static NAPI_DELEGATE_VTBL: NapiDelegateVtbl = NapiDelegateVtbl {
    query_interface: napi_delegate_query_interface,
    add_ref: napi_delegate_add_ref,
    release: napi_delegate_release,
    invoke: napi_delegate_invoke,
};

pub(crate) struct NapiDelegateData {
    env: sys::napi_env,
    func_ref: sys::napi_ref,
    param_types: Vec<NativeType>,
    /// Aligned with `param_types`; `Some` for pointer parameters declared as a sealed class.
    /// Shorter than `param_types` (or empty) means "resolve at invoke time" for the rest.
    param_classes: Vec<SealedParam>,
}

impl Drop for NapiDelegateData {
    fn drop(&mut self) {
        // Same threading envelope as dropping a v8::Global in the original: the final
        // Release is expected on the JS thread.
        unsafe {
            let _ = sys::napi_delete_reference(self.env, self.func_ref);
        }
    }
}

#[repr(C)]
pub(crate) struct NapiDelegate {
    vtable: *const NapiDelegateVtbl,
    ref_count: AtomicU32,
    guid: GUID,
    data: *mut NapiDelegateData,
}

unsafe impl Send for NapiDelegate {}
unsafe impl Sync for NapiDelegate {}

/// Allocate a NapiDelegate COM object over `func`; returns the IUnknown-compatible pointer
/// (refcount 1, owned by the caller/WinRT callee). Pointer parameters are typed at invoke time
/// through GetRuntimeClassName; see [`make_napi_delegate_typed`] for the declared-type variant.
pub fn make_napi_delegate(
    env: &Env,
    func: &JsFunction,
    guid: GUID,
    param_types: Vec<NativeType>,
) -> Option<*mut c_void> {
    make_napi_delegate_typed(env, func, guid, param_types, Vec::new())
}

/// The sealed-class declarations of a delegate's Invoke parameters, aligned with the
/// `param_types` that [`crate::delegate_info_from_type_sig`] reports for `iid_name`. Only
/// pointer parameters whose declared type is a sealed runtime class get an entry; every other
/// parameter (unsealed classes, interfaces, `Object`, scalars) is `None` and keeps the
/// runtime-class resolution at invoke time. Empty when `iid_name` is not a delegate.
pub(crate) fn delegate_param_classes(iid_name: &str, param_types: &[NativeType]) -> Vec<SealedParam> {
    let Some(sigs) = crate::delegate_param_sigs(iid_name) else {
        return Vec::new();
    };
    param_types
        .iter()
        .zip(sigs.iter())
        .map(|(ty, sig)| match ty {
            NativeType::Pointer => crate::sealed_class_for_type_name(sig),
            _ => None,
        })
        .collect()
}

/// [`make_napi_delegate`] with the parameters' sealed-class declarations resolved up front
/// (`param_classes` from [`delegate_param_classes`]), so an event handler receives its typed
/// arguments without a per-invoke IInspectable QI, GetRuntimeClassName and metadata lookup.
pub(crate) fn make_napi_delegate_typed(
    env: &Env,
    func: &JsFunction,
    guid: GUID,
    param_types: Vec<NativeType>,
    param_classes: Vec<SealedParam>,
) -> Option<*mut c_void> {
    let mut func_ref: sys::napi_ref = std::ptr::null_mut();
    let status =
        unsafe { sys::napi_create_reference(env.raw(), func.raw(), 1, &mut func_ref) };
    if status != sys::Status::napi_ok || func_ref.is_null() {
        return None;
    }
    let data = Box::new(NapiDelegateData {
        env: env.raw(),
        func_ref,
        param_types,
        param_classes,
    });
    let delegate = Box::new(NapiDelegate {
        vtable: &NAPI_DELEGATE_VTBL as *const _,
        ref_count: AtomicU32::new(1),
        guid,
        data: Box::into_raw(data),
    });
    Some(Box::into_raw(delegate) as *mut c_void)
}

/// Invoke a NapiDelegate COM pointer through its vtable — what a WinRT event source does when
/// the event fires. Exposed for the event registry and end-to-end tests.
///
/// # Safety
/// `ptr` must be a live pointer returned by `make_napi_delegate`, called on the JS thread.
pub unsafe fn invoke_delegate_raw(ptr: *mut c_void, p0: usize, p1: usize, p2: usize) -> i32 {
    let d = ptr as *mut NapiDelegate;
    (((*(*d).vtable).invoke)(d, p0, p1, p2, 0)).0
}

/// Release one COM reference on a NapiDelegate (frees it at zero).
///
/// # Safety
/// `ptr` must be a live pointer returned by `make_napi_delegate`; the final release must
/// happen on the JS thread (it deletes the napi function reference).
pub unsafe fn release_delegate_raw(ptr: *mut c_void) -> u32 {
    let d = ptr as *mut NapiDelegate;
    ((*(*d).vtable).release)(d)
}

unsafe extern "system" fn napi_delegate_query_interface(
    this: *mut NapiDelegate,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let d = &*this;
    if *iid == IUnknown::IID || *iid == d.guid {
        *out = this as *mut c_void;
        napi_delegate_add_ref(this);
        HRESULT(0)
    } else {
        *out = std::ptr::null_mut();
        HRESULT(0x80004002u32 as i32)
    }
}

unsafe extern "system" fn napi_delegate_add_ref(this: *mut NapiDelegate) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn napi_delegate_release(this: *mut NapiDelegate) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        let b = Box::from_raw(this);
        drop(Box::from_raw(b.data));
    }
    prev - 1
}

unsafe extern "system" fn napi_delegate_invoke(
    this: *mut NapiDelegate,
    p0: usize,
    p1: usize,
    p2: usize,
    _p3: usize,
) -> HRESULT {
    // catch_unwind so Rust panics cannot cross the WinRT C++ caller stack (UB / CLR FailFast).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        napi_delegate_invoke_inner(this, p0, p1, p2)
    }));
    match result {
        Ok(hr) => hr,
        Err(_) => HRESULT(0x80004005u32 as i32),
    }
}

fn napi_delegate_invoke_inner(this: *mut NapiDelegate, p0: usize, p1: usize, p2: usize) -> HRESULT {
    const E_FAIL: HRESULT = HRESULT(0x80004005u32 as i32);
    if this.is_null() {
        return E_FAIL;
    }
    let data = unsafe {
        let data_ptr = (*this).data;
        if data_ptr.is_null() {
            return E_FAIL;
        }
        &*data_ptr
    };
    let env = data.env;
    if env.is_null() {
        return E_FAIL;
    }

    unsafe {
        let mut scope: sys::napi_handle_scope = std::ptr::null_mut();
        if sys::napi_open_handle_scope(env, &mut scope) != sys::Status::napi_ok {
            return E_FAIL;
        }
        // Everything below must reach close_handle_scope; keep a single exit.
        let hr = (|| -> HRESULT {
            let mut func: sys::napi_value = std::ptr::null_mut();
            if sys::napi_get_reference_value(env, data.func_ref, &mut func)
                != sys::Status::napi_ok
                || func.is_null()
            {
                return E_FAIL;
            }

            let params_raw = [p0, p1, p2];
            let n = data.param_types.len().min(3);
            let mut js_args: [sys::napi_value; 4] = [std::ptr::null_mut(); 4];
            for i in 0..n {
                let raw = params_raw[i];
                let class = data.param_classes.get(i).and_then(|c| c.as_ref());
                let val = match delegate_param_to_napi(env, raw, &data.param_types[i], class) {
                    Some(v) => v,
                    None => return E_FAIL,
                };
                js_args[i] = val;
            }

            let mut recv: sys::napi_value = std::ptr::null_mut();
            let _ = sys::napi_get_undefined(env, &mut recv);
            let mut call_result: sys::napi_value = std::ptr::null_mut();
            let status = sys::napi_call_function(
                env,
                recv,
                func,
                n,
                js_args.as_ptr(),
                &mut call_result,
            );
            if status != sys::Status::napi_ok {
                // A JS exception must not escape into WinRT C++ frames: capture it into the
                // runtime's last-error slot (mirrors the TryCatch in the v8 original).
                let mut exc: sys::napi_value = std::ptr::null_mut();
                if sys::napi_get_and_clear_last_exception(env, &mut exc) == sys::Status::napi_ok
                    && !exc.is_null()
                {
                    if let Some(msg) = napi_value_to_rust_string(env, exc) {
                        crate::store_last_js_error(msg);
                    }
                }
            }
            HRESULT(0)
        })();
        let _ = sys::napi_close_handle_scope(env, scope);
        hr
    }
}

/// Convert one raw delegate Invoke parameter to a napi value per its NativeType — builds the
/// JS-visible arguments the delegate callback is invoked with.
///
/// Pointer params declared as a sealed class (`class` is `Some`) wrap straight from that
/// declaration. Otherwise the concrete WinRT type is resolved via
/// `ns_proxy::try_wrap_inspectable_pointer` (INSTANCE_CACHE / GetRuntimeClassName) so the JS
/// callback receives a fully typed proxy (property/method access); a raw external is the
/// fallback when that resolution fails.
unsafe fn delegate_param_to_napi(
    env: sys::napi_env,
    raw: usize,
    ty: &NativeType,
    class: Option<&(Arc<str>, Decl)>,
) -> Option<sys::napi_value> {
    let mut out: sys::napi_value = std::ptr::null_mut();
    let ok = match ty {
        NativeType::Pointer => {
            if raw == 0 {
                sys::napi_get_null(env, &mut out)
            } else {
                let env_obj = Env::from_raw(env);
                // The parameter is borrowed from the event source for the duration of Invoke;
                // the wrapper needs its own reference (AddRef via clone of a ManuallyDrop view).
                let sealed = class.and_then(|(name, decl)| {
                    let owned: IUnknown = {
                        let borrowed =
                            std::mem::ManuallyDrop::new(IUnknown::from_raw(raw as *mut c_void));
                        (*borrowed).clone()
                    };
                    crate::napi_engine::ns_proxy::create_instance_proxy(
                        &env_obj,
                        name,
                        decl.clone(),
                        owned,
                    )
                    .ok()
                });
                if let Some(proxy) = sealed {
                    out = proxy.raw();
                    sys::Status::napi_ok
                } else if let Some(proxy) =
                    crate::napi_engine::ns_proxy::try_wrap_inspectable_pointer(
                        &env_obj,
                        raw as *mut c_void,
                    )
                {
                    out = proxy.raw();
                    sys::Status::napi_ok
                } else {
                    extern "C" fn noop_finalize(
                        _env: sys::napi_env,
                        _data: *mut c_void,
                        _hint: *mut c_void,
                    ) {
                    }
                    sys::napi_create_external(
                        env,
                        raw as *mut c_void,
                        Some(noop_finalize),
                        std::ptr::null_mut(),
                        &mut out,
                    )
                }
            }
        }
        NativeType::Bool => sys::napi_get_boolean(env, (raw as u8) != 0, &mut out),
        NativeType::U8 => sys::napi_create_uint32(env, raw as u8 as u32, &mut out),
        NativeType::I8 => sys::napi_create_int32(env, raw as i8 as i32, &mut out),
        NativeType::U16 => sys::napi_create_uint32(env, raw as u16 as u32, &mut out),
        NativeType::I16 => sys::napi_create_int32(env, raw as i16 as i32, &mut out),
        NativeType::U32 => sys::napi_create_uint32(env, raw as u32, &mut out),
        NativeType::I32 => sys::napi_create_int32(env, raw as i32, &mut out),
        NativeType::U64 => sys::napi_create_double(env, raw as u64 as f64, &mut out),
        NativeType::I64 => sys::napi_create_double(env, raw as i64 as f64, &mut out),
        _ => sys::napi_get_undefined(env, &mut out),
    };
    if ok == sys::Status::napi_ok {
        Some(out)
    } else {
        None
    }
}

/// Installs `__nsAsDelegate(typeName, fn)` + the `NSWinRT.asDelegate` JS surface. Resolves the
/// delegate's GUID + parameter types from WinRT metadata via `crate::delegate_info_from_type_sig`
/// and wraps `fn` with `make_napi_delegate`.
pub fn install_as_delegate(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;

    let as_delegate_fn =
        env.create_function_from_closure("__nsAsDelegate", |ctx: CallContext| {
            native_as_delegate(&ctx)
        })?;
    global.set_named_property("__nsAsDelegate", as_delegate_fn)?;

    let func_ctor: JsFunction = global.get_named_property("Function")?;
    let body = env.create_string(AS_DELEGATE_HELPER_JS)?;
    let installer_obj = func_ctor.new_instance(&[body])?;
    let installer: JsFunction = unsafe { JsFunction::from_raw(env.raw(), installer_obj.raw()) }?;
    installer.call_without_args(None)?;
    Ok(())
}

fn native_as_delegate(ctx: &CallContext) -> napi::Result<JsUnknown> {
    if ctx.length < 2 {
        return Err(napi::Error::from_reason(
            "__nsAsDelegate(typeName, fn): expected 2 arguments".to_string(),
        ));
    }
    let type_name_val = ctx.get::<JsUnknown>(0)?;
    if !matches!(type_name_val.get_type(), Ok(ValueType::String)) {
        return Err(napi::Error::from_reason(
            "__nsAsDelegate: first argument must be a string".to_string(),
        ));
    }
    let type_name_str: napi::JsString = unsafe { type_name_val.cast() };
    let type_name = type_name_str
        .into_utf8()?
        .as_str()
        .map_err(|e| napi::Error::from_reason(e.to_string()))?
        .to_owned();

    let func: JsFunction = ctx.get(1)?;

    let Some((guid, param_types)) = crate::delegate_info_from_type_sig(&type_name) else {
        return Err(napi::Error::from_reason(format!(
            "__nsAsDelegate: unknown delegate type '{type_name}'"
        )));
    };

    let param_classes = delegate_param_classes(&type_name, &param_types);
    let Some(ptr) = make_napi_delegate_typed(&ctx.env, &func, guid, param_types, param_classes)
    else {
        return Err(napi::Error::from_reason(
            "__nsAsDelegate: failed to create native delegate".to_string(),
        ));
    };

    let handle = crate::napi_engine::value::external_from_ptr(&ctx.env, ptr)
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    let mut result = ctx.env.create_object()?;
    result.set_named_property("handle", handle)?;
    Ok(crate::napi_engine::value::as_unknown(&ctx.env, result))
}

/// JS half — supports both `asDelegate(fn)` and `asDelegate(typeName, fn)` call shapes.
const AS_DELEGATE_HELPER_JS: &str = r#"
'use strict';
(function () {
    globalThis.NSWinRT = globalThis.NSWinRT || {};
    globalThis.NSWinRT.asDelegate = function (typeNameOrFn, fn) {
        // Two-argument form: asDelegate(typeName, fn) -> COM-backed JsDelegate
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
})();
'as-delegate-ok'
"#;

/// Stringify a napi value (used for exception capture); None on failure.
unsafe fn napi_value_to_rust_string(env: sys::napi_env, value: sys::napi_value) -> Option<String> {
    let mut coerced: sys::napi_value = std::ptr::null_mut();
    if sys::napi_coerce_to_string(env, value, &mut coerced) != sys::Status::napi_ok {
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
    if sys::napi_get_value_string_utf8(
        env,
        coerced,
        buf.as_mut_ptr() as *mut _,
        buf.len(),
        &mut written,
    ) != sys::Status::napi_ok
    {
        return None;
    }
    buf.truncate(written);
    String::from_utf8(buf).ok()
}
