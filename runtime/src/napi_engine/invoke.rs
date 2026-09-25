//! Resolves WinRT classes via metadata, activates factories, and drives `MethodCall::call_napi`
//! end-to-end — the invocation layer the ns_proxy traps call into.
//!
//! `invoke_static` / `invoke_instance` are the napi analogs of what the rusty_v8 interceptor
//! callbacks do when JS calls a WinRT method: metadata lookup → MethodCall construction →
//! marshaled invoke → return-value conversion.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use napi::{Env, JsUnknown};
use windows::core::{IUnknown, Interface, HRESULT};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_SINGLETHREADED};

use crate::class_helpers::find_class_method;
use crate::error::{generic_error, type_error, AnyError};
use crate::method_call::{MethodCall, PreparedCall};
use crate::napi_engine::ns_hostobject::HostHandle;
use crate::napi_engine::value as nv;
use crate::property_call::{PreparedProperty, PropertyCall};
use crate::ReturnKind;
use crate::value::NativeType;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::Declaration;
use metadata::meta_data_reader::MetadataReader;

/// WinRT must be initialized on this thread before any metadata/factory call. Mirrors
/// `Runtime::new`'s tolerance: an already-initialized (or differently-moded) apartment is fine.
/// Runs once per thread — this sits on every WinRT invocation, so the RoInitialize/scan/
/// dispatcher work must not repeat per call.
pub fn ensure_winrt_initialized() {
    thread_local! {
        static WINRT_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if WINRT_READY.with(|r| r.get()) {
        return;
    }
    crate::napi_engine::crash_reporter::install();
    let _ = unsafe { RoInitialize(RO_INIT_SINGLETHREADED) };
    // Runtime::new's third-party winmd auto-scan doesn't run on the napi path (no Runtime),
    // so do it here once (cwd + addon dir) — WebView2 / app types resolve without an explicit
    // registerWinmd call.
    crate::napi_engine::interop::scan_default_winmd_dirs();
    // Give the JS thread a DispatcherQueue (parity with Runtime::new) — WinRT APIs that post
    // back to the creating thread (composition, XAML islands via WindowsXamlManager) need one;
    // it is drained by the existing message pumping. Idempotent, cheap when already present.
    crate::ui_dispatcher::init_ui_dispatcher();
    WINRT_READY.with(|r| r.set(true));
}

fn class_declaration(
    class_name: &str,
) -> Result<std::sync::Arc<parking_lot::RwLock<dyn Declaration>>, AnyError> {
    MetadataReader::find_by_name(class_name)
        .ok_or_else(|| type_error(format!("WinRT type not found: {class_name}")))
}

/// Convert a finished call's `(hr, result)` into a JS value using the declared return type.
/// Scalars/strings are read from the stable return buffer; enum returns are the slot value
/// itself; reference types resolve to typed instance proxies (external fallback for structs
/// and unresolvable types). Shared by method and property invocation.
pub(crate) fn convert_call_result(
    env: &Env,
    kind: &ReturnKind,
    is_void: bool,
    return_type: &str,
    hr: HRESULT,
    result: *mut c_void,
) -> Result<JsUnknown, AnyError> {
    if hr.is_err() {
        let detail = crate::error::format_hresult_message(hr);
        return Err(generic_error(format!("WinRT call failed: {detail}")));
    }
    if is_void {
        let u = env
            .get_undefined()
            .map_err(|e| type_error(e.to_string()))?;
        return Ok(nv::as_unknown(env, u));
    }
    // Fast paths off the precomputed ReturnKind. Everything else (unsealed classes, which may
    // come back as a derived runtime class; interfaces; enums; structs; Guid; Object; pointer
    // primitives) takes the string-driven resolution below.
    match kind {
        ReturnKind::Object {
            decl,
            type_name,
            sealed: true,
        } => {
            if result.is_null() {
                let n = env.get_null().map_err(|e| type_error(e.to_string()))?;
                return Ok(nv::as_unknown(env, n));
            }
            // The callee handed back a +1 reference; the wrapper takes ownership of it. A
            // sealed class has no derived runtime classes, so the declaration is exact and no
            // IInspectable QI / GetRuntimeClassName / metadata lookup is needed.
            let instance = unsafe { IUnknown::from_raw(result) };
            let proxy = crate::napi_engine::ns_proxy::create_instance_proxy(
                env,
                type_name,
                decl.clone(),
                instance,
            )
            .map_err(|e| generic_error(e.to_string()))?;
            return Ok(nv::as_unknown(env, proxy));
        }
        ReturnKind::Primitive(nt) => match nt {
            NativeType::Pointer
            | NativeType::Buffer
            | NativeType::Function
            | NativeType::Void
            | NativeType::Struct(_) => {}
            _ => {
                // Scalar or String: `result` points at the stable return buffer.
                return unsafe { nv::read_value_from_ptr(env, result as *const c_void, nt) };
            }
        },
        _ => {}
    }
    if let Ok(nt) = NativeType::try_from(return_type) {
        match nt {
            NativeType::Pointer | NativeType::Buffer | NativeType::Function | NativeType::Void => {
            }
            _ => {
                // Scalar or String: `result` points at the stable return buffer.
                return unsafe { nv::read_value_from_ptr(env, result as *const c_void, &nt) };
            }
        }
    }
    // Dotted WinRT return type: enums are the slot VALUE (not a pointer); structs stay
    // externals for now; classes/interfaces wrap into typed proxies.
    if return_type.contains('.') {
        if let Some(declaration) = MetadataReader::find_by_name(
            crate::helpers::strip_generic_suffix(return_type),
        ) {
            match declaration.read().kind() {
                metadata::declarations::declaration::DeclarationKind::Enum => {
                    let v = result as usize as u32;
                    let js = env
                        .create_uint32(v)
                        .map_err(|e| type_error(e.to_string()))?;
                    return Ok(nv::as_unknown(env, js));
                }
                metadata::declarations::declaration::DeclarationKind::Struct => {
                    if result.is_null() {
                        let u = env.get_undefined().map_err(|e| type_error(e.to_string()))?;
                        return Ok(nv::as_unknown(env, u));
                    }
                    let obj = crate::napi_engine::ns_proxy::create_struct_object_from_raw(
                        env,
                        &declaration,
                        result as *const u8,
                    )
                    .map_err(|e| generic_error(e.to_string()))?;
                    return Ok(nv::as_unknown(env, obj));
                }
                metadata::declarations::declaration::DeclarationKind::Interface
                | metadata::declarations::declaration::DeclarationKind::GenericInterface
                | metadata::declarations::declaration::DeclarationKind::GenericInterfaceInstance => {
                    // Interface-typed return (IAsyncAction/Operation, IVector, IMap, …). The
                    // concrete object often has no discoverable runtime-class name, so wrap by
                    // the STATIC return type's interface declaration.
                    if result.is_null() {
                        let u = env.get_undefined().map_err(|e| type_error(e.to_string()))?;
                        return Ok(nv::as_unknown(env, u));
                    }
                    let instance = unsafe { IUnknown::from_raw(result) };
                    // A closed-generic return type ("IMapView`2<String, String>") must wrap with
                    // the *instance* declaration (correct parameterized IID + type args) — the
                    // stripped lookup above only found the open generic.
                    let wrap_decl = if return_type.contains('<') {
                        MetadataReader::find_by_name(return_type)
                            .unwrap_or_else(|| declaration.clone())
                    } else {
                        declaration.clone()
                    };
                    let proxy = crate::napi_engine::ns_proxy::create_instance_proxy(
                        env,
                        return_type,
                        wrap_decl,
                        instance,
                    )
                    .map_err(|e| generic_error(e.to_string()))?;
                    return Ok(nv::as_unknown(env, proxy));
                }
                _ => {}
            }
        }
    }
    if !result.is_null() {
        if let Some(proxy) = crate::napi_engine::ns_proxy::try_wrap_inspectable_pointer(env, result)
        {
            // The wrapper took its own reference (clone); release the +1 the call handed us,
            // exactly like the unbox branch below.
            unsafe { IUnknown::from_raw(result) };
            return Ok(nv::as_unknown(env, proxy));
        }
        // Boxed primitive (IReference<T> from e.g. PropertySet.Lookup) → JS primitive.
        if let Some(v) = nv::try_unbox_property_value(env, result) {
            // The boxed COM object is not wrapped by anything JS-side; release the ref the
            // call handed us now that its value is extracted.
            unsafe { IUnknown::from_raw(result) };
            return Ok(v);
        }
    }
    unsafe { nv::read_return_value(env, result, &NativeType::Pointer) }
}

fn convert_result(
    env: &Env,
    mc: &MethodCall,
    hr: HRESULT,
    result: *mut c_void,
) -> Result<JsUnknown, AnyError> {
    convert_call_result(env, mc.return_kind(), mc.is_void(), mc.return_type(), hr, result)
}

/// Invoke a method declared on a (possibly generic) interface, via QI to `iid`. Used by
/// interface-instance proxies (IAsyncOperation, IVector, IMap, …).
pub fn invoke_interface_method(
    env: &Env,
    instance: IUnknown,
    method: &metadata::declarations::method_declaration::MethodDeclaration,
    iid: windows::core::GUID,
    type_args: Vec<String>,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let Some(mut pc) = crate::property_call::PropertyCall::new_method_for_interface(
        method, instance, iid, type_args,
    ) else {
        return Err(generic_error(format!(
            "interface method '{}' construction failed",
            method.name()
        )));
    };
    let (hr, result, _outs) = pc.call_napi(env, args);
    convert_call_result(env, pc.return_kind(), pc.is_void(), pc.return_type(), hr, result)
}

/// Get/set a property declared on a (possibly generic) interface, via QI to `iid`.
pub fn invoke_interface_property(
    env: &Env,
    instance: IUnknown,
    property: &metadata::declarations::property_declaration::PropertyDeclaration,
    iid: windows::core::GUID,
    type_args: Vec<String>,
    value: Option<&JsUnknown>,
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let is_setter = value.is_some();
    let Some(mut pc) = crate::property_call::PropertyCall::new_for_interface(
        property, is_setter, instance, false, iid, type_args,
    ) else {
        return Err(generic_error(format!(
            "interface property '{}' construction failed",
            property.name()
        )));
    };
    let args: Vec<JsUnknown> = match value {
        Some(v) => vec![nv::dup(env, v)],
        None => Vec::new(),
    };
    let (hr, result, _outs) = pc.call_napi(env, &args);
    if is_setter {
        if hr.is_err() {
            let detail = crate::error::format_hresult_message(hr);
            return Err(generic_error(format!(
                "Property set '{}' failed: {detail}",
                property.name()
            )));
        }
        let u = env.get_undefined().map_err(|e| type_error(e.to_string()))?;
        return Ok(nv::as_unknown(env, u));
    }
    convert_call_result(env, pc.return_kind(), pc.is_void(), pc.return_type(), hr, result)
}

/// Get or set a WinRT property on an owned COM reference via `PropertyCall::call_napi`.
/// Getter: `value = None`; setter: `value = Some(&js)` (returns undefined).
pub fn invoke_property(
    env: &Env,
    instance: IUnknown,
    property: &metadata::declarations::property_declaration::PropertyDeclaration,
    value: Option<&JsUnknown>,
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let is_setter = value.is_some();
    let Some(mut pc) = crate::property_call::PropertyCall::new(property, is_setter, instance, false)
    else {
        return Err(generic_error(format!(
            "PropertyCall construction failed for '{}'",
            property.name()
        )));
    };
    let args: Vec<JsUnknown> = match value {
        Some(v) => vec![nv::dup(env, v)],
        None => Vec::new(),
    };
    let (hr, result, _outs) = pc.call_napi(env, &args);
    if is_setter {
        if hr.is_err() {
            let detail = crate::error::format_hresult_message(hr);
            return Err(generic_error(format!(
                "Property set '{}' failed: {detail}",
                property.name()
            )));
        }
        let u = env
            .get_undefined()
            .map_err(|e| type_error(e.to_string()))?;
        return Ok(nv::as_unknown(env, u));
    }
    convert_call_result(env, pc.return_kind(), pc.is_void(), pc.return_type(), hr, result)
}

/// Invoke an instance method on a host object's own COM reference: the call borrows the
/// interface pointer from the handle's QI cache (no AddRef, no QueryInterface, no Release on
/// the repeat path) and the return value is converted straight from the call's ReturnKind.
pub(crate) fn invoke_host_method(
    env: &Env,
    handle: &HostHandle,
    method: &metadata::declarations::method_declaration::MethodDeclaration,
    is_sealed: bool,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let mut mc = MethodCall::new_cached(method, is_sealed, handle.instance(), handle.qi());
    if let Some(msg) = mc.init_error_message() {
        return Err(generic_error(msg.to_string()));
    }
    let (hr, result, _out) = mc.call_napi(env, args);
    convert_call_result(env, mc.return_kind(), mc.is_void(), mc.return_type(), hr, result)
}

/// Get (`value = None`) or set (`value = Some`) a property on a host object's own COM
/// reference through the handle's QI cache; see [`invoke_host_method`].
pub(crate) fn invoke_host_property(
    env: &Env,
    handle: &HostHandle,
    property: &metadata::declarations::property_declaration::PropertyDeclaration,
    value: Option<&JsUnknown>,
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let is_setter = value.is_some();
    let Some(mut pc) =
        PropertyCall::new_cached(property, is_setter, handle.instance(), handle.qi())
    else {
        return Err(generic_error(format!(
            "PropertyCall construction failed for '{}'",
            property.name()
        )));
    };
    let (hr, result, _outs) = match value {
        Some(v) => {
            let args = [nv::dup(env, v)];
            pc.call_napi(env, &args)
        }
        None => pc.call_napi(env, &[]),
    };
    if is_setter {
        if hr.is_err() {
            let detail = crate::error::format_hresult_message(hr);
            return Err(generic_error(format!(
                "Property set '{}' failed: {detail}",
                property.name()
            )));
        }
        let u = env.get_undefined().map_err(|e| type_error(e.to_string()))?;
        return Ok(nv::as_unknown(env, u));
    }
    convert_call_result(env, pc.return_kind(), pc.is_void(), pc.return_type(), hr, result)
}

/// Invoke a method through a [`PreparedCall`] (a static method or constructor resolved once on
/// its activation factory): the call borrows the prepared interface pointer and vtable slot.
pub(crate) fn invoke_prepared_method(
    env: &Env,
    prepared: &PreparedCall,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    let mut mc = MethodCall::from_prepared(prepared);
    let (hr, result, _out) = mc.call_napi(env, args);
    convert_call_result(env, mc.return_kind(), mc.is_void(), mc.return_type(), hr, result)
}

/// Get (`value = None`) or set (`value = Some`) a property through a [`PreparedProperty`]
/// (a static accessor resolved once on its activation factory).
pub(crate) fn invoke_prepared_property(
    env: &Env,
    prepared: &PreparedProperty,
    property: &metadata::declarations::property_declaration::PropertyDeclaration,
    value: Option<&JsUnknown>,
) -> Result<JsUnknown, AnyError> {
    let mut pc = PropertyCall::from_prepared(prepared);
    let (hr, result, _outs) = match value {
        Some(v) => {
            let args = [nv::dup(env, v)];
            pc.call_napi(env, &args)
        }
        None => pc.call_napi(env, &[]),
    };
    if value.is_some() {
        if hr.is_err() {
            let detail = crate::error::format_hresult_message(hr);
            return Err(generic_error(format!(
                "Property set '{}' failed: {detail}",
                property.name()
            )));
        }
        let u = env.get_undefined().map_err(|e| type_error(e.to_string()))?;
        return Ok(nv::as_unknown(env, u));
    }
    convert_call_result(env, pc.return_kind(), pc.is_void(), pc.return_type(), hr, result)
}

/// Invoke a static WinRT method: activation factory → MethodCall → call_napi.
pub fn invoke_static(
    env: &Env,
    class_name: &str,
    method_name: &str,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let declaration = class_declaration(class_name)?;
    let lock = declaration.read();
    let class = lock
        .as_any()
        .downcast_ref::<ClassDeclaration>()
        .ok_or_else(|| type_error(format!("{class_name} is not a runtime class")))?;
    let method = find_class_method(class, method_name)
        .ok_or_else(|| type_error(format!("{class_name}.{method_name} not found")))?;
    invoke_static_method(env, class_name, &method, class.is_sealed(), args)
}

/// Invoke a static WinRT method whose declaration is already resolved — the host-ctor
/// closures capture the `MethodDeclaration` at build time, so per call this is only:
/// factory cache hit → MethodCall (cached StaticInfo) → call_napi.
pub fn invoke_static_method(
    env: &Env,
    class_name: &str,
    method: &metadata::declarations::method_declaration::MethodDeclaration,
    is_sealed: bool,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let factory = crate::class_activation_factory(class_name)
        .map_err(|e| generic_error(format!("activation factory failed: {e}")))?;

    let mut mc = MethodCall::new(method, is_sealed, factory, false);
    if let Some(msg) = mc.init_error_message() {
        return Err(generic_error(msg.to_string()));
    }
    let (hr, result, _out) = mc.call_napi(env, args);
    convert_result(env, &mc, hr, result)
}

/// Invoke an instance method whose declaration is already resolved (host-prototype closures
/// capture it at build time). The pointer is borrowed: cloned (AddRef) for the call object.
pub fn invoke_instance_method(
    env: &Env,
    instance_ptr: *mut c_void,
    method: &metadata::declarations::method_declaration::MethodDeclaration,
    is_sealed: bool,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    if instance_ptr.is_null() {
        return Err(type_error("null WinRT instance"));
    }
    let instance: IUnknown = unsafe {
        let borrowed = ManuallyDrop::new(IUnknown::from_raw(instance_ptr));
        (*borrowed).clone()
    };
    invoke_instance_method_owned(env, instance, method, is_sealed, args)
}

/// Owned-instance flavor of [`invoke_instance_method`] — used by closures that hold an
/// `IUnknown` (Proxy instance state) rather than a raw pointer.
pub fn invoke_instance_method_owned(
    env: &Env,
    instance: IUnknown,
    method: &metadata::declarations::method_declaration::MethodDeclaration,
    is_sealed: bool,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let mut mc = MethodCall::new(method, is_sealed, instance, false);
    if let Some(msg) = mc.init_error_message() {
        return Err(generic_error(msg.to_string()));
    }
    let (hr, result, _out) = mc.call_napi(env, args);
    convert_result(env, &mc, hr, result)
}

/// Invoke an instance WinRT method on a raw COM pointer (as carried by a pointer external).
/// The pointer is borrowed: it is AddRef'd for the duration of the call object.
pub fn invoke_instance(
    env: &Env,
    instance_ptr: *mut c_void,
    class_name: &str,
    method_name: &str,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    if instance_ptr.is_null() {
        return Err(type_error("null WinRT instance"));
    }
    let declaration = class_declaration(class_name)?;
    // Borrow → owned: clone AddRefs so MethodCall's ownership is balanced.
    let instance: IUnknown = unsafe {
        let borrowed = ManuallyDrop::new(IUnknown::from_raw(instance_ptr));
        (*borrowed).clone()
    };
    invoke_instance_owned(env, instance, class_name, &declaration, method_name, args)
}

/// Invoke an instance method on an owned COM reference — the primitive the instance-proxy
/// method trap uses directly.
pub fn invoke_instance_owned(
    env: &Env,
    instance: IUnknown,
    class_name: &str,
    declaration: &std::sync::Arc<parking_lot::RwLock<dyn Declaration>>,
    method_name: &str,
    args: &[JsUnknown],
) -> Result<JsUnknown, AnyError> {
    ensure_winrt_initialized();
    let lock = declaration.read();
    let class = lock
        .as_any()
        .downcast_ref::<ClassDeclaration>()
        .ok_or_else(|| type_error(format!("{class_name} is not a runtime class")))?;
    let method = find_class_method(class, method_name)
        .ok_or_else(|| type_error(format!("{class_name}.{method_name} not found")))?;

    let mut mc = MethodCall::new(&method, class.is_sealed(), instance, false);
    if let Some(msg) = mc.init_error_message() {
        return Err(generic_error(msg.to_string()));
    }
    let (hr, result, _out) = mc.call_napi(env, args);
    convert_result(env, &mc, hr, result)
}
