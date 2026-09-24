use crate::class_helpers::{
    class_has_member_named, collect_class_methods, collect_class_properties_with_declaring,
    find_class_method, find_class_property, find_event_methods, find_interface_event_methods,
    find_static_property_declaring_class,
};
use crate::error;
use crate::generic_method_call::GenericMethodCall;
use crate::method_call::MethodCall;
use crate::property_call::PropertyCall;
use crate::value::{
    ffi_parse_bool_arg, ffi_parse_buffer_arg, ffi_parse_f32_arg, ffi_parse_f64_arg,
    ffi_parse_function_arg, ffi_parse_i16_arg, ffi_parse_i32_arg, ffi_parse_i64_arg,
    ffi_parse_i8_arg, ffi_parse_isize_arg, ffi_parse_pointer_arg, ffi_parse_string_arg,
    ffi_parse_struct_arg, ffi_parse_u16_arg, ffi_parse_u32_arg, ffi_parse_u64_arg,
    ffi_parse_u8_arg, ffi_parse_usize_arg, read_value_from_ptr, set_ret_val, NativeType,
    NativeValue, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER,
};
use crate::{
    class_activation_factory, delegate_info_from_add_method, js_delegate_params_from_declaration,
    resolve_class_factory_from_parent, throw_js_error, DeclarationFFI, JsDelegate, JsDelegateData,
    ReturnKind, JS_DELEGATE_VTBL,
};
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::declarations::delegate_declaration::generic_delegate_declaration::GenericDelegateDeclaration;
use metadata::declarations::delegate_declaration::generic_delegate_instance_declaration::GenericDelegateInstanceDeclaration;
use metadata::declarations::delegate_declaration::{DelegateDeclaration, DelegateDeclarationImpl};
use metadata::declarations::enum_declaration::EnumDeclaration;
use metadata::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::namespace_declaration::NamespaceDeclaration;
use metadata::declarations::property_declaration::PropertyDeclaration;
use metadata::declarations::struct_declaration::StructDeclaration;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
use metadata::value::Value;
use parking_lot::RwLock;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
use v8::{FunctionTemplate, Local};
use windows::core::{IInspectable, IUnknown, Interface, HSTRING};
use windows::Foundation::Collections::IPropertySet;
use windows::Foundation::PropertyValue;
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::WinRT::IActivationFactory;
use windows::Win32::UI::Shell::IInitializeWithWindow;

// Track constructors currently being built on this thread to avoid re-entrant
// template/property mutations that can corrupt V8 descriptor arrays.
thread_local!(static CREATING_CTORS: RefCell<Vec<String>> = RefCell::new(Vec::new()));

thread_local!(static SHARED_METHOD_FNS: RefCell<ahash::AHashMap<String, v8::Global<v8::Function>>> = RefCell::new(ahash::AHashMap::new()));

/// Per-isolate cache of instance wrapper templates, keyed by resolved runtime
/// class name. Stored in an isolate slot so the `v8::Global`s die with their
/// isolate instead of dangling in a thread_local.
pub(crate) struct InstanceTemplateCache(
    pub RefCell<ahash::AHashMap<String, v8::Global<v8::FunctionTemplate>>>,
);

impl InstanceTemplateCache {
    pub(crate) fn new() -> Self {
        Self(RefCell::new(ahash::AHashMap::new()))
    }
}

/// Receiver lookup across both V8 callback argument types: property
/// interceptors expose no `this()` in these bindings, only `holder()`.
pub(crate) trait CallbackThisObject<'s> {
    fn this_object(&self) -> v8::Local<'s, v8::Object>;
}

impl<'s> CallbackThisObject<'s> for v8::FunctionCallbackArguments<'s> {
    #[inline]
    fn this_object(&self) -> v8::Local<'s, v8::Object> {
        self.this()
    }
}

impl<'s> CallbackThisObject<'s> for v8::PropertyCallbackArguments<'s> {
    #[inline]
    fn this_object(&self) -> v8::Local<'s, v8::Object> {
        self.holder()
    }
}

/// COM instance from a wrapper object's internal field 0. `None` when `this`
/// is not a WinRT wrapper, letting callers fall back to the declaration baked
/// into the callback data.
#[inline]
pub(crate) fn this_instance(
    scope: &mut v8::PinScope<'_, '_>,
    this: v8::Local<v8::Object>,
) -> Option<IUnknown> {
    let ptr = this_declaration_ffi(scope, this)?;
    unsafe { (*ptr).instance.clone() }
}

/// The per-instance `DeclarationFFI` stored on a wrapper object. The pointee
/// is leaked at instance creation, so it outlives the wrapper.
#[inline]
pub(crate) fn this_declaration_ffi(
    scope: &mut v8::PinScope<'_, '_>,
    this: v8::Local<v8::Object>,
) -> Option<*mut DeclarationFFI> {
    if this.internal_field_count() < 1 {
        return None;
    }
    let field = this.get_internal_field(scope, 0)?;
    let ext = unsafe { field.cast::<v8::External>() };
    let ptr = ext.value() as *mut DeclarationFFI;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

pub(crate) fn handle_ns_func(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
}

pub(crate) fn handle_indexed_property_setter(
    _scope: &mut v8::PinScope<'_, '_>,
    _index: u32,
    _value: v8::Local<v8::Value>,
    _args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    v8::Intercepted::kNo
}

pub(crate) fn handle_indexed_property_getter(
    _scope: &mut v8::PinScope<'_, '_>,
    _index: u32,
    _args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    v8::Intercepted::kNo
}

pub(crate) fn wire_winrt_event(
    scope: &mut v8::PinScope<'_, '_>,
    name: &str,
    instance: Option<IUnknown>,
    add_method: &MethodDeclaration,
    remove_method: &MethodDeclaration,
    value: Local<v8::Value>,
) -> v8::Intercepted {
    let identity = instance.as_ref().and_then(crate::com_identity);

    if let Some(id) = identity {
        let old = crate::EVENT_REGISTRY.with(|r| {
            r.borrow_mut()
                .get_mut(&id)
                .and_then(|events| events.remove(name))
        });
        if let Some(old) = old {
            if let Some(inst) = instance.clone() {
                let mut mc =
                    MethodCall::new(remove_method, remove_method.is_sealed(), inst, false);
                let _ = mc.call_with_event_token(old.token);
            }
        }
    }

    // Assigning null/undefined just unsubscribes. Bail before the handle probe:
    // ToObject(null) would throw a TypeError into the assigning script.
    if value.is_null_or_undefined() {
        return v8::Intercepted::kYes;
    }

    let Some(inst) = instance else {
        // No COM instance to attach to (e.g. a ctor object without a factory):
        // the unsubscribe bookkeeping above is all that can be done.
        return v8::Intercepted::kYes;
    };

    let handle_ptr: Option<*mut c_void> = value.to_object(scope).and_then(|obj| {
        let key = v8::String::new(scope, "handle")?;
        let handle_val = obj.get(scope, key.into())?;
        v8::Local::<v8::External>::try_from(handle_val)
            .ok()
            .map(|ext| ext.value())
    });
    let effective_ptr: Option<*mut c_void> = handle_ptr.or_else(|| {
        let func = v8::Local::<v8::Function>::try_from(value).ok()?;
        let (guid, param_types) = delegate_info_from_add_method(add_method)?;
        let data = Box::new(JsDelegateData {
            js_func: v8::Global::new(scope, func),
            param_types,
        });
        let delegate = Box::new(JsDelegate {
            vtable: &JS_DELEGATE_VTBL as *const _,
            ref_count: std::sync::atomic::AtomicU32::new(1),
            guid,
            data: Box::into_raw(data),
        });
        Some(Box::into_raw(delegate) as *mut c_void)
    });

    if let Some(delegate_ptr) = effective_ptr {
        let mut mc = MethodCall::new(add_method, add_method.is_sealed(), inst, false);
        let (ret, token) = mc.call_with_raw_ptr(delegate_ptr);
        if ret.is_err() {
            let detail = format!(
                "Event add '{}' failed: {} (0x{:08X})",
                name,
                ret.message(),
                ret.0 as u32
            );
            let message = v8::String::new(scope, &detail).unwrap();
            let error = v8::Exception::error(scope, message);
            scope.throw_exception(error);
            return v8::Intercepted::kYes;
        }
        if let Some(id) = identity {
            crate::EVENT_REGISTRY.with(|r| {
                r.borrow_mut().entry(id).or_default().insert(
                    name.to_string(),
                    crate::EventRegistration {
                        token,
                        handler: v8::Global::new(scope, value),
                    },
                );
            });
        }
    }
    v8::Intercepted::kYes
}

pub(crate) fn read_winrt_event<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    instance: Option<&IUnknown>,
    name: &str,
) -> Local<'a, v8::Value> {
    if let Some(id) = instance.and_then(crate::com_identity) {
        let handler = crate::EVENT_REGISTRY.with(|r| {
            r.borrow()
                .get(&id)
                .and_then(|events| events.get(name))
                .map(|e| e.handler.clone())
        });
        if let Some(global) = handler {
            return v8::Local::new(scope, &global);
        }
    }
    v8::null(scope).into()
}

pub(crate) fn handle_named_property_query(
    _scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<v8::Name>,
    _args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    rv.set_int32(0);
    v8::Intercepted::kNo
}

/// Attempts to wrap a raw COM pointer (a WinRT reference-type return value) as a full typed proxy by
/// resolving its concrete type via `GetRuntimeClassName`. Returns `None` (without releasing the ref) when
/// the pointer is null or its type can't be resolved, so the caller can fall back to a plain wrapper.
///
/// SAFETY: `value` must be a valid COM (`IUnknown`-derived) pointer. WinRT marshals array/struct returns
/// separately (out-params / value buffers), so a reference-type method return reaching here is always COM.
pub(crate) fn try_wrap_inspectable_pointer<'a>(
    value: *mut c_void,
    scope: &mut v8::PinScope<'a, '_>,
) -> Option<v8::Local<'a, v8::Value>> {
    if value.is_null() {
        return None;
    }
    let instance = unsafe { IUnknown::from_raw(value) };
    let resolved = instance
        .cast::<IInspectable>()
        .ok()
        .and_then(|insp| insp.GetRuntimeClassName().ok())
        .map(|cn| cn.to_string())
        .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
        .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct));
    match resolved {
        Some((cname, decl)) => {
            let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                cname.as_str(),
                None,
                None,
                decl,
                Some(instance),
                scope,
            )
            .into();
            Some(ret)
        }
        None => {
            // Not resolvable; keep the ref alive so the caller's raw-pointer wrapper stays valid.
            let _ = std::mem::ManuallyDrop::new(instance);
            None
        }
    }
}

/// Sets a method/property return value, resolving `Object`/IInspectable returns to a full typed
/// proxy via GetRuntimeClassName so property/event interceptors work (e.g. the WebView2 returned by
/// `XamlReader.Load`). Any other signature defers to `set_ret_val`. A non-resolvable Object falls
/// back to the generic pointer wrapper without releasing the COM ref.
pub(crate) fn set_ret_val_resolving_object(
    result: *mut c_void,
    return_sig: &str,
    scope: &mut v8::PinScope<'_, '_>,
    mut retval: v8::ReturnValue,
) {
    if return_sig == "Object" && !result.is_null() {
        let instance = unsafe { IUnknown::from_raw(result) };
        let resolved = instance
            .cast::<IInspectable>()
            .ok()
            .and_then(|insp| insp.GetRuntimeClassName().ok())
            .map(|cn| cn.to_string())
            .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
            .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct));
        match resolved {
            Some((cname, decl)) => {
                let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                    cname.as_str(),
                    None,
                    None,
                    decl,
                    Some(instance),
                    scope,
                )
                .into();
                retval.set(ret.into());
                return;
            }
            None => {
                // Keep the ref alive; fall through to the generic pointer wrapper.
                let _ = std::mem::ManuallyDrop::new(instance);
            }
        }
    }
    if let Ok(return_type) = NativeType::try_from(return_sig) {
        unsafe { set_ret_val(result, scope, retval, return_type) };
    }
}

pub(crate) fn handle_named_property_getter(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let this = args.holder();
    let dec = this.get_internal_field(scope, 0).unwrap();
    let dec = unsafe { dec.cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();

    let store_field = match this.get_internal_field(scope, 1) {
        Some(f) => f,
        None => return v8::Intercepted::kNo,
    };
    let store = unsafe { store_field.cast::<v8::Map>() };
    let kind = lock.kind();
    if key.is_string() {
        if let Some(cache) = store.get(scope, key.into()) {
            if !cache.is_null_or_undefined() {
                rv.set(cache);
                return v8::Intercepted::kYes;
            }
        }

        let name = key.to_string(scope).unwrap().to_rust_string_lossy(scope);

        // Expose raw IUnknown pointer so the dotnet bridge can marshal WinRT objects
        // via Marshal.GetObjectForIUnknown. Only meaningful on instance proxies.
        if name == "__native_ptr" {
            let ptr = dec
                .instance
                .as_ref()
                .map(|unk| unsafe { unk.as_raw() } as u64)
                .unwrap_or(0);
            rv.set(v8::BigInt::new_from_u64(scope, ptr).into());
            return v8::Intercepted::kYes;
        }

        match kind {
            DeclarationKind::Namespace => {
                let parent = dec.inner.clone();
                let dec = lock.as_any().downcast_ref::<NamespaceDeclaration>();
                if let Some(dec) = dec {
                    let full_name = format!("{}.{}", dec.full_name(), name.as_str());

                    if let Some(dec) = MetadataReader::find_by_name_or_generic(full_name.as_str()) {
                        let declaration = Arc::clone(&dec);
                        let lock = dec.read();

                        match lock.kind() {
                            DeclarationKind::Struct => {
                                let struct_dec =
                                    lock.as_any().downcast_ref::<StructDeclaration>().unwrap();
                                let name = struct_dec.name().to_string();
                                drop(lock);

                                let ret = create_ns_struct_ctor_object(
                                    name.as_str(),
                                    Arc::clone(&dec),
                                    scope,
                                );
                                let ret: Local<v8::Value> = ret.into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            DeclarationKind::Class => {
                                let ret: Local<v8::Value> = create_ns_ctor_object(
                                    lock.name(),
                                    Some(parent),
                                    declaration,
                                    scope,
                                )
                                .into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            DeclarationKind::Interface
                            | DeclarationKind::GenericInterface
                            | DeclarationKind::GenericInterfaceInstance
                            | DeclarationKind::Delegate
                            | DeclarationKind::GenericDelegate
                            | DeclarationKind::GenericDelegateInstance
                            | DeclarationKind::Event => {
                                let ret: Local<v8::Value> = create_ns_ctor_object(
                                    lock.name(),
                                    Some(parent),
                                    declaration,
                                    scope,
                                )
                                .into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                            _ => {
                                let ret: Local<v8::Value> =
                                    create_ns_object(name.as_str(), declaration, scope).into();
                                store.set(scope, key.into(), ret);
                                rv.set(ret);
                            }
                        }
                        return v8::Intercepted::kYes;
                    }

                    return v8::Intercepted::kNo;
                }
            }
            DeclarationKind::Class => {
                let clazz_dec = lock.as_any().downcast_ref::<ClassDeclaration>();

                if let Some(clazz_dec) = clazz_dec {
                    if let Some(method) = find_class_method(clazz_dec, &name) {
                        crate::class_helpers::register_overload_siblings(clazz_dec, &name, &method);
                        {
                            let declaration = Arc::new(RwLock::new(method));
                            let declaration =
                                Box::into_raw(Box::new(DeclarationFFI::new_with_instance(
                                    declaration,
                                    dec.instance.clone(),
                                )));
                            let ext = v8::External::new(scope, declaration as _);

                            let builder = v8::Function::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method =
                                        lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                                    let instance = dec.instance.clone().unwrap();
                                    let alt = crate::class_helpers::overload_for_argc(method, args.length() as usize);
                                    let method = alt.as_ref().unwrap_or(method);
                                    let mut method = MethodCall::new(
                                        method,
                                        method.is_sealed(),
                                        instance,
                                        false,
                                    );
                                    let (ret, result, _outs) = method.call(scope, &args);

                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                        return;
                                    }

                                    if method.is_void() {
                                        retval.set_undefined();
                                        return;
                                    }

                                    let return_value_opt: Option<Local<v8::Value>> = match method
                                        .return_kind()
                                    {
                                        ReturnKind::Void => None,
                                        ReturnKind::Guid => {
                                            let obj = unsafe {
                                                crate::guid_ptr_to_js_object(result, scope)
                                            };
                                            Some(obj.into())
                                        }
                                        ReturnKind::Struct(declaration) => Some(
                                            crate::create_struct_object_from_raw(
                                                declaration.clone(),
                                                result,
                                                scope,
                                            )
                                            .into(),
                                        ),
                                        ReturnKind::Object { decl, type_name }
                                        | ReturnKind::InterfaceObject { decl, type_name } => {
                                            if result.is_null() {
                                                Some(v8::null(scope).into())
                                            } else {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                Some(
                                                    create_ns_ctor_instance_object(
                                                        type_name.as_ref(),
                                                        None,
                                                        dec.parent.clone(),
                                                        decl.clone(),
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into(),
                                                )
                                            }
                                        }
                                        ReturnKind::DynamicObject => {
                                            if result.is_null() {
                                                None
                                            } else {
                                                let instance =
                                                    unsafe { IUnknown::from_raw(result) };
                                                let resolved = instance
                                                    .cast::<IInspectable>()
                                                    .ok()
                                                    .and_then(|insp| {
                                                        insp.GetRuntimeClassName().ok()
                                                    })
                                                    .map(|cn| cn.to_string())
                                                    .and_then(|n| {
                                                        MetadataReader::find_by_name(&n)
                                                            .map(|d| (n, d))
                                                    })
                                                    .filter(|(_, d)| {
                                                        !matches!(
                                                            d.read().kind(),
                                                            DeclarationKind::Struct
                                                        )
                                                    });
                                                match resolved {
                                                    Some((name, decl)) => Some(
                                                        create_ns_ctor_instance_object(
                                                            name.as_str(),
                                                            None,
                                                            dec.parent.clone(),
                                                            decl,
                                                            Some(instance),
                                                            scope,
                                                        )
                                                        .into(),
                                                    ),
                                                    None => {
                                                        let _ =
                                                            std::mem::ManuallyDrop::new(instance);
                                                        None
                                                    }
                                                }
                                            }
                                        }
                                        ReturnKind::Primitive(nt) => {
                                            let v = unsafe {
                                                read_value_from_ptr(
                                                    result as *const c_void,
                                                    scope,
                                                    nt.clone(),
                                                )
                                            };
                                            Some(v)
                                        }
                                    };

                                    if !_outs.is_empty() {
                                        let mut arr_len = _outs.len();
                                        if return_value_opt.is_some() {
                                            arr_len += 1;
                                        }
                                        let arr = v8::Array::new(scope, arr_len as i32);
                                        let mut idx = 0u32;
                                        if let Some(rv) = return_value_opt {
                                            arr.set_index(scope, idx, rv);
                                            idx += 1;
                                        }
                                        for outv in _outs.into_iter() {
                                            arr.set_index(scope, idx, outv);
                                            idx += 1;
                                        }
                                        retval.set(arr.into());
                                        return;
                                    }

                                    if let Some(rv) = return_value_opt {
                                        retval.set(rv);
                                    }
                                },
                            )
                            .data(ext.into())
                            .build(scope);

                            let func = builder.unwrap();
                            let func: Local<v8::Value> = func.into();
                            store.set(scope, key.into(), func);
                            rv.set(func);
                            return v8::Intercepted::kYes;
                        }
                    }

                    if dec.instance.is_some() && find_event_methods(clazz_dec, &name).is_some() {
                        let handler = read_winrt_event(scope, dec.instance.as_ref(), &name);
                        rv.set(handler);
                        return v8::Intercepted::kYes;
                    }
                }
            }
            DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance
            | DeclarationKind::Delegate
            | DeclarationKind::GenericDelegate
            | DeclarationKind::GenericDelegateInstance
            | DeclarationKind::Event => {
                if let Some(impl_key) = v8::String::new(scope, "__implementation__") {
                    if let Some(implementation) = store.get(scope, impl_key.into()) {
                        if let Some(implementation) = implementation.to_object(scope) {
                            if let Some(value) = implementation.get(scope, key.into()) {
                                if !value.is_null_or_undefined() {
                                    store.set(scope, key.into(), value);
                                    rv.set(value);
                                    return v8::Intercepted::kYes;
                                }
                            }
                        }
                    }
                }

                if dec.instance.is_some() && find_interface_event_methods(&*lock, &name).is_some()
                {
                    let handler = read_winrt_event(scope, dec.instance.as_ref(), &name);
                    rv.set(handler);
                    return v8::Intercepted::kYes;
                }
            }
            DeclarationKind::Enum => {
                let enum_dec = lock.as_any().downcast_ref::<EnumDeclaration>();
                if let Some(enum_dec) = enum_dec {
                    if let Some(value) = enum_dec.enum_for_name(name.as_str()) {
                        match value.value() {
                            Value::Int32(value) => {
                                rv.set_int32(value);
                                let ret: Local<v8::Value> = v8::Integer::new(scope, value).into();
                                store.set(scope, key.into(), ret);
                                return v8::Intercepted::kYes;
                            }
                            Value::Uint32(value) => {
                                rv.set_uint32(value);
                                let ret: Local<v8::Value> =
                                    v8::Integer::new_from_unsigned(scope, value).into();
                                store.set(scope, key.into(), ret);
                                return v8::Intercepted::kYes;
                            }
                            _ => {}
                        }
                    }
                    return v8::Intercepted::kNo;
                }
            }
            _ => {}
        }
    }
    v8::Intercepted::kNo
}

pub(crate) fn handle_named_property_setter(
    scope: &mut v8::PinScope<'_, '_>,
    key: Local<v8::Name>,
    value: Local<v8::Value>,
    args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    let this = args.holder();
    let Some(dec_field) = this.get_internal_field(scope, 0) else {
        return v8::Intercepted::kNo;
    };
    let dec = unsafe { dec_field.cast::<v8::External>() }.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();
    let kind = lock.kind();

    let Some(store_field) = this.get_internal_field(scope, 1) else {
        return v8::Intercepted::kNo;
    };
    let store = unsafe { store_field.cast::<v8::Map>() };

    let name = key.to_rust_string_lossy(scope);

    let is_reserved = match kind {
        DeclarationKind::Namespace => lock
            .as_any()
            .downcast_ref::<NamespaceDeclaration>()
            .map(|d| d.children().contains(&name))
            .unwrap_or(false),
        DeclarationKind::Enum => lock
            .as_any()
            .downcast_ref::<EnumDeclaration>()
            .map(|d| d.enum_for_name(&name).is_some())
            .unwrap_or(false),
        DeclarationKind::Class => lock
            .as_any()
            .downcast_ref::<ClassDeclaration>()
            .map(|d| class_has_member_named(d, &name))
            .unwrap_or(false),
        DeclarationKind::Interface => lock
            .as_any()
            .downcast_ref::<InterfaceDeclaration>()
            .map(|d| {
                d.methods().iter().any(|m| m.name() == name)
                    || d.properties().iter().any(|p| p.name() == name)
            })
            .unwrap_or(false),
        DeclarationKind::Struct => lock
            .as_any()
            .downcast_ref::<StructDeclaration>()
            .map(|d| d.fields().iter().any(|f| f.name() == name))
            .unwrap_or(false),
        _ => false,
    };

    // Wire WinRT event handlers via add/remove ABI methods for class instances.
    if kind == DeclarationKind::Class && !is_reserved {
        if let Some(class) = lock.as_any().downcast_ref::<ClassDeclaration>() {
            if let Some((add_method, remove_method)) = find_event_methods(class, &name) {
                let instance = dec.instance.clone();
                drop(lock);
                return wire_winrt_event(
                    scope,
                    &name,
                    instance,
                    &add_method,
                    &remove_method,
                    value,
                );
            }
        }
    }

    // Interface-typed wrappers: wire events declared on the interface itself.
    if !is_reserved
        && matches!(
            kind,
            DeclarationKind::Interface | DeclarationKind::GenericInterfaceInstance
        )
    {
        if let Some((add_method, remove_method)) = find_interface_event_methods(&*lock, &name) {
            let instance = dec.instance.clone();
            drop(lock);
            return wire_winrt_event(scope, &name, instance, &add_method, &remove_method, value);
        }
    }

    if !is_reserved {
        store.set(scope, key.into(), value);
        v8::Intercepted::kYes
    } else {
        v8::Intercepted::kNo
    }
}

fn instance_method_dispatch(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let dec = unsafe { args.data().cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();
    let method_decl = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
    let instance = match this_instance(scope, args.this()) {
        Some(i) => i,
        None => match dec.instance.clone() {
            Some(i) => i,
            None => {
                let msg = v8::String::new(scope, "NativeScript: method invoked with no native instance").unwrap();
                let err = v8::Exception::error(scope, msg);
                scope.throw_exception(err);
                return;
            }
        },
    };
    let alt = crate::class_helpers::overload_for_argc(method_decl, args.length() as usize);
    let method_decl = alt.as_ref().unwrap_or(method_decl);
    let mut method = MethodCall::new(method_decl, method_decl.is_sealed(), instance, false);
    let (ret, result, _outs) = method.call(scope, &args);

    if ret.is_err() {
        let detail = crate::error::format_hresult_message(ret);
        let msg = v8::String::new(scope, &detail).unwrap();
        let err = v8::Exception::error(scope, msg);
        scope.throw_exception(err);
        return;
    }

    if method.is_void() {
        retval.set_undefined();
        return;
    }

    let return_value_opt: Option<Local<v8::Value>> = match method.return_kind() {
        ReturnKind::Void => None,
        ReturnKind::Guid => {
            let obj = unsafe { crate::guid_ptr_to_js_object(result, scope) };
            Some(obj.into())
        }
        ReturnKind::Struct(declaration) => {
            Some(crate::create_struct_object_from_raw(declaration.clone(), result, scope).into())
        }
        ReturnKind::Object { decl, type_name }
        | ReturnKind::InterfaceObject { decl, type_name } => {
            if result.is_null() {
                Some(v8::null(scope).into())
            } else {
                let instance = unsafe { IUnknown::from_raw(result) };
                let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                    type_name.as_ref(),
                    None,
                    dec.parent.clone(),
                    decl.clone(),
                    Some(instance),
                    scope,
                )
                .into();
                Some(ret)
            }
        }
        ReturnKind::DynamicObject => {
            if result.is_null() {
                None
            } else {
                let instance = unsafe { IUnknown::from_raw(result) };
                let resolved = instance
                    .cast::<IInspectable>()
                    .ok()
                    .and_then(|insp| insp.GetRuntimeClassName().ok())
                    .map(|cn| cn.to_string())
                    .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
                    .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct));
                match resolved {
                    Some((name, decl)) => {
                        let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                            name.as_str(),
                            None,
                            dec.parent.clone(),
                            decl,
                            Some(instance),
                            scope,
                        )
                        .into();
                        Some(ret)
                    }
                    None => {
                        let _ = std::mem::ManuallyDrop::new(instance);
                        None
                    }
                }
            }
        }
        ReturnKind::Primitive(nt) => {
            let v = unsafe { read_value_from_ptr(result as *const c_void, scope, nt.clone()) };
            Some(v)
        }
    };

    if !_outs.is_empty() {
        let mut arr_len = _outs.len();
        if return_value_opt.is_some() {
            arr_len += 1;
        }
        let arr = v8::Array::new(scope, arr_len as i32);
        let mut idx = 0u32;
        if let Some(rv) = return_value_opt {
            arr.set_index(scope, idx, rv);
            idx += 1;
        }
        for outv in _outs.into_iter() {
            arr.set_index(scope, idx, outv);
            idx += 1;
        }
        retval.set(arr.into());
        return;
    }

    if let Some(rv) = return_value_opt {
        retval.set(rv);
    }
}

/// Tries to find a property by looking up the instance's WinRT runtime class name in the
/// sideloaded WinMDs rather than the static (projected) type. Called when the static type
/// lookup misses — e.g. `Microsoft.UI.Xaml.Application` doesn't expose `MainWindow`, but
/// a sideloaded `App.winmd` describes the concrete `nativescriptwindowspokedex.App` class
/// which does. Returns `None` when the runtime class equals the static type (already
/// searched), when no sideloaded WinMD describes it, or when the property isn't there.
fn find_runtime_class_property(
    instance: &impl Interface,
    prop_name: &str,
    static_type_name: &str,
) -> Option<PropertyDeclaration> {
    let runtime_name = instance
        .cast::<IInspectable>()
        .ok()?
        .GetRuntimeClassName()
        .ok()
        .map(|s| s.to_string())?;

    if runtime_name.is_empty() || runtime_name == static_type_name {
        return None;
    }

    let decl = MetadataReader::find_by_name(&runtime_name)?;
    let lock = decl.read();
    let runtime_clazz = lock.as_any().downcast_ref::<ClassDeclaration>()?;
    find_class_property(runtime_clazz, prop_name)
}

/// Sends opcode 0x0B to the dotnet bridge and converts the binary response to a V8 value.
/// Returns None when the bridge signals "property not found" (0xFF error response) so the
/// caller can fall through to `v8::Intercepted::kNo`.
fn try_clr_property_get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    instance_raw: *mut std::ffi::c_void,
    prop_name: &str,
) -> Option<Local<'s, v8::Value>> {
    let ptr_i64 = instance_raw as i64;
    if ptr_i64 == 0 {
        return None;
    }
    let name_bytes = prop_name.as_bytes();
    // Request: [0x0B][ptr i64 LE][name-len u16 LE][name UTF-8]
    let mut req = Vec::with_capacity(11 + name_bytes.len());
    req.push(0x0Bu8);
    req.extend_from_slice(&ptr_i64.to_le_bytes());
    req.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    req.extend_from_slice(name_bytes);

    let resp = crate::dotnet::call_dotnet_binary(&req).ok()?;
    parse_clr_bin_response(scope, &resp)
}

/// Parses the binary response from opcode 0x0B into a V8 value.
/// Returns None for error (0xFF) or unrecognised tags so the caller returns kNo.
/// Returns Some(v8::null) for a legitimate null property value (0x00).
fn parse_clr_bin_response<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    resp: &[u8],
) -> Option<Local<'s, v8::Value>> {
    let tag = *resp.first()?;
    match tag {
        0x00 => Some(v8::null(scope).into()),
        0x01 => Some(v8::Boolean::new(scope, false).into()),
        0x02 => Some(v8::Boolean::new(scope, true).into()),
        0x03 => {
            if resp.len() < 5 { return None; }
            let v = i32::from_le_bytes(resp[1..5].try_into().ok()?);
            Some(v8::Integer::new(scope, v).into())
        }
        0x04 => {
            if resp.len() < 9 { return None; }
            let v = f64::from_le_bytes(resp[1..9].try_into().ok()?);
            Some(v8::Number::new(scope, v).into())
        }
        0x05 => {
            if resp.len() < 5 { return None; }
            let slen = u32::from_le_bytes(resp[1..5].try_into().ok()?) as usize;
            if resp.len() < 5 + slen { return None; }
            let s = std::str::from_utf8(&resp[5..5 + slen]).ok()?;
            Some(v8::String::new(scope, s)?.into())
        }
        // 0x06 Handle: [i32 handle][u16 type-name-len][type-name][u8 has_ptr][i64 ptr?]
        // Box() always calls ObtainNativePtr for WinRT objects, so the native ptr is present.
        // Use it directly to create a JS proxy; the handle keeps the managed object alive.
        0x06 => {
            if resp.len() < 7 { return None; }
            let name_len = u16::from_le_bytes([resp[5], resp[6]]) as usize;
            let after = 7 + name_len;
            if resp.len() <= after { return None; }
            if resp[after] == 1 && resp.len() >= after + 9 {
                let ptr = i64::from_le_bytes(resp[after + 1..after + 9].try_into().ok()?);
                if ptr != 0 {
                    return try_wrap_inspectable_pointer(
                        ptr as *mut std::ffi::c_void,
                        scope,
                    );
                }
            }
            None
        }
        0xFF => None, // error / property not found
        _ => None,
    }
}

pub(crate) fn handle_instance_property_getter(
    scope: &mut v8::PinScope<'_, '_>,
    key: Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    if !key.is_string() {
        return v8::Intercepted::kNo;
    }

    let name = key.to_rust_string_lossy(scope);
    if name == "__probe__" {
        let value = v8::String::new(scope, "instance-handler-active").unwrap();
        rv.set(value.into());
        return v8::Intercepted::kYes;
    }

    // Prefer the per-instance DeclarationFFI on the holder (internal field 0):
    // with the shared class template the interceptor data only carries the
    // declaration from the first-built instance.
    let dec = match this_declaration_ffi(scope, args.holder()) {
        Some(p) => p,
        None => {
            let d = unsafe { args.data().cast::<v8::External>() };
            d.value() as *mut DeclarationFFI
        }
    };
    let dec = unsafe { &*dec };
    let lock = dec.read();

    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
        if find_interface_event_methods(&*lock, &name).is_some() {
            let handler = read_winrt_event(scope, dec.instance.as_ref(), &name);
            rv.set(handler);
            return v8::Intercepted::kYes;
        }
        return v8::Intercepted::kNo;
    };

    // Primary lookup: static WinRT type from bundled WinMD.
    // Fallback: sideloaded WinMD keyed by the instance's IInspectable runtime class name.
    // Both paths produce a PropertyDeclaration so the single invocation block below handles both.
    let found_property = find_class_property(clazz, &name).or_else(|| {
        dec.instance
            .as_ref()
            .and_then(|inst| find_runtime_class_property(inst, &name, clazz.full_name()))
    });

    if let Some(property) = found_property {
        // Static properties (e.g. UIElement.PointerPressedEvent) must be called via
        // the declaring class's activation factory, not the instance pointer.
        let instance_for_call = if property.is_static() {
            let declaring = find_static_property_declaring_class(clazz, &name);
            match declaring
                .as_deref()
                .and_then(|n| crate::class_activation_factory(n).ok())
            {
                Some(factory) => factory,
                None => return v8::Intercepted::kNo,
            }
        } else {
            match dec.instance.clone() {
                Some(inst) => inst,
                None => return v8::Intercepted::kNo,
            }
        };
        let property_call_opt = PropertyCall::new(&property, false, instance_for_call, false);
        let Some(mut property_call) = property_call_opt else {
            return v8::Intercepted::kNo;
        };

        let (ret, result, _outs) = property_call.call_with_values(scope, &[]);

        if ret.is_err() {
            let detail = format!(
                "Property get '{}' failed: {} (0x{:08X})",
                name,
                ret.message(),
                ret.0 as u32
            );
            let message = v8::String::new(scope, &detail).unwrap();
            let error = v8::Exception::error(scope, message);
            scope.throw_exception(error);
            return v8::Intercepted::kYes;
        }

        if property_call.is_void() {
            rv.set_undefined();
            return v8::Intercepted::kYes;
        }

        let ret_val: Option<Local<v8::Value>> = match property_call.return_kind() {
            ReturnKind::Void => None,
            ReturnKind::Guid => {
                let obj = unsafe { crate::guid_ptr_to_js_object(result, scope) };
                Some(obj.into())
            }
            ReturnKind::Struct(declaration) => Some(
                crate::create_struct_object_from_raw(declaration.clone(), result, scope).into(),
            ),
            ReturnKind::Object { decl, type_name } => {
                if result.is_null() {
                    Some(v8::null(scope).into())
                } else {
                    let instance = unsafe { IUnknown::from_raw(result) };
                    Some(
                        create_ns_ctor_instance_object(
                            type_name.as_ref(),
                            None,
                            None,
                            decl.clone(),
                            Some(instance),
                            scope,
                        )
                        .into(),
                    )
                }
            }
            ReturnKind::InterfaceObject { decl, type_name } => {
                if result.is_null() {
                    Some(v8::null(scope).into())
                } else {
                    let instance = unsafe { IUnknown::from_raw(result) };
                    let (resolved_name, resolved_decl) = instance
                        .cast::<IInspectable>()
                        .ok()
                        .and_then(|insp| insp.GetRuntimeClassName().ok())
                        .map(|cn| cn.to_string())
                        .and_then(|n| MetadataReader::find_by_name(&n).map(|d| (n, d)))
                        .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct))
                        .unwrap_or_else(|| (type_name.to_string(), decl.clone()));
                    Some(
                        create_ns_ctor_instance_object(
                            resolved_name.as_str(),
                            None,
                            None,
                            resolved_decl,
                            Some(instance),
                            scope,
                        )
                        .into(),
                    )
                }
            }
            ReturnKind::DynamicObject => try_wrap_inspectable_pointer(result, scope),
            ReturnKind::Primitive(nt) => {
                let v = unsafe {
                    read_value_from_ptr(result as *const std::ffi::c_void, scope, nt.clone())
                };
                Some(v)
            }
        };

        if let Some(v) = ret_val {
            rv.set(v);
            return v8::Intercepted::kYes;
        }

        return v8::Intercepted::kNo;
    }

    if let Some(method) = find_class_method(clazz, &name) {

        crate::class_helpers::register_overload_siblings(clazz, &name, &method);
        let key = format!("{}::{}", clazz.full_name(), name);
        if let Some(func) =
            SHARED_METHOD_FNS.with(|c| c.borrow().get(&key).map(|g| v8::Local::new(scope, g)))
        {
            rv.set(func.into());
            return v8::Intercepted::kYes;
        }

        let method_dec = Arc::new(RwLock::new(method.clone()));
        let method_ffi = DeclarationFFI::new_with_instance(method_dec, None);
        let method_ffi = Box::into_raw(Box::new(method_ffi));
        let ext = v8::External::new(scope, method_ffi as _);

        let function = v8::Function::builder(instance_method_dispatch)
            .data(ext.into())
            .build(scope)
            .unwrap();

        SHARED_METHOD_FNS.with(|c| {
            c.borrow_mut().insert(key, v8::Global::new(scope, function));
        });

        rv.set(function.into());
        return v8::Intercepted::kYes;
    }

    if find_event_methods(clazz, &name).is_some() {
        let handler = read_winrt_event(scope, dec.instance.as_ref(), &name);
        rv.set(handler);
        return v8::Intercepted::kYes;
    }

    // CLR reflection fallback: the property isn't in WinRT metadata but may exist as a
    // CLR-only member on a managed subclass (e.g. App.MainWindow on a class that derives
    // from Microsoft.UI.Xaml.Application). Ask the dotnet bridge to reflect on the actual
    // runtime type via opcode 0x0B. Only attempted when an instance pointer is available.
    if let Some(instance) = &dec.instance {
        let raw_ptr = unsafe { instance.as_raw() };
        if let Some(val) = try_clr_property_get(scope, raw_ptr, &name) {
            rv.set(val);
            return v8::Intercepted::kYes;
        }
    }

    v8::Intercepted::kNo
}

/// Named property setter for WinRT instance objects.
/// Handles WinRT property writes (setter ABI call).
/// Data External points to a `DeclarationFFI` wrapping the ClassDeclaration.
pub(crate) fn handle_instance_property_setter(
    scope: &mut v8::PinScope<'_, '_>,
    key: Local<v8::Name>,
    value: Local<v8::Value>,
    args: v8::PropertyCallbackArguments,
    mut _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    if !key.is_string() {
        return v8::Intercepted::kNo;
    }

    let name = key.to_rust_string_lossy(scope);
    // Prefer the per-instance DeclarationFFI on the holder (internal field 0);
    // the interceptor data on a shared class template belongs to the
    // first-built instance.
    let dec = match this_declaration_ffi(scope, args.holder()) {
        Some(p) => p,
        None => {
            let d = unsafe { args.data().cast::<v8::External>() };
            d.value() as *mut DeclarationFFI
        }
    };
    let dec = unsafe { &mut *dec };
    let lock = dec.read();

    // For interface declarations, handle property setters directly using the
    // parameterized IID from the declaration and type-arg substitution.
    {
        use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
        let kind = lock.kind();
        let iface_property = match kind {
            DeclarationKind::Interface => {
                if let Some(iface) = lock.as_any().downcast_ref::<InterfaceDeclaration>() {
                    iface
                        .properties()
                        .iter()
                        .find(|p| p.name() == name)
                        .cloned()
                        .map(|property| (iface.id(), Vec::<String>::new(), property))
                } else {
                    None
                }
            }
            DeclarationKind::GenericInterfaceInstance => {
                if let Some(iface) = lock
                    .as_any()
                    .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                {
                    let full = iface.full_name();
                    let type_args = if let Some(open) = full.find('<') {
                        let inner = &full[open + 1..full.len() - 1];
                        inner.split(',').map(|s| s.trim().to_string()).collect()
                    } else {
                        Vec::new()
                    };
                    iface
                        .properties()
                        .iter()
                        .find(|p| p.name() == name)
                        .cloned()
                        .map(|property| (iface.id(), type_args, property))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((iid, type_args, property)) = iface_property {
            if property.setter().is_none() {
                return v8::Intercepted::kNo;
            }
            let Some(ref instance) = dec.instance else {
                return v8::Intercepted::kNo;
            };
            let Some(mut property_call) = PropertyCall::new_for_interface(
                &property,
                true,
                instance.clone(),
                false,
                iid,
                type_args,
            ) else {
                return v8::Intercepted::kNo;
            };
            let (ret, _, _outs) = property_call.call_with_values(scope, &[value]);
            if ret.is_err() {
                let detail = format!(
                    "Property set '{}' failed: {} (0x{:08X})",
                    name,
                    ret.message(),
                    ret.0 as u32
                );
                let message = v8::String::new(scope, &detail).unwrap();
                let error = v8::Exception::error(scope, message);
                scope.throw_exception(error);
            }
            return v8::Intercepted::kYes;
        }
    }

    let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
        if let Some((add_method, remove_method)) = find_interface_event_methods(&*lock, &name) {
            let instance = dec.instance.clone();
            drop(lock);
            return wire_winrt_event(scope, &name, instance, &add_method, &remove_method, value);
        }
        return v8::Intercepted::kNo;
    };

    // Try WinRT properties first.
    if let Some(property) = find_class_property(clazz, &name) {
        if property.setter().is_none() {
            return v8::Intercepted::kNo;
        }

        let Some(mut property_call) =
            PropertyCall::new(&property, true, dec.instance.clone().unwrap(), false)
        else {
            return v8::Intercepted::kNo;
        };
        let (ret, _, _outs) = property_call.call_with_values(scope, &[value]);
        if ret.is_err() {
            let detail = format!(
                "Property set '{}' failed: {} (0x{:08X})",
                name,
                ret.message(),
                ret.0 as u32
            );
            let message = v8::String::new(scope, &detail).unwrap();
            let error = v8::Exception::error(scope, message);
            scope.throw_exception(error);
        }
        return v8::Intercepted::kYes;
    }

    // Try WinRT events: `instance.EventName = new DelegateType({ Invoke: fn })`.
    if let Some((add_method, remove_method)) = find_event_methods(clazz, &name) {
        let instance = dec.instance.clone();
        drop(lock);
        return wire_winrt_event(scope, &name, instance, &add_method, &remove_method, value);
    }

    // Not a class property or event: try mapping bracket-assignment
    // (`obj['name'] = value`) to WinRT map semantics by calling
    // `IPropertySet::Insert` when the instance implements it.
    if let Some(ref instance) = dec.instance {
        if let Ok(ps) = instance.clone().cast::<IPropertySet>() {
            let key_h = HSTRING::from(name.as_str());
            if let Some(inspectable) = v8_value_to_inspectable(scope, value, dec) {
                match ps.Insert(&key_h, &inspectable) {
                    Ok(_) => return v8::Intercepted::kYes,
                    Err(e) => {
                        let code = e.code();
                        let detail = format!(
                            "IPropertySet.Insert failed: {}",
                            crate::error::format_hresult_message(code)
                        );
                        let v8_msg = v8::String::new(scope, &detail).unwrap();
                        let err = v8::Exception::error(scope, v8_msg);
                        scope.throw_exception(err);
                        return v8::Intercepted::kYes;
                    }
                }
            }
        }
    }

    // Default fallback: store the value on the instance backing map so
    // consumers can still attach arbitrary JS properties to WinRT objects.
    let this = args.holder();
    let store_field = match this.get_internal_field(scope, 1) {
        Some(f) => f,
        None => return v8::Intercepted::kNo,
    };
    let store = unsafe { store_field.cast::<v8::Map>() };
    store.set(scope, key.into(), value);
    return v8::Intercepted::kYes;

    v8::Intercepted::kNo
}

fn v8_value_to_inspectable(
    scope: &mut v8::PinScope<'_, '_>,
    val: Local<v8::Value>,
    _dec: &DeclarationFFI,
) -> Option<IInspectable> {
    if let Ok(sv) = v8::Local::<v8::String>::try_from(val) {
        let s = sv.to_rust_string_lossy(scope);
        let h = HSTRING::from(s);
        if let Ok(pv) = PropertyValue::CreateString(&h) {
            if let Ok(ins) = pv.cast::<IInspectable>() {
                return Some(ins);
            }
        }
        return None;
    }

    if let Ok(nv) = v8::Local::<v8::Number>::try_from(val) {
        let f = nv.value();
        if f == f.trunc() && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
            if let Ok(pv) = PropertyValue::CreateInt32(f as i32) {
                if let Ok(ins) = pv.cast::<IInspectable>() {
                    return Some(ins);
                }
            }
        } else {
            if let Ok(pv) = PropertyValue::CreateDouble(f) {
                if let Ok(ins) = pv.cast::<IInspectable>() {
                    return Some(ins);
                }
            }
        }
        return None;
    }

    if let Ok(bv) = v8::Local::<v8::Boolean>::try_from(val) {
        if let Ok(pv) = PropertyValue::CreateBoolean(bv.is_true()) {
            if let Ok(ins) = pv.cast::<IInspectable>() {
                return Some(ins);
            }
        }
        return None;
    }

    if val.is_object() {
        if let Some(obj) = val.to_object(scope) {
            if let Some(dec_field) = obj.get_internal_field(scope, 0) {
                let dec_ptr =
                    unsafe { dec_field.cast::<v8::External>() }.value() as *mut DeclarationFFI;
                if !dec_ptr.is_null() {
                    let wrapper = unsafe { &*dec_ptr };
                    if let Some(inst) = &wrapper.instance {
                        if let Ok(ins) = inst.clone().cast::<IInspectable>() {
                            return Some(ins);
                        }
                    }
                }
            }

            // External handle `handle` property pattern used for delegates and wrappers.
            if let Some(handle_key) = v8::String::new(scope, "handle") {
                if let Some(handle_val) = obj.get(scope, handle_key.into()) {
                    if let Ok(ext) = v8::Local::<v8::External>::try_from(handle_val) {
                        let ptr = ext.value();
                        if !ptr.is_null() {
                            // Construct an IUnknown from the raw pointer and cast to IInspectable.
                            let raw = ptr as *mut std::ffi::c_void;
                            let unknown = unsafe { IUnknown::from_raw(raw) };
                            if let Ok(ins) = unknown.cast::<IInspectable>() {
                                return Some(ins);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

pub(crate) unsafe fn guid_ptr_to_js_object<'a>(
    ptr: *mut c_void,
    scope: &mut v8::PinScope<'a, '_>,
) -> v8::Local<'a, v8::Object> {
    use windows::core::GUID;
    let g = &*(ptr as *const GUID);

    let guid_str = format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
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
        g.data4[7]
    );

    let obj = v8::Object::new(scope);

    let key_data1 = v8::String::new(scope, "data1").unwrap();
    let val_data1 = v8::Integer::new_from_unsigned(scope, g.data1);
    obj.set(scope, key_data1.into(), val_data1.into());

    let key_data2 = v8::String::new(scope, "data2").unwrap();
    let val_data2 = v8::Integer::new_from_unsigned(scope, g.data2 as u32);
    obj.set(scope, key_data2.into(), val_data2.into());

    let key_data3 = v8::String::new(scope, "data3").unwrap();
    let val_data3 = v8::Integer::new_from_unsigned(scope, g.data3 as u32);
    obj.set(scope, key_data3.into(), val_data3.into());

    let arr = v8::Array::new(scope, 8);
    for (i, &byte) in g.data4.iter().enumerate() {
        let byte_val = v8::Integer::new_from_unsigned(scope, byte as u32);
        arr.set_index(scope, i as u32, byte_val.into());
    }
    let key_data4 = v8::String::new(scope, "data4").unwrap();
    obj.set(scope, key_data4.into(), arr.into());

    let guid_v8 = v8::String::new(scope, &guid_str).unwrap();
    let to_string_fn = v8::FunctionTemplate::builder(
        |_scope: &mut v8::PinScope<'_, '_>,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let s = unsafe { args.data().cast::<v8::String>() };
            retval.set(s.into());
        },
    )
    .data(guid_v8.into())
    .build(scope)
    .get_function(scope)
    .unwrap();

    let key_to_string = v8::String::new(scope, "toString").unwrap();
    obj.set(scope, key_to_string.into(), to_string_fn.into());
    let key_value_of = v8::String::new(scope, "valueOf").unwrap();
    obj.set(scope, key_value_of.into(), to_string_fn.into());

    obj
}

pub(crate) unsafe fn raw_result_to_local<'s>(
    result: *mut c_void,
    signature: &str,
    parent_decl: Option<Arc<RwLock<dyn Declaration>>>,
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<Local<'s, v8::Value>> {
    let raw = result as usize;
    match signature {
        "Void" => None,
        "Guid" => Some(unsafe { guid_ptr_to_js_object(result, scope) }.into()),
        _ if !signature.contains('.') => {
            let native_type = NativeType::try_from(signature).ok()?;
            let v: Local<v8::Value> = match native_type {
                NativeType::Void => return None,
                NativeType::Bool => v8::Boolean::new(scope, (raw as u8) != 0).into(),
                NativeType::U8 => v8::Number::new(scope, raw as u8 as f64).into(),
                NativeType::I8 => v8::Number::new(scope, (raw as u8 as i8) as f64).into(),
                NativeType::U16 => v8::Number::new(scope, raw as u16 as f64).into(),
                NativeType::I16 => v8::Number::new(scope, (raw as u16 as i16) as f64).into(),
                NativeType::U32 => v8::Number::new(scope, raw as u32 as f64).into(),
                NativeType::I32 => v8::Number::new(scope, (raw as u32 as i32) as f64).into(),
                NativeType::U64 => {
                    let v = raw as u64;
                    if v > MAX_SAFE_INTEGER as u64 {
                        v8::BigInt::new_from_u64(scope, v).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::I64 => {
                    let v = raw as u64 as i64;
                    if v > MAX_SAFE_INTEGER as i64 || v < MIN_SAFE_INTEGER as i64 {
                        v8::BigInt::new_from_i64(scope, v).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::USize => {
                    if raw > MAX_SAFE_INTEGER as usize {
                        v8::BigInt::new_from_u64(scope, raw as u64).into()
                    } else {
                        v8::Number::new(scope, raw as f64).into()
                    }
                }
                NativeType::ISize => {
                    let v = raw as isize;
                    if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&v) {
                        v8::BigInt::new_from_i64(scope, v as i64).into()
                    } else {
                        v8::Number::new(scope, v as f64).into()
                    }
                }
                NativeType::F32 => v8::Number::new(scope, f32::from_bits(raw as u32) as f64).into(),
                NativeType::F64 => v8::Number::new(scope, f64::from_bits(raw as u64)).into(),
                NativeType::String => {
                    let hstring: HSTRING = std::mem::transmute(result);
                    let s = hstring.to_string();
                    v8::String::new(scope, s.as_str())?.into()
                }
                NativeType::Pointer => {
                    if result.is_null() {
                        return None;
                    }
                    let unknown = IUnknown::from_raw(result);
                    if let Ok(inspectable) = unknown.cast::<IInspectable>() {
                        if let Ok(class_name) = inspectable.GetRuntimeClassName() {
                            let name_str = class_name.to_string();
                            if let Some(decl) = MetadataReader::find_by_name(&name_str) {
                                let instance = unknown.clone();
                                return Some(
                                    create_ns_ctor_instance_object(
                                        &name_str,
                                        None,
                                        parent_decl,
                                        decl,
                                        Some(instance),
                                        scope,
                                    )
                                    .into(),
                                );
                            }
                        }
                    }
                    v8::External::new(scope, result).into()
                }
                _ => return None,
            };
            Some(v)
        }
        _ => {
            if result.is_null() {
                return None;
            }
            let com_instance = IUnknown::from_raw(result);
            let decl = MetadataReader::find_by_name(signature)?;
            Some(
                create_ns_ctor_instance_object(
                    signature,
                    None,
                    parent_decl,
                    decl,
                    Some(com_instance),
                    scope,
                )
                .into(),
            )
        }
    }
}

pub(crate) fn create_ns_object<'a>(
    name: &str,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let name = v8::String::new(scope, name).unwrap();
    let tmpl = FunctionTemplate::new(scope, handle_ns_func);
    tmpl.set_class_name(name);
    let object_tmpl = tmpl.instance_template(scope);
    object_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .query(handle_named_property_query)
            .getter(handle_named_property_getter)
            .setter(handle_named_property_setter),
    );
    object_tmpl.set_internal_field_count(2);

    let object = object_tmpl.new_instance(scope).unwrap();
    let declaration_box = Box::new(DeclarationFFI::new(declaration));
    let declaration_ptr = Box::into_raw(declaration_box);
    let ext = v8::External::new(scope, declaration_ptr as _);
    object.set_internal_field(0, ext.into());
    // Borrow a temporary reference to the leaked Box via the raw pointer so
    // we can inspect the declaration before handing ownership to V8.
    let declaration_ref: &DeclarationFFI = unsafe { &*declaration_ptr };

    let object_store = v8::Map::new(scope);
    object.set_internal_field(1, object_store.into());

    // If this proxy represents an interface (including generic instances),
    // pre-populate the instance backing store with method functions so
    // callers can access methods (e.g. IVector.Append) directly on the
    // proxied object without needing a separate implementation object.
    {
        let decl_lock = declaration_ref.read();
        match decl_lock.kind() {
            DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance => {
                // Treat the declaration as a BaseClassDeclarationImpl to access methods.
                let methods: Vec<MethodDeclaration> = {
                    // Downcast to the appropriate concrete type and collect methods.
                    if let DeclarationKind::Interface = decl_lock.kind() {
                        let iface = decl_lock
                            .as_any()
                            .downcast_ref::<InterfaceDeclaration>()
                            .unwrap();
                        iface.methods().to_vec()
                    } else if let DeclarationKind::GenericInterface = decl_lock.kind() {
                        let g = decl_lock
                            .as_any()
                            .downcast_ref::<GenericInterfaceDeclaration>()
                            .unwrap();
                        g.methods().to_vec()
                    } else {
                        let gi = decl_lock
                            .as_any()
                            .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                            .unwrap();
                        gi.methods().to_vec()
                    }
                };

                drop(decl_lock);

                // Install each method as a bound function on the instance store.
                for method in methods.iter() {
                    let method_name = if method.overload_name().is_empty() {
                        method.name().to_string()
                    } else {
                        method.overload_name().to_string()
                    };

                    if let Some(k) = v8::String::new(scope, method_name.as_str()) {
                        // Skip if already present.
                        if let Some(existing) = object_store.get(scope, k.into()) {
                            if !existing.is_null_or_undefined() {
                                continue;
                            }
                        }

                        let declaration_ffi = DeclarationFFI::new_with_instance(
                            Arc::new(RwLock::new(method.clone())),
                            None,
                        );
                        let declaration_ffi = Box::into_raw(Box::new(declaration_ffi));
                        let ext = v8::External::new(scope, declaration_ffi as _);

                        let builder = v8::Function::builder(instance_method_dispatch)
                            .data(ext.into())
                            .build(scope)
                            .unwrap();

                        let func: Local<v8::Value> = builder.into();
                        object_store.set(scope, k.into(), func);
                    }
                }
            }
            _ => {}
        }
    }

    object.into()
}

pub(crate) fn create_ns_ctor_instance_object<'a>(
    name: &str,
    factory: Option<IUnknown>,
    parent: Option<Arc<RwLock<dyn Declaration>>>,
    declaration: Arc<RwLock<dyn Declaration>>,
    instance: Option<IUnknown>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let identity_key: Option<usize> = instance
        .as_ref()
        .and_then(|unk| unk.cast::<IUnknown>().ok().map(|id| id.as_raw() as usize));
    if let Some(key) = identity_key {
        let hit = crate::INSTANCE_CACHE.with(|cache| {
            cache
                .borrow()
                .get(&key)
                .and_then(|weak| weak.to_local(scope))
        });
        if let Some(local) = hit {
            return local.into();
        }
    }

    let resolved_concrete: Option<(String, Arc<RwLock<dyn Declaration>>)> = if name.contains('<') {
        None
    } else {
        instance
            .as_ref()
            .and_then(|unk| unk.cast::<IInspectable>().ok())
            .and_then(|insp| insp.GetRuntimeClassName().ok())
            .map(|cn| cn.to_string())
            .filter(|cn| !cn.is_empty() && cn != name && !cn.contains('<'))
            .and_then(|cn| MetadataReader::find_by_name(&cn).map(|d| (cn, d)))
            .filter(|(_, d)| !matches!(d.read().kind(), DeclarationKind::Struct))
    };
    let (name, declaration): (&str, Arc<RwLock<dyn Declaration>>) = match &resolved_concrete {
        Some((cn, d)) => (cn.as_str(), d.clone()),
        None => (name, declaration),
    };

    // Interface wrappers derive their members from `parent`, so its identity
    // must be part of the key. The "N|" prefix keeps these templates separate
    // from the parallel lib.rs builder's — the two interceptor sets differ.
    let template_key: String = match &parent {
        Some(p) => format!("N|{}|{}", name, p.read().full_name()),
        None => format!("N|{}", name),
    };
    let cached_tmpl: Option<v8::Global<v8::FunctionTemplate>> = scope
        .get_slot::<InstanceTemplateCache>()
        .and_then(|c| c.0.borrow().get(template_key.as_str()).cloned());
    if let Some(tmpl_global) = cached_tmpl {
        let tmpl = v8::Local::new(scope, &tmpl_global);
        return finish_instance_object(tmpl, declaration, instance, identity_key, scope);
    }

    let class_name = v8::String::new(scope, name).unwrap();

    let tmpl = FunctionTemplate::new(scope, handle_ns_func);
    let object_tmpl = tmpl.instance_template(scope);

    object_tmpl.set_internal_field_count(2);

    let declaration_ffi = Box::into_raw(Box::new(DeclarationFFI::new_with_instance(
        declaration.clone(),
        instance.clone(),
    )));
    let ext = v8::External::new(scope, declaration_ffi as _);

    // Use named top-level functions instead of inline closures (named accessor pattern).
    object_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .getter(handle_instance_property_getter)
            .setter(handle_instance_property_setter)
            .data(ext.into()),
    );

    tmpl.set_class_name(class_name);

    let proto = tmpl.prototype_template(scope);

    {
        let lock = declaration.read();
        let kind = lock.kind();

        match kind {
            DeclarationKind::Class => {
                let clazz = lock.as_any().downcast_ref::<ClassDeclaration>().unwrap();
                let class_methods = collect_class_methods(clazz);
                let class_properties_with_declaring =
                    collect_class_properties_with_declaring(clazz);
                let mut seen_member_names: HashSet<String> = HashSet::new();

                let to_string_func = FunctionTemplate::builder(
                    |_scope: &mut v8::PinScope<'_, '_>,
                     args: v8::FunctionCallbackArguments,
                     mut retval: v8::ReturnValue| {
                        retval.set(args.data());
                    },
                )
                .data(class_name.into())
                .build(scope);

                let to_string = v8::String::new(scope, "toString").unwrap();
                proto.set(to_string.into(), to_string_func.into());

                for method in class_methods.iter() {
                    let method_name = if method.overload_name().is_empty() {
                        method.name().to_string()
                    } else {
                        method.overload_name().to_string()
                    };
                    let is_static = method.is_static();
                    let key = format!(
                        "{}:{}",
                        method_name,
                        if is_static { "static" } else { "instance" }
                    );
                    if !seen_member_names.insert(key) {
                        continue;
                    }

                    let name = v8::String::new(scope, method_name.as_str());
                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );
                    let declaration = Box::into_raw(Box::new(declaration));
                    let ext = v8::External::new(scope, declaration as _);

                    extern "C" fn callback(callback: *const v8::FunctionCallbackInfo) {
                        let info = unsafe { &*callback };
                        v8::callback_scope!(unsafe scope, info);
                        let args = unsafe {
                            v8::FunctionCallbackArguments::from_function_callback_info(info)
                        };
                        let mut retval = v8::ReturnValue::from_function_callback_info(info);

                        let dec = unsafe { args.data().cast::<v8::External>() };
                        let dec = dec.value() as *mut DeclarationFFI;
                        let dec = unsafe { &*dec };
                        let lock = dec.read();
                        let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                        let Some(__ns_inst) =
                            this_instance(scope, args.this()).or_else(|| dec.instance.clone())
                        else {
                            return;
                        };
                        let mut method =
                            MethodCall::new(method, method.is_sealed(), __ns_inst, false);
                        let (ret, result, _outs) = method.call(scope, &args);

                        if ret.is_err() {
                            let detail = crate::error::format_hresult_message(ret);
                            let msg = v8::String::new(scope, &detail).unwrap();
                            let err = v8::Exception::error(scope, msg.into());
                            scope.throw_exception(err);
                            return;
                        } else if !method.is_void() {
                            let ret_v: Option<Local<v8::Value>> = match method.return_kind() {
                                ReturnKind::Void => None,
                                ReturnKind::Guid => {
                                    let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                                    Some(obj.into())
                                }
                                ReturnKind::Struct(declaration) => Some(
                                    crate::create_struct_object_from_raw(
                                        declaration.clone(),
                                        result,
                                        scope,
                                    )
                                    .into(),
                                ),
                                ReturnKind::Object { decl, type_name }
                                | ReturnKind::InterfaceObject { decl, type_name } => {
                                    if result.is_null() {
                                        Some(v8::null(scope).into())
                                    } else {
                                        let instance = unsafe { IUnknown::from_raw(result) };
                                        Some(
                                            create_ns_ctor_instance_object(
                                                type_name.as_ref(),
                                                None,
                                                dec.parent.clone(),
                                                decl.clone(),
                                                Some(instance),
                                                scope,
                                            )
                                            .into(),
                                        )
                                    }
                                }
                                ReturnKind::DynamicObject => {
                                    try_wrap_inspectable_pointer(result, scope)
                                }
                                ReturnKind::Primitive(nt) => {
                                    let v = unsafe {
                                        read_value_from_ptr(
                                            result as *const std::ffi::c_void,
                                            scope,
                                            nt.clone(),
                                        )
                                    };
                                    Some(v)
                                }
                            };
                            if let Some(v) = ret_v {
                                retval.set(v.into());
                            }
                        } else {
                            retval.set_undefined();
                        }
                    }

                    let func = FunctionTemplate::builder_raw(callback)
                        .data(ext.into())
                        .build(scope);

                    if is_static {
                        tmpl.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        // Instance-template properties are copied onto every object
                        // at new_instance; prototype members are shared.
                        proto.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    }
                }

                for (property, declaring_class_name) in class_properties_with_declaring.iter() {
                    let property_name = property.name().to_string();
                    let is_static = property.is_static();
                    let key = format!(
                        "{}:{}",
                        property_name,
                        if is_static { "static" } else { "instance" }
                    );
                    if !seen_member_names.insert(key) {
                        continue;
                    }

                    let name = v8::String::new(scope, property_name.as_str());
                    let (effective_instance, static_factory_cls) = if is_static {
                        if declaring_class_name.as_str() == clazz.full_name() {
                            (factory.clone(), None)
                        } else {
                            (None, Some(declaring_class_name.clone()))
                        }
                    } else {
                        (instance.clone(), None)
                    };
                    let mut declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(property.clone())),
                        effective_instance,
                    );
                    declaration.static_factory_class = static_factory_cls;

                    let getter_declaration = declaration.clone();
                    let getter_declaration = Box::into_raw(Box::new(getter_declaration));
                    let getter_declaration_ext = v8::External::new(scope, getter_declaration as _);

                    let getter = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };
                            let dec = dec.value() as *mut DeclarationFFI;
                            let dec = unsafe { &*dec };
                            let lock = dec.read();
                            let method =
                                lock.as_any().downcast_ref::<PropertyDeclaration>().unwrap();
                            // Instance properties resolve their COM target from `this`
                            // (shared class template); statics fall back to the factory.
                            let factory = match this_instance(scope, args.this()) {
                                Some(inst) => inst,
                                None => match resolve_class_factory_from_parent(dec) {
                                    Ok(f) => f,
                                    Err(e) => {
                                        throw_js_error(
                                            scope,
                                            &format!(
                                                "Failed to resolve property factory: {}",
                                                e.message()
                                            ),
                                        );
                                        return;
                                    }
                                },
                            };
                            let Some(mut method) = PropertyCall::new(method, false, factory, false)
                            else {
                                return;
                            };
                            let (ret, result, _outs) = method.call(scope, &args);
                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            } else if !method.is_void() {
                                let ret_v: Option<Local<v8::Value>> = match method.return_kind() {
                                    ReturnKind::Void => None,
                                    ReturnKind::Guid => {
                                        let obj = unsafe { guid_ptr_to_js_object(result, scope) };
                                        Some(obj.into())
                                    }
                                    ReturnKind::Struct(declaration) => Some(
                                        crate::create_struct_object_from_raw(
                                            declaration.clone(),
                                            result,
                                            scope,
                                        )
                                        .into(),
                                    ),
                                    ReturnKind::Object { decl, type_name }
                                    | ReturnKind::InterfaceObject { decl, type_name } => {
                                        if result.is_null() {
                                            Some(v8::null(scope).into())
                                        } else {
                                            let instance = unsafe { IUnknown::from_raw(result) };
                                            Some(
                                                create_ns_ctor_instance_object(
                                                    type_name.as_ref(),
                                                    None,
                                                    None,
                                                    decl.clone(),
                                                    Some(instance),
                                                    scope,
                                                )
                                                .into(),
                                            )
                                        }
                                    }
                                    ReturnKind::DynamicObject => {
                                        try_wrap_inspectable_pointer(result, scope)
                                    }
                                    ReturnKind::Primitive(nt) => {
                                        let v = unsafe {
                                            read_value_from_ptr(
                                                result as *const std::ffi::c_void,
                                                scope,
                                                nt.clone(),
                                            )
                                        };
                                        Some(v)
                                    }
                                };
                                if let Some(v) = ret_v {
                                    retval.set(v.into());
                                }
                            } else {
                                retval.set_undefined();
                            }
                        },
                    )
                    .data(getter_declaration_ext.into())
                    .build(scope);

                    let mut setter: Option<Local<FunctionTemplate>> = None;
                    if property.setter().is_some() {
                        let setter_declaration = declaration;
                        let setter_declaration = Box::into_raw(Box::new(setter_declaration));
                        let setter_declaration_ext =
                            v8::External::new(scope, setter_declaration as _);
                        setter = Some(FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                 args: v8::FunctionCallbackArguments,
                                                                 _retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };
                            let dec = dec.value() as *mut DeclarationFFI;
                            let dec = unsafe { &*dec };
                            let lock = dec.read();
                            let prop = lock.as_any().downcast_ref::<PropertyDeclaration>().unwrap();
                            // Instance setters resolve their COM target from `this`
                            // (shared class template); statics fall back to the factory.
                            let factory = match this_instance(scope, args.this()) {
                                Some(inst) => inst,
                                None => match resolve_class_factory_from_parent(dec) {
                                    Ok(f) => f,
                                    Err(e) => {
                                        throw_js_error(scope, &format!("Failed to resolve property factory for setter: {}", e.message()));
                                        return;
                                    }
                                },
                            };
                            let Some(mut method) = PropertyCall::new(prop, true, factory, false) else { return; };
                            let (ret, _, _outs) = method.call(scope, &args);
                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg);
                                scope.throw_exception(err);
                            }
                        })
                        .data(setter_declaration_ext.into())
                        .build(scope));
                    }

                    if property.is_static() {
                        let name = name.unwrap();
                        tmpl.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        let name = name.unwrap();
                        // Prototype, not instance template — see methods loop above.
                        proto.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::NONE,
                        );
                    }
                }
            }
            DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance => {
                let clazz: &dyn BaseClassDeclarationImpl = match kind {
                    DeclarationKind::Interface => lock
                        .as_any()
                        .downcast_ref::<InterfaceDeclaration>()
                        .unwrap(),
                    DeclarationKind::GenericInterface => lock
                        .as_any()
                        .downcast_ref::<GenericInterfaceDeclaration>()
                        .unwrap(),
                    DeclarationKind::GenericInterfaceInstance => lock
                        .as_any()
                        .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                        .unwrap(),
                    _ => unreachable!(),
                };

                let to_string_func = FunctionTemplate::builder(
                    |_scope: &mut v8::PinScope<'_, '_>,
                     args: v8::FunctionCallbackArguments,
                     mut retval: v8::ReturnValue| {
                        retval.set(args.data());
                    },
                )
                .data(class_name.into())
                .build(scope);

                let to_string = v8::String::new(scope, "toString").unwrap();
                proto.set(to_string.into(), to_string_func.into());

                if let Some(clazz) = parent {
                    let clazz = clazz.read();
                    let kind = clazz.kind();

                    match kind {
                        DeclarationKind::Class => {
                            let clazz = clazz.as_any().downcast_ref::<ClassDeclaration>().unwrap();

                            for method in clazz.methods().iter() {
                                let name = v8::String::new(scope, method.name());
                                let is_static = method.is_static();
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(method.clone())),
                                    if is_static {
                                        factory.clone()
                                    } else {
                                        instance.clone()
                                    },
                                );
                                let declaration = Box::into_raw(Box::new(declaration));
                                let ext = v8::External::new(scope, declaration as _);

                                let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(method, method.is_sealed(), __ns_inst, false);
                                    let (ret, result, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    } else if !method.is_void() {
                                        set_ret_val_resolving_object(result, method.return_type(), scope, retval);
                                    } else {
                                        retval.set_undefined();
                                    }
                                })
                                .data(ext.into())
                                .build(scope);

                                if is_static {
                                    tmpl.set(name.unwrap().into(), func.into());
                                } else {
                                    proto.set(name.unwrap().into(), func.into());
                                }
                            }

                            for property in clazz.properties().iter() {
                                let name = v8::String::new(scope, property.name());
                                let is_static = property.is_static();
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(property.clone())),
                                    if is_static {
                                        factory.clone()
                                    } else {
                                        instance.clone()
                                    },
                                );

                                let getter_declaration = declaration.clone();
                                let getter_declaration =
                                    Box::into_raw(Box::new(getter_declaration));
                                let getter_declaration_ext =
                                    v8::External::new(scope, getter_declaration as _);

                                let getter = FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                        args: v8::FunctionCallbackArguments,
                                                                        mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method = lock.as_any().downcast_ref::<PropertyDeclaration>().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(method.getter(), false, __ns_inst, false);
                                    let (ret, result, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    } else if !method.is_void() {
                                        set_ret_val_resolving_object(result, method.return_type(), scope, retval);
                                    } else {
                                        retval.set_undefined();
                                    }
                                })
                                .data(getter_declaration_ext.into())
                                .build(scope);

                                let mut setter: Option<Local<FunctionTemplate>> = None;
                                if property.setter().is_some() {
                                    let setter_declaration = declaration;
                                    let setter_declaration =
                                        Box::into_raw(Box::new(setter_declaration));
                                    let setter_declaration_ext =
                                        v8::External::new(scope, setter_declaration as _);
                                    setter = Some(FunctionTemplate::builder(|_scope: &mut v8::PinScope<'_, '_>,
                                                                             _args: v8::FunctionCallbackArguments,
                                                                             _retval: v8::ReturnValue| {})
                                        .data(setter_declaration_ext.into())
                                        .build(scope));
                                }

                                if property.is_static() {
                                    let name = name.unwrap();
                                    tmpl.set_accessor_property(
                                        name.into(),
                                        Some(getter),
                                        setter,
                                        v8::PropertyAttribute::DONT_DELETE,
                                    );
                                } else {
                                    let name = name.unwrap();
                                    proto.set_accessor_property(
                                        name.into(),
                                        Some(getter),
                                        setter,
                                        v8::PropertyAttribute::READ_ONLY
                                            | v8::PropertyAttribute::DONT_DELETE,
                                    );
                                }
                            }
                        }
                        DeclarationKind::Interface
                        | DeclarationKind::GenericInterface
                        | DeclarationKind::GenericInterfaceInstance => {
                            let iface_kind = kind;
                            let clazz: &dyn BaseClassDeclarationImpl = match iface_kind {
                                DeclarationKind::Interface => clazz
                                    .as_any()
                                    .downcast_ref::<InterfaceDeclaration>()
                                    .unwrap(),
                                DeclarationKind::GenericInterface => clazz
                                    .as_any()
                                    .downcast_ref::<GenericInterfaceDeclaration>()
                                    .unwrap(),
                                DeclarationKind::GenericInterfaceInstance => clazz
                                    .as_any()
                                    .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                                    .unwrap(),
                                _ => unreachable!(),
                            };

                            for method in clazz.methods().iter() {
                                let name = v8::String::new(scope, method.name());
                                let is_static = method.is_static();
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(method.clone())),
                                    if is_static {
                                        factory.clone()
                                    } else {
                                        instance.clone()
                                    },
                                );
                                let declaration = Box::into_raw(Box::new(declaration));
                                let ext = v8::External::new(scope, declaration as _);

                                let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(method, method.is_sealed(), __ns_inst, false);
                                    let (ret, result, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    } else if !method.is_void() {
                                        set_ret_val_resolving_object(result, method.return_type(), scope, retval);
                                    } else {
                                        retval.set_undefined();
                                    }
                                })
                                .data(ext.into())
                                .build(scope);

                                if is_static {
                                    tmpl.set(name.unwrap().into(), func.into());
                                } else {
                                    proto.set(name.unwrap().into(), func.into());
                                }
                            }

                            for property in clazz.properties().iter() {
                                let name = v8::String::new(scope, property.name());
                                let is_static = property.is_static();
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(property.clone())),
                                    if is_static {
                                        factory.clone()
                                    } else {
                                        instance.clone()
                                    },
                                );

                                let getter_declaration = declaration.clone();
                                let getter_declaration =
                                    Box::into_raw(Box::new(getter_declaration));
                                let getter_declaration_ext =
                                    v8::External::new(scope, getter_declaration as _);

                                let getter = FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                        args: v8::FunctionCallbackArguments,
                                                                        mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method = lock.as_any().downcast_ref::<PropertyDeclaration>().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this()).or_else(|| dec.instance.clone()) else { return; };
                                    let Some(mut method) = PropertyCall::new(method, false, __ns_inst, false) else {
                                        return;
                                    };
                                    let (ret, result, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    } else if !method.is_void() {
                                        set_ret_val_resolving_object(result, method.return_type(), scope, retval);
                                    } else {
                                        retval.set_undefined();
                                    }
                                })
                                .data(getter_declaration_ext.into())
                                .build(scope);

                                let mut setter: Option<Local<FunctionTemplate>> = None;
                                if property.setter().is_some() {
                                    let setter_declaration = declaration;
                                    let setter_declaration =
                                        Box::into_raw(Box::new(setter_declaration));
                                    let setter_declaration_ext =
                                        v8::External::new(scope, setter_declaration as _);
                                    setter = Some(FunctionTemplate::builder(|_scope: &mut v8::PinScope<'_, '_>,
                                                                             _args: v8::FunctionCallbackArguments,
                                                                             _retval: v8::ReturnValue| {})
                                        .data(setter_declaration_ext.into())
                                        .build(scope));
                                }

                                if property.is_static() {
                                    let name = name.unwrap();
                                    tmpl.set_accessor_property(
                                        name.into(),
                                        Some(getter),
                                        setter,
                                        v8::PropertyAttribute::DONT_DELETE,
                                    );
                                } else {
                                    let name = name.unwrap();
                                    proto.set_accessor_property(
                                        name.into(),
                                        Some(getter),
                                        setter,
                                        v8::PropertyAttribute::READ_ONLY
                                            | v8::PropertyAttribute::DONT_DELETE,
                                    );
                                }
                            }
                        }
                        DeclarationKind::Delegate
                        | DeclarationKind::GenericDelegate
                        | DeclarationKind::GenericDelegateInstance => {
                            let method = match kind {
                                DeclarationKind::Delegate => clazz
                                    .as_any()
                                    .downcast_ref::<DelegateDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                DeclarationKind::GenericDelegate => clazz
                                    .as_any()
                                    .downcast_ref::<GenericDelegateDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                DeclarationKind::GenericDelegateInstance => clazz
                                    .as_any()
                                    .downcast_ref::<GenericDelegateInstanceDeclaration>()
                                    .map(|delegate| delegate.invoke_method().clone()),
                                _ => None,
                            };

                            if let Some(method) = method {
                                let name = v8::String::new(scope, method.name());
                                let declaration = DeclarationFFI::new_with_instance(
                                    Arc::new(RwLock::new(method)),
                                    instance.clone(),
                                );
                                let declaration = Box::into_raw(Box::new(declaration));
                                let ext = v8::External::new(scope, declaration as _);

                                let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                                          args: v8::FunctionCallbackArguments,
                                                                          mut retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this()).or_else(|| dec.instance.clone()) else { return; };
                                    let mut method = MethodCall::new(method, method.is_sealed(), __ns_inst, false);
                                    let (ret, result, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg.into());
                                        scope.throw_exception(err);
                                        return;
                                    } else if !method.is_void() {
                                        if let Ok(return_type) = NativeType::try_from(method.return_type()) {
                                            unsafe { set_ret_val(result, scope, retval, return_type); }
                                        }
                                    } else {
                                        retval.set_undefined();
                                    }
                                })
                                .data(ext.into())
                                .build(scope);

                                if let Some(name) = name {
                                    proto.set(name.into(), func.into());
                                }
                            }
                        }
                        _ => {}
                    }
                }

                for method in clazz.methods().iter() {
                    let name = v8::String::new(scope, method.name());
                    let is_static = method.is_static();
                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );
                    let declaration = Box::into_raw(Box::new(declaration));
                    let ext = v8::External::new(scope, declaration as _);

                    let func = v8::FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };
                            let dec = dec.value() as *mut DeclarationFFI;
                            let dec = unsafe { &*dec };
                            let lock = dec.read();
                            let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                            let Some(__ns_inst) =
                                this_instance(scope, args.this()).or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let mut method =
                                MethodCall::new(method, method.is_sealed(), __ns_inst, false);
                            let (ret, result, _outs) = method.call(scope, &args);
                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            } else if !method.is_void() {
                                match NativeType::try_from(method.return_type()) {
                                    Ok(return_type) => unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    },
                                    Err(_) => {}
                                }
                            } else {
                                retval.set_undefined();
                            }
                        },
                    )
                    .data(ext.into())
                    .build(scope);

                    if is_static {
                        tmpl.set(name.unwrap().into(), func.into());
                    } else {
                        proto.set(name.unwrap().into(), func.into());
                    }
                }

                for property in clazz.properties().iter() {
                    let name = v8::String::new(scope, property.name());
                    let is_static = property.is_static();
                    let declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(property.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );

                    let getter_declaration = declaration.clone();
                    let getter_declaration = Box::into_raw(Box::new(getter_declaration));
                    let getter_declaration_ext = v8::External::new(scope, getter_declaration as _);

                    let getter = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let dec = unsafe { args.data().cast::<v8::External>() };
                            let dec = dec.value() as *mut DeclarationFFI;
                            let dec = unsafe { &*dec };
                            let lock = dec.read();
                            let method =
                                lock.as_any().downcast_ref::<PropertyDeclaration>().unwrap();
                            let Some(__ns_inst) =
                                this_instance(scope, args.this()).or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let Some(mut method) =
                                PropertyCall::new(method, false, __ns_inst, false)
                            else {
                                return;
                            };
                            let (ret, result, _outs) = method.call(scope, &args);
                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            } else if !method.is_void() {
                                match NativeType::try_from(method.return_type()) {
                                    Ok(return_type) => unsafe {
                                        set_ret_val(result, scope, retval, return_type);
                                    },
                                    Err(_) => {}
                                }
                            } else {
                                retval.set_undefined();
                            }
                        },
                    )
                    .data(getter_declaration_ext.into())
                    .build(scope);

                    let mut setter: Option<Local<FunctionTemplate>> = None;
                    if property.setter().is_some() {
                        let setter_declaration = declaration;
                        let setter_declaration = Box::into_raw(Box::new(setter_declaration));
                        let setter_declaration_ext =
                            v8::External::new(scope, setter_declaration as _);
                        setter = Some(
                            FunctionTemplate::builder(
                                |scope: &mut v8::PinScope<'_, '_>,
                                 args: v8::FunctionCallbackArguments,
                                 _retval: v8::ReturnValue| {
                                    let dec = unsafe { args.data().cast::<v8::External>() };
                                    let dec = dec.value() as *mut DeclarationFFI;
                                    let dec = unsafe { &*dec };
                                    let lock = dec.read();
                                    let prop = lock
                                        .as_any()
                                        .downcast_ref::<PropertyDeclaration>()
                                        .unwrap();
                                    let setter = prop.setter().unwrap();
                                    let Some(__ns_inst) = this_instance(scope, args.this())
                                        .or_else(|| dec.instance.clone())
                                    else {
                                        return;
                                    };
                                    let mut method =
                                        MethodCall::new(setter, false, __ns_inst, false);
                                    let (ret, _, _outs) = method.call(scope, &args);
                                    if ret.is_err() {
                                        let detail = crate::error::format_hresult_message(ret);
                                        let msg = v8::String::new(scope, &detail).unwrap();
                                        let err = v8::Exception::error(scope, msg);
                                        scope.throw_exception(err);
                                    }
                                },
                            )
                            .data(setter_declaration_ext.into())
                            .build(scope),
                        );
                    }

                    if property.is_static() {
                        let name = name.unwrap();
                        tmpl.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        let name = name.unwrap();
                        proto.set_accessor_property(
                            name.into(),
                            Some(getter),
                            setter,
                            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE,
                        );
                    }
                }
            }
            DeclarationKind::GenericInterface => {
                let clazz = lock
                    .as_any()
                    .downcast_ref::<GenericInterfaceDeclaration>()
                    .unwrap();
                let return_types = crate::helpers::get_generic_return_types(name);
                let type_args_str: String = return_types.names().join(",");

                for method in clazz.methods() {
                    let signature = method.return_type();
                    let return_type = Signature::to_string(method.metadata().unwrap(), &signature);
                    let return_type_index =
                        usize::from_str_radix(&*return_type.as_str().replace("Var!", ""), 10)
                            .unwrap();
                    let return_type = *return_types.names().get(return_type_index).unwrap();

                    let name = v8::String::new(scope, method.name());
                    let is_static = method.is_static();
                    let parent = declaration.clone();
                    let mut declaration = DeclarationFFI::new_with_instance(
                        Arc::new(RwLock::new(method.clone())),
                        if is_static {
                            factory.clone()
                        } else {
                            instance.clone()
                        },
                    );
                    declaration.parent = Some(parent);
                    let declaration = Box::into_raw(Box::new(declaration));
                    let return_type = v8::String::new(scope, return_type).unwrap();
                    let type_args_v8 = v8::String::new(scope, &type_args_str).unwrap();
                    let ext = v8::External::new(scope, declaration as _);
                    let data = v8::Array::new_with_elements(
                        scope,
                        &[ext.into(), return_type.into(), type_args_v8.into()],
                    );

                    let func = FunctionTemplate::builder(
                        |scope: &mut v8::PinScope<'_, '_>,
                         args: v8::FunctionCallbackArguments,
                         mut retval: v8::ReturnValue| {
                            let data = v8::Local::<v8::Array>::try_from(args.data()).unwrap();
                            let return_type = data
                                .get_index(scope, 1)
                                .unwrap()
                                .to_rust_string_lossy(scope);
                            let type_args_str = data
                                .get_index(scope, 2)
                                .unwrap()
                                .to_rust_string_lossy(scope);
                            let type_args: Vec<String> = if type_args_str.is_empty() {
                                Vec::new()
                            } else {
                                type_args_str.split(',').map(|s| s.to_owned()).collect()
                            };
                            let dec =
                                unsafe { data.get_index(scope, 0).unwrap().cast::<v8::External>() };
                            let dec = dec.value() as *mut DeclarationFFI;
                            let dec = unsafe { &*dec };
                            let lock = dec.read();
                            let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                            let parent = dec.parent.as_ref().unwrap();
                            let parent = parent.read();
                            let parent = parent
                                .as_any()
                                .downcast_ref::<GenericInterfaceDeclaration>()
                                .unwrap();
                            let Some(__ns_inst) =
                                this_instance(scope, args.this()).or_else(|| dec.instance.clone())
                            else {
                                return;
                            };
                            let mut method = GenericMethodCall::new(
                                parent,
                                method,
                                method.is_sealed(),
                                __ns_inst,
                                false,
                                return_type,
                                type_args,
                            );
                            let (ret, result, _outs) = method.call(scope, &args);
                            if ret.is_err() {
                                let detail = crate::error::format_hresult_message(ret);
                                let msg = v8::String::new(scope, &detail).unwrap();
                                let err = v8::Exception::error(scope, msg.into());
                                scope.throw_exception(err);
                                return;
                            } else if !method.is_void() {
                                let return_sig = method.return_type();
                                match NativeType::try_from(return_sig) {
                                    Ok(return_type) => {
                                        if return_sig.contains('.') {
                                            if let Some(declaration) =
                                                MetadataReader::find_by_name(return_sig)
                                            {
                                                let ret: Local<v8::Value> = if matches!(
                                                    declaration.read().kind(),
                                                    DeclarationKind::Struct
                                                ) {
                                                    crate::create_struct_object_from_raw(
                                                        declaration,
                                                        result,
                                                        scope,
                                                    )
                                                    .into()
                                                } else {
                                                    let instance = unsafe {
                                                        IUnknown::from_raw(
                                                            *(result as *mut *mut c_void),
                                                        )
                                                    };
                                                    create_ns_ctor_instance_object(
                                                        return_sig,
                                                        None,
                                                        dec.parent.clone(),
                                                        declaration,
                                                        Some(instance),
                                                        scope,
                                                    )
                                                    .into()
                                                };
                                                retval.set(ret.into());
                                                return;
                                            }
                                        }
                                        unsafe {
                                            set_ret_val(result, scope, retval, return_type);
                                        }
                                    }
                                    Err(_) => {}
                                }
                            } else {
                                retval.set_undefined();
                            }
                        },
                    )
                    .data(data.into())
                    .build(scope);

                    if is_static {
                        tmpl.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    } else {
                        proto.set_with_attr(
                            name.unwrap().into(),
                            func.into(),
                            v8::PropertyAttribute::DONT_DELETE,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    {
        let g = v8::Global::new(scope, tmpl);
        if let Some(cache) = scope.get_slot::<InstanceTemplateCache>() {
            cache.0.borrow_mut().insert(template_key, g);
        }
    }

    finish_instance_object(tmpl, declaration, instance, identity_key, scope)
}

/// Instantiate a wrapper from a (possibly cached) class template and attach the
/// per-instance state: internal fields, the `handle` property, and the
/// identity-cache entry with its weak finalizer.
pub(crate) fn finish_instance_object<'a>(
    tmpl: Local<'a, FunctionTemplate>,
    declaration: Arc<RwLock<dyn Declaration>>,
    instance: Option<IUnknown>,
    identity_key: Option<usize>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let object_tmpl = tmpl.instance_template(scope);
    let object = match object_tmpl.new_instance(scope) {
        Some(o) => o,
        None => {
            let msg = v8::String::new(scope, "Failed to create instance object").unwrap();
            let err = v8::Exception::error(scope, msg.into());
            scope.throw_exception(err);
            return v8::null(scope).into();
        }
    };

    let declaration_ffi = Box::into_raw(Box::new(DeclarationFFI::new_with_instance(
        declaration,
        instance.clone(),
    )));
    let ext = v8::External::new(scope, declaration_ffi as _);
    object.set_internal_field(0, ext.into());
    let object_store = v8::Map::new(scope);
    object.set_internal_field(1, object_store.into());

    if let Some(handle_key) = v8::String::new(scope, "handle") {
        let handle_value: Local<v8::Value> = if let Some(instance) = instance.as_ref() {
            v8::External::new(scope, instance.as_raw() as *mut c_void).into()
        } else {
            v8::null(scope).into()
        };
        // Define, don't Set: a Set on an object with named interceptors runs the WinRT setter
        // interceptor (a metadata lookup for a property called `handle`) and the prototype-chain
        // setter walk: a measurable share of every instance wrap.
        object.create_data_property(scope, handle_key.into(), handle_value);
    }

    if let Some(key) = identity_key {
        // Internal field 0 owns `declaration_ffi` (and through it an AddRef'd IUnknown); only
        // callbacks on this object read it, so it dies with the wrapper. Without this every
        // wrapped WinRT object was leaked (never Released) for the life of the process.
        let ffi_addr = declaration_ffi as usize;
        let weak = v8::Weak::with_guaranteed_finalizer(
            scope.as_mut(),
            object,
            Box::new(move || {
                crate::INSTANCE_CACHE.with(|cache| {
                    cache.borrow_mut().remove(&key);
                });
                let ffi = unsafe { Box::from_raw(ffi_addr as *mut DeclarationFFI) };
                crate::global_fns::drop_unless_com_teardown(ffi);
            }),
        );
        let new_size = crate::INSTANCE_CACHE.with(|cache| {
            let mut c = cache.borrow_mut();
            c.insert(key, weak);
            c.len()
        });
        crate::maybe_request_gc_nudge(new_size, scope.as_mut());
    }

    object.into()
}

pub(crate) fn create_ns_ctor_object<'a>(
    name: &str,
    parent: Option<Arc<RwLock<dyn Declaration>>>,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    // Re-entrancy guard: if we're already building this constructor on this
    // thread, return a lightweight stub to avoid mutating V8 templates
    // multiple times which can corrupt internal descriptor state.
    let name_str = name;
    let already_building = CREATING_CTORS.with(|set| {
        let mut set = set.borrow_mut();
        if set.iter().any(|s| s == name_str) {
            true
        } else {
            set.push(name_str.to_string());
            false
        }
    });

    if already_building {
        let stub = v8::FunctionTemplate::builder(
            |_scope: &mut v8::PinScope<'_, '_>,
             _args: v8::FunctionCallbackArguments,
             mut _retval: v8::ReturnValue| {},
        )
        .build(scope);
        let func = stub.get_function(scope).unwrap();
        let key = v8::String::new(scope, "__typeName__").unwrap();
        let val = v8::String::new(scope, name_str).unwrap();
        func.set(scope, key.into(), val.into());
        return func.into();
    }

    let name = v8::String::new(scope, name).unwrap();

    let mut ext = DeclarationFFI::new(Arc::clone(&declaration));
    ext.parent = parent;
    let ext = Box::into_raw(Box::new(ext));
    let ext = v8::External::new(scope, ext as _);

    let tmpl = v8::FunctionTemplate::builder(
        |scope: &mut v8::PinScope<'_, '_>,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let length = args.length();
            let dec = unsafe { args.data().cast::<v8::External>() };
            let dec = dec.value() as *mut DeclarationFFI;
            let dec = unsafe { &*dec };
            let lock = dec.read();
            let kind = lock.kind();
            let ext = args.data();

            match kind {
                DeclarationKind::Class => {
                    let clazz = lock.as_any().downcast_ref::<ClassDeclaration>().unwrap();

                    // Attempt activation using several candidate type names derived
                    // from metadata (full name, stripped-generic, simple name).
                    // This allows trying alternate activators when the default
                    // `RoGetActivationFactory` lookup for `full_name` doesn't work
                    // (observed with some XAML types such as FontFamily).
                    let mut clazz_factory_opt: Option<IUnknown> = None;
                    let mut last_err: Option<windows::core::Error> = None;
                    let mut candidates: Vec<String> = Vec::new();
                    candidates.push(clazz.full_name().to_string());
                    let stripped =
                        crate::helpers::strip_generic_suffix(clazz.full_name()).to_string();
                    if stripped != candidates[0] {
                        candidates.push(stripped);
                    }
                    let simple = clazz.name().to_string();
                    if !simple.is_empty() && !candidates.contains(&simple) {
                        candidates.push(simple);
                    }

                    for candidate in candidates.iter() {
                        match class_activation_factory(candidate.as_str()) {
                            Ok(factory) => {
                                clazz_factory_opt = Some(factory);
                                break;
                            }
                            Err(e) => {
                                last_err = Some(e);
                            }
                        }
                    }

                    let clazz_factory = match clazz_factory_opt {
                        Some(f) => f,
                        None => {
                            if let Some(e) = last_err {
                                throw_js_error(
                                    scope,
                                    format!(
                                        "Failed to activate WinRT class {}: {}",
                                        clazz.full_name(),
                                        e.message()
                                    )
                                    .as_str(),
                                );
                            } else {
                                throw_js_error(
                                    scope,
                                    format!("Failed to activate WinRT class {}", clazz.full_name())
                                        .as_str(),
                                );
                            }
                            return;
                        }
                    };

                    if length == 0 {
                        match clazz_factory.cast::<IActivationFactory>() {
                            Ok(activation_factory) => {
                                match unsafe { activation_factory.ActivateInstance() } {
                                    Ok(instance) => {
                                        // Upcast IInspectable -> IUnknown WITHOUT QI.
                                        // For WinRT composable XAML types, QI(IUnknown) returns a
                                        // different identity pointer with a shorter vtable. Passing
                                        // that pointer where IUIElement* is expected causes a crash
                                        // when XAML later calls UIElement vtable slots on it.
                                        let result: IUnknown = instance.into();

                                        if let Ok(init) = result.cast::<IInitializeWithWindow>() {
                                            let hwnd = unsafe { GetConsoleWindow() };
                                            if !hwnd.is_invalid() {
                                                let _ = unsafe { init.Initialize(hwnd) };
                                            }
                                        }

                                        let instance = create_ns_ctor_instance_object(
                                            clazz.name(),
                                            Some(clazz_factory.clone()),
                                            None,
                                            dec.inner.clone(),
                                            Some(result),
                                            scope,
                                        );
                                        retval.set(instance);
                                        return;
                                    }
                                    Err(error) => {
                                        throw_js_error(
                                            scope,
                                            &format!(
                                                "ActivateInstance failed for WinRT class {}: {}",
                                                clazz.full_name(),
                                                error.message()
                                            ),
                                        );
                                        return;
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }

                    unsafe {
                        let is_sealed = clazz.is_sealed();
                        for ctor in clazz.initializers() {
                            let number_of_parameters = ctor.number_of_parameters();
                            if number_of_parameters != length as usize {
                                continue;
                            }
                            let mut method =
                                MethodCall::new(ctor, is_sealed, clazz_factory.clone(), true);
                            let (ret, result, _outs) = method.call(scope, &args);

                            if ret.is_ok() {
                                if result.is_null() {
                                    retval.set(v8::null(scope).into());
                                    return;
                                }
                                // Wrap the raw result pointer as-is — do NOT QI to IUnknown identity.
                                // Same reasoning as the zero-arg path: QI(IUnknown) on a composable
                                // XAML type returns a shorter-vtable identity pointer that crashes
                                // when XAML calls UIElement vtable slots on the stored object.
                                let result = IUnknown::from_raw(result);

                                if let Ok(init) = result.cast::<IInitializeWithWindow>() {
                                    let hwnd = unsafe { GetConsoleWindow() };
                                    if !hwnd.is_invalid() {
                                        let _ = unsafe { init.Initialize(hwnd) };
                                    }
                                }

                                let instance = create_ns_ctor_instance_object(
                                    clazz.name(),
                                    Some(clazz_factory),
                                    None,
                                    dec.inner.clone(),
                                    Some(result),
                                    scope,
                                );
                                retval.set(instance);
                                return;
                            } else {
                                let message = ret.message().to_string();
                                let message = v8::String::new(scope, message.as_str()).unwrap();
                                let error = v8::Exception::error(scope, message.into());
                                scope.throw_exception(error);
                            }
                        }
                    }
                }
                DeclarationKind::Struct => {}
                DeclarationKind::Delegate
                | DeclarationKind::GenericDelegate
                | DeclarationKind::GenericDelegateInstance => {
                    if length >= 1 {
                        let arg0 = args.get(0);
                        let maybe_func: Option<v8::Local<v8::Function>> = if arg0.is_function() {
                            v8::Local::<v8::Function>::try_from(arg0).ok()
                        } else if arg0.is_object() {
                            if let Some(obj) = arg0.to_object(scope) {
                                let mut found = None;
                                for name in &["Invoke", "invoke"] {
                                    if let Some(key) = v8::String::new(scope, name) {
                                        if let Some(val) = obj.get(scope, key.into()) {
                                            if let Ok(f) = v8::Local::<v8::Function>::try_from(val)
                                            {
                                                found = Some(f);
                                                break;
                                            }
                                        }
                                    }
                                }
                                found
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some(func) = maybe_func {
                            if let Some((guid, param_types)) =
                                js_delegate_params_from_declaration(&*lock, kind)
                            {
                                let global_func = v8::Global::new(scope, func);
                                let data = Box::new(JsDelegateData {
                                    js_func: global_func,
                                    param_types,
                                });
                                let delegate = Box::new(JsDelegate {
                                    vtable: &JS_DELEGATE_VTBL as *const _,
                                    ref_count: std::sync::atomic::AtomicU32::new(1),
                                    guid,
                                    data: Box::into_raw(data),
                                });
                                let raw = Box::into_raw(delegate) as *mut c_void;
                                let result_obj = v8::Object::new(scope);
                                if let Some(key) = v8::String::new(scope, "handle") {
                                    result_obj.set(
                                        scope,
                                        key.into(),
                                        v8::External::new(scope, raw).into(),
                                    );
                                }
                                retval.set(result_obj.into());
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }

            let object_tmpl = v8::ObjectTemplate::new(scope);
            object_tmpl.set_named_property_handler(
                v8::NamedPropertyHandlerConfiguration::new()
                    .query(handle_named_property_query)
                    .getter(handle_named_property_getter)
                    .setter(handle_named_property_setter),
            );
            object_tmpl.set_indexed_property_handler(
                v8::IndexedPropertyHandlerConfiguration::new()
                    .setter(handle_indexed_property_setter)
                    .getter(handle_indexed_property_getter),
            );
            object_tmpl.set_internal_field_count(2);
            let object = object_tmpl.new_instance(scope).unwrap();
            object.set_internal_field(0, ext.into());

            let object_store = v8::Map::new(scope);

            if matches!(
                kind,
                DeclarationKind::Interface
                    | DeclarationKind::GenericInterface
                    | DeclarationKind::GenericInterfaceInstance
                    | DeclarationKind::Delegate
                    | DeclarationKind::GenericDelegate
                    | DeclarationKind::GenericDelegateInstance
                    | DeclarationKind::Event
            ) && length >= 1
            {
                let implementation = args.get(0);
                if implementation.is_object() || implementation.is_function() {
                    if let Some(impl_key) = v8::String::new(scope, "__implementation__") {
                        object_store.set(scope, impl_key.into(), implementation);
                    }
                }
            }

            object.set_internal_field(1, object_store.into());
            retval.set(object.into());
        },
    )
    .data(ext.into())
    .build(scope);
    tmpl.set_class_name(name);

    {
        let lock = declaration.read();

        if lock.kind() != DeclarationKind::Class {
            let func = tmpl.get_function(scope).unwrap();
            CREATING_CTORS.with(|set| {
                set.borrow_mut().retain(|s| s != name_str);
            });
            return func.into();
        }

        let clazz = lock.as_any().downcast_ref::<ClassDeclaration>().unwrap();

        let mut added_names: HashSet<String> = HashSet::new();

        for method in clazz.methods().iter() {
            let is_static = method.is_static();
            if !is_static {
                continue;
            }

            let m_name = method.name();
            if added_names.contains(m_name) {
                continue;
            }
            added_names.insert(m_name.to_string());

            let name = v8::String::new(scope, method.name());

            let parent = Arc::clone(&declaration);
            let mut declaration =
                DeclarationFFI::new_with_instance(Arc::new(RwLock::new(method.clone())), None);
            declaration.parent = Some(parent);
            let declaration = Box::into_raw(Box::new(declaration));
            let ext = v8::External::new(scope, declaration as _);

            let func = v8::FunctionTemplate::builder(|scope: &mut v8::PinScope<'_, '_>,
                                                      args: v8::FunctionCallbackArguments,
                                                      mut retval: v8::ReturnValue| {
                let dec = unsafe { args.data().cast::<v8::External>() };
                let dec = dec.value() as *mut DeclarationFFI;
                let dec = unsafe { &*dec };
                let lock = dec.read();
                let method = lock.as_any().downcast_ref::<MethodDeclaration>().unwrap();
                let return_type = method.return_type();
                let signature = Signature::to_string(method.metadata().unwrap(), &return_type);

                let factory = match resolve_class_factory_from_parent(dec) {
                    Ok(factory) => factory,
                    Err(error) => {
                        throw_js_error(scope, &format!(
                            "Failed to resolve WinRT static method factory for {}: {}",
                            method.name(), error.message()
                        ));
                        return;
                    }
                };

                let mut method = MethodCall::new(method, method.is_sealed(), factory, false);
                let (ret, result, _outs) = method.call(scope, &args);

                if ret.is_ok() {
                    let mut return_value_opt: Option<Local<v8::Value>> = None;
                    unsafe {
                        match signature.as_str() {
                            "Boolean" => { return_value_opt = Some(v8::Boolean::new(scope, *(result as *mut bool)).into()); }
                            "Guid" => {
                                let obj = guid_ptr_to_js_object(result, scope);
                                return_value_opt = Some(obj.into());
                            }
                            _ if !signature.contains('.') => {
                                match NativeType::try_from(signature.as_str()) {
                                    Ok(return_type) => { let v = read_value_from_ptr(result as *const c_void, scope, return_type); return_value_opt = Some(v); }
                                    Err(_) => { return_value_opt = None; }
                                }
                            }
                            _ => {
                                if result.is_null() {
                                    return_value_opt = Some(v8::null(scope).into());
                                } else {
                                    let instance = IUnknown::from_raw(result);
                                    let Some(declaration) = MetadataReader::find_by_name(signature.as_str()) else {
                                        let message = format!(
                                            "Unable to resolve return declaration for WinRT type '{}'", signature
                                        );
                                        let message = v8::String::new(scope, message.as_str()).unwrap();
                                        let error = v8::Exception::error(scope, message.into());
                                        scope.throw_exception(error);
                                        return;
                                    };
                                    let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                        signature.as_str(), dec.instance.clone(), dec.parent.clone(),
                                        declaration, Some(instance), scope,
                                    ).into();
                                    return_value_opt = Some(ret.into());
                                }
                            }
                        }
                    }

                    if !_outs.is_empty() {
                        let mut arr_len = _outs.len();
                        if return_value_opt.is_some() { arr_len += 1; }
                        let arr = v8::Array::new(scope, arr_len as i32);
                        let mut idx = 0u32;
                        if let Some(rv) = return_value_opt {
                            arr.set_index(scope, idx, rv);
                            idx += 1;
                        }
                        for outv in _outs.into_iter() {
                            arr.set_index(scope, idx, outv);
                            idx += 1;
                        }
                        retval.set(arr.into());
                    } else if let Some(rv) = return_value_opt {
                        retval.set(rv);
                    }
                } else {
                    let message = ret.message().to_string();
                    let message = v8::String::new(scope, message.as_str()).unwrap();
                    let error = v8::Exception::error(scope, message.into());
                    scope.throw_exception(error);
                }
            })
            .data(ext.into())
            .build(scope);

            tmpl.set(name.unwrap().into(), func.into());
        }

        // Collect static properties from the full class hierarchy so that statics
        // inherited from base classes (e.g. UIElement.PointerPressedEvent on Panel)
        // are exposed on the constructor and use the correct activation factory.
        let all_static_props = collect_class_properties_with_declaring(clazz);
        for (property, declaring_class_name) in all_static_props.iter() {
            if !property.is_static() {
                continue;
            }

            let prop_name_str = property.name();
            if added_names.contains(prop_name_str) {
                continue;
            }
            added_names.insert(prop_name_str.to_string());

            let Some(prop_name) = v8::String::new(scope, property.name()) else {
                continue;
            };

            let mut prop_dec =
                DeclarationFFI::new_with_instance(Arc::new(RwLock::new(property.clone())), None);
            // Lazy: for own-class statics use the declaration parent directly;
            // for inherited statics store only the class name and call
            // RoGetActivationFactory on first access via resolve_class_factory_from_parent.
            if declaring_class_name.as_str() == clazz.full_name() {
                prop_dec.parent = Some(Arc::clone(&declaration));
            } else {
                prop_dec.static_factory_class = Some(declaring_class_name.clone());
            }
            let prop_ext = Box::into_raw(Box::new(prop_dec));
            let prop_ext = v8::External::new(scope, prop_ext as _);

            let getter = v8::FunctionTemplate::builder(
                |scope: &mut v8::PinScope<'_, '_>,
                 args: v8::FunctionCallbackArguments,
                 mut retval: v8::ReturnValue| {
                    let dec = unsafe { args.data().cast::<v8::External>() };
                    let dec = dec.value() as *mut DeclarationFFI;
                    let dec = unsafe { &*dec };
                    let lock = dec.read();
                    let Some(property) = lock.as_any().downcast_ref::<PropertyDeclaration>() else {
                        return;
                    };

                    let signature = {
                        let sig = property.getter().return_type();
                        match property.getter().metadata() {
                            Some(md) => Signature::to_string(md, &sig),
                            None => return,
                        }
                    };

                    let factory = match resolve_class_factory_from_parent(dec) {
                        Ok(f) => f,
                        Err(e) => {
                            throw_js_error(
                                scope,
                                &format!(
                                    "Failed to resolve static property factory: {}",
                                    e.message()
                                ),
                            );
                            return;
                        }
                    };

                    let Some(mut prop_call) = PropertyCall::new(property, false, factory, false)
                    else {
                        return;
                    };
                    let (hresult, result, _outs) = prop_call.call_with_values(scope, &[]);

                    if hresult.is_ok() {
                        unsafe {
                            match signature.as_str() {
                                "Boolean" => {
                                    retval.set_bool(*(result as *mut bool));
                                }
                                "Guid" => {
                                    let obj = guid_ptr_to_js_object(result, scope);
                                    retval.set(obj.into());
                                }
                                _ if !signature.contains('.') => {
                                    match NativeType::try_from(signature.as_str()) {
                                        Ok(return_type) => {
                                            set_ret_val(result, scope, retval, return_type);
                                        }
                                        Err(_) => {
                                            retval.set_undefined();
                                        }
                                    }
                                }
                                _ => {
                                    if result.is_null() {
                                        retval.set(v8::null(scope).into());
                                    } else {
                                        let instance = IUnknown::from_raw(result);
                                        let Some(ret_decl) =
                                            MetadataReader::find_by_name(signature.as_str())
                                        else {
                                            return;
                                        };
                                        let ret: Local<v8::Value> = create_ns_ctor_instance_object(
                                            signature.as_str(),
                                            dec.instance.clone(),
                                            dec.parent.clone(),
                                            ret_decl,
                                            Some(instance),
                                            scope,
                                        )
                                        .into();
                                        retval.set(ret.into());
                                    }
                                }
                            }
                        }
                    }
                },
            )
            .data(prop_ext.into())
            .build(scope);

            tmpl.set_accessor_property(
                prop_name.into(),
                Some(getter),
                None,
                v8::PropertyAttribute::DONT_DELETE,
            );
        }
    }

    let func = tmpl.get_function(scope).unwrap();

    {
        let lock = declaration.read();
        if let Some(full_name) = match lock.kind() {
            DeclarationKind::Class => lock
                .as_any()
                .downcast_ref::<ClassDeclaration>()
                .map(|clazz| clazz.full_name().to_string()),
            _ => None,
        } {
            let key = v8::String::new(scope, "__typeName__").unwrap();
            let value = v8::String::new(scope, full_name.as_str()).unwrap();
            func.set(scope, key.into(), value.into());

            if let Some(class_key) = v8::String::new(scope, "__nsWinRTClass__") {
                let class_val = v8::Boolean::new(scope, true);
                func.set(scope, class_key.into(), class_val.into());
            }
        }
    }

    // Collect a small list of candidate activation names from metadata and
    // attach them to the constructor function so callers (or later runtime
    // logic) can attempt alternate activation factories when the default
    // `RoGetActivationFactory` result does not work (observed with FontFamily).
    {
        let lock = declaration.read();
        if let Some(clazz) = lock.as_any().downcast_ref::<ClassDeclaration>() {
            let mut activators: Vec<String> = Vec::new();
            let full_name = clazz.full_name().to_string();
            activators.push(full_name.clone());

            // Add a generic-stripped variant when present (e.g. remove `<T>` suffixes).
            let stripped = crate::helpers::strip_generic_suffix(full_name.as_str()).to_string();
            if stripped != full_name {
                activators.push(stripped);
            }

            // Also include the simple class name as a last-resort candidate.
            let simple = clazz.name().to_string();
            if !simple.is_empty() && !activators.contains(&simple) {
                activators.push(simple);
            }

            if let Some(key) = v8::String::new(scope, "__activators__") {
                let arr = v8::Array::new(scope, activators.len() as i32);
                for (i, s) in activators.iter().enumerate() {
                    if let Some(vs) = v8::String::new(scope, s.as_str()) {
                        arr.set_index(scope, i as u32, vs.into());
                    }
                }
                func.set(scope, key.into(), arr.into());
            }
        }
    }

    CREATING_CTORS.with(|set| {
        set.borrow_mut().retain(|s| s != name_str);
    });

    func.into()
}

pub(crate) fn ns_struct_field_getter(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let key = key.to_rust_string_lossy(scope);
    let this = args.data();
    let dec = unsafe { this.cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let lock = dec.read();

    if key == "toString" {
        let name = lock.name();
        let name = v8::String::new(scope, name).unwrap();
        let func = v8::Function::builder(
            |_scope: &mut v8::PinScope<'_, '_>,
             args: v8::FunctionCallbackArguments,
             mut retval: v8::ReturnValue| {
                retval.set(args.data());
            },
        )
        .data(name.into())
        .build(scope);
        rv.set(func.unwrap().into());
        return v8::Intercepted::kYes;
    }

    let struct_dec = lock.as_any().downcast_ref::<StructDeclaration>().unwrap();
    let mut offset = 0_isize;
    let instance = dec.struct_instance.as_ref();
    let mut position = 0;
    for field in struct_dec.fields() {
        if field.name() == key.as_str() {
            if let Some((buffer, types)) = instance {
                let mut current_field_position = 0;
                for field_type in types.iter() {
                    let size = field_type.size();
                    if position == current_field_position {
                        unsafe {
                            let buffer = buffer.as_ptr().offset(offset);
                            let slice = std::slice::from_raw_parts(buffer, size);
                            match field_type {
                                NativeType::Void => {}
                                NativeType::Bool => {
                                    let ret: &u8 = std::mem::transmute(slice.as_ptr() as *const u8);
                                    rv.set_bool(*ret == 1);
                                }
                                NativeType::U8 => {
                                    let ret: &u8 = std::mem::transmute(slice.as_ptr() as *const u8);
                                    rv.set_uint32(*ret as u32);
                                }
                                NativeType::I8 => {
                                    let ret: &i8 = std::mem::transmute(slice.as_ptr() as *const i8);
                                    rv.set_int32(*ret as i32);
                                }
                                NativeType::U16 => {
                                    let ret: &u16 =
                                        std::mem::transmute(slice.as_ptr() as *const u16);
                                    rv.set_uint32(*ret as u32);
                                }
                                NativeType::I16 => {
                                    let ret: &i16 =
                                        std::mem::transmute(slice.as_ptr() as *const i16);
                                    rv.set_int32(*ret as i32);
                                }
                                NativeType::U32 => {
                                    let ret: &u32 =
                                        std::mem::transmute(slice.as_ptr() as *const u32);
                                    rv.set_uint32(*ret);
                                }
                                NativeType::I32 => {
                                    let ret: &i32 =
                                        std::mem::transmute(slice.as_ptr() as *const i32);
                                    rv.set_int32(*ret);
                                }
                                NativeType::U64 => {
                                    let ret = *std::mem::transmute::<*const u64, &u64>(
                                        slice.as_ptr() as *const u64,
                                    );
                                    let v: v8::Local<v8::Value> = if ret > MAX_SAFE_INTEGER as u64 {
                                        v8::BigInt::new_from_u64(scope, ret).into()
                                    } else {
                                        v8::Number::new(scope, ret as f64).into()
                                    };
                                    rv.set(v);
                                }
                                NativeType::I64 => {
                                    let ret = *std::mem::transmute::<*const i64, &i64>(
                                        slice.as_ptr() as *const i64,
                                    );
                                    let v: v8::Local<v8::Value> = if ret > MAX_SAFE_INTEGER as i64
                                        || ret < MIN_SAFE_INTEGER as i64
                                    {
                                        v8::BigInt::new_from_i64(scope, ret).into()
                                    } else {
                                        v8::Number::new(scope, ret as f64).into()
                                    };
                                    rv.set(v);
                                }
                                NativeType::USize | NativeType::ISize => {}
                                NativeType::F32 => {
                                    let ret: f32 = if cfg!(target_endian = "big") {
                                        f32::from_be_bytes(<[u8; 4]>::try_from(slice).unwrap())
                                    } else {
                                        f32::from_le_bytes(<[u8; 4]>::try_from(slice).unwrap())
                                    };
                                    rv.set(v8::Number::new(scope, ret as f64).into());
                                }
                                NativeType::F64 => {
                                    let ret: &f64 =
                                        std::mem::transmute(slice.as_ptr() as *const f64);
                                    rv.set(v8::Number::new(scope, *ret).into());
                                }
                                NativeType::Pointer
                                | NativeType::Buffer
                                | NativeType::Function
                                | NativeType::Struct(_)
                                | NativeType::String => {}
                            }
                        }
                    }
                    current_field_position += 1;
                    offset += size as isize;
                }
            }
            break;
        }
        position += 1;
    }
    v8::Intercepted::kYes
}

pub(crate) fn ns_struct_field_setter(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<v8::Name>,
    value: v8::Local<v8::Value>,
    args: v8::PropertyCallbackArguments,
    _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    let key = key.to_rust_string_lossy(scope);
    let this = args.data();
    let dec = unsafe { this.cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let instance = unsafe { (&mut *dec).struct_instance.as_mut() };
    let dec = unsafe { &mut *dec };
    let lock = dec.write();
    let struct_dec = lock.as_any().downcast_ref::<StructDeclaration>().unwrap();
    let mut offset = 0_isize;
    let mut position = 0;
    for field in struct_dec.fields() {
        if field.name() == key.as_str() {
            if let Some((buffer, types)) = instance {
                let mut current_field_position = 0;
                for field_type in types.iter() {
                    let size = field_type.size();
                    if position == current_field_position {
                        let parsed = match field_type {
                            NativeType::Void => Err(error::type_error(
                                "Void is not a valid WinRT struct field type",
                            )),
                            NativeType::Bool => ffi_parse_bool_arg(value),
                            NativeType::U8 => ffi_parse_u8_arg(value),
                            NativeType::I8 => ffi_parse_i8_arg(value),
                            NativeType::U16 => ffi_parse_u16_arg(value),
                            NativeType::I16 => ffi_parse_i16_arg(value),
                            NativeType::U32 => ffi_parse_u32_arg(value),
                            NativeType::I32 => ffi_parse_i32_arg(value),
                            NativeType::U64 => ffi_parse_u64_arg(scope, value),
                            NativeType::I64 => ffi_parse_i64_arg(scope, value),
                            NativeType::USize => ffi_parse_usize_arg(scope, value),
                            NativeType::ISize => ffi_parse_isize_arg(scope, value),
                            NativeType::F32 => ffi_parse_f32_arg(value),
                            NativeType::F64 => ffi_parse_f64_arg(value),
                            NativeType::Pointer => ffi_parse_pointer_arg(scope, value),
                            NativeType::Buffer => ffi_parse_buffer_arg(scope, value),
                            NativeType::Function => ffi_parse_function_arg(scope, value),
                            NativeType::Struct(_) => ffi_parse_struct_arg(scope, value),
                            NativeType::String => ffi_parse_string_arg(scope, value),
                        };
                        match parsed {
                            Ok(v) => unsafe {
                                let buf_ptr = buffer.as_mut_ptr().offset(offset);
                                let src: *mut u8 = std::mem::transmute(v.as_arg(field_type));
                                let slice = std::slice::from_raw_parts_mut(buf_ptr, size);
                                std::ptr::copy(src, slice.as_mut_ptr(), size);
                            },
                            Err(err) => {
                                let message = v8::String::new(scope, &err.to_string()).unwrap();
                                let error = v8::Exception::error(scope, message.into());
                                scope.throw_exception(error);
                            }
                        }
                    }
                    current_field_position += 1;
                    offset += size as isize;
                }
            }
            break;
        }
        position += 1;
    }
    v8::Intercepted::kYes
}

pub(crate) fn ns_struct_field_enumerator(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Array>,
) {
    let this = args.data();
    let dec = unsafe { this.cast::<v8::External>() };
    let dec = dec.value() as *mut DeclarationFFI;
    let dec = unsafe { &*dec };
    let field_names: Vec<String> = {
        let lock = dec.read();
        match lock.as_any().downcast_ref::<StructDeclaration>() {
            Some(s) => s.fields().iter().map(|f| f.name().to_string()).collect(),
            None => return,
        }
    };
    let elements: Vec<v8::Local<v8::Value>> = field_names
        .iter()
        .filter_map(|name| v8::String::new(scope, name.as_str()).map(|s| s.into()))
        .collect();
    let array = v8::Array::new_with_elements(scope, &elements);
    rv.set(array);
}

pub(crate) fn create_ns_struct_ctor_object<'a>(
    name: &str,
    declaration: Arc<RwLock<dyn Declaration>>,
    scope: &mut v8::PinScope<'a, '_>,
) -> Local<'a, v8::Value> {
    let name = v8::String::new(scope, name).unwrap();

    let ext = DeclarationFFI::new(Arc::clone(&declaration));
    let ext = Box::into_raw(Box::new(ext));
    let ext = v8::External::new(scope, ext as _);

    let tmpl = FunctionTemplate::builder(
        |scope: &mut v8::PinScope<'_, '_>,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let dec = unsafe { args.data().cast::<v8::External>() };
            let dec = dec.value() as *mut DeclarationFFI;
            let dec = unsafe { &mut *dec };
            let lock = dec.write();
            let ext = args.data();

            let object_tmpl = v8::ObjectTemplate::new(scope);
            object_tmpl.set_internal_field_count(1);

            let mut field_args: Vec<NativeValue> = Vec::new();
            let mut field_types: Vec<NativeType> = Vec::new();

            let struct_dec = lock.as_any().downcast_ref::<StructDeclaration>().unwrap();

            let field_count = struct_dec.fields().len();
            let arg_count = args.length() as usize;
            // Positional mode: new Vector3(1, 2, 3)  — arg count matches field count and
            // the first arg is not a plain object.  Object-literal mode: new Vector3({X:1,...})
            let use_positional = arg_count == field_count
                && (arg_count > 1 || (arg_count == 1 && !args.get(0).is_object()));

            for (idx, field) in struct_dec.fields().iter().enumerate() {
                let field_type =
                    Signature::to_string(field.base().metadata().unwrap(), &field.type_());
                let native_type = NativeType::try_from(field_type.as_str()).unwrap();
                field_types.push(native_type.clone());

                let field_value = if use_positional {
                    Some(args.get(idx as i32))
                } else {
                    let object = args.get(0).to_object(scope).unwrap();
                    let name = v8::String::new(scope, field.name()).unwrap();
                    object.get(scope, name.into())
                };

                match field_value {
                    None => {
                        let message = format!("Missing key {}", field.name());
                        let message = v8::String::new(scope, message.as_str()).unwrap();
                        let error = v8::Exception::error(scope, message.into());
                        scope.throw_exception(error);
                    }
                    Some(field) => {
                        let value = match native_type {
                            NativeType::Void => Err(error::type_error(
                                "Void is not a valid WinRT struct field type",
                            )),
                            NativeType::Bool => ffi_parse_bool_arg(field),
                            NativeType::U8 => ffi_parse_u8_arg(field),
                            NativeType::I8 => ffi_parse_i8_arg(field),
                            NativeType::U16 => ffi_parse_u16_arg(field),
                            NativeType::I16 => ffi_parse_i16_arg(field),
                            NativeType::U32 => ffi_parse_u32_arg(field),
                            NativeType::I32 => ffi_parse_i32_arg(field),
                            NativeType::U64 => ffi_parse_u64_arg(scope, field),
                            NativeType::I64 => ffi_parse_i64_arg(scope, field),
                            NativeType::USize => ffi_parse_usize_arg(scope, field),
                            NativeType::ISize => ffi_parse_isize_arg(scope, field),
                            NativeType::F32 => ffi_parse_f32_arg(field),
                            NativeType::F64 => ffi_parse_f64_arg(field),
                            NativeType::Pointer => ffi_parse_pointer_arg(scope, field),
                            NativeType::Buffer => ffi_parse_buffer_arg(scope, field),
                            NativeType::Function => ffi_parse_function_arg(scope, field),
                            NativeType::Struct(_) => ffi_parse_struct_arg(scope, field),
                            NativeType::String => ffi_parse_string_arg(scope, field),
                        };
                        match value {
                            Ok(value) => {
                                field_args.push(value);
                            }
                            Err(err) => {
                                let message = err.to_string();
                                let message = v8::String::new(scope, message.as_str()).unwrap();
                                let error = v8::Exception::error(scope, message.into());
                                scope.throw_exception(error);
                            }
                        }
                    }
                }
            }

            let mut struct_size = 0_usize;
            let params = field_types
                .clone()
                .into_iter()
                .map(|item| {
                    struct_size = struct_size + item.size();
                    libffi::middle::Type::try_from(item)
                })
                .collect::<Result<Vec<libffi::middle::Type>, error::AnyError>>();

            assert!(params.is_ok());

            let mut struct_buf: Vec<u8> = unsafe { vec![0_u8; struct_size] };
            struct_buf.shrink_to_fit();

            let mut position = 0_isize;
            for (field_type, field_value) in field_types.iter().zip(field_args.iter()) {
                let size = field_type.size();
                unsafe {
                    let buffer = struct_buf.as_mut_ptr();
                    let buffer = buffer.offset(position);
                    let value: *mut u8 = std::mem::transmute(field_value.as_arg(field_type));
                    let slice = std::slice::from_raw_parts_mut(buffer, size);
                    std::ptr::copy(value, slice.as_mut_ptr(), size);
                }
                position = position + size as isize;
            }

            let name = lock.name().to_string();
            drop(lock);
            dec.struct_instance = Some((struct_buf, field_types));

            let _name = v8::String::new(scope, name.as_str()).unwrap();

            object_tmpl.set_named_property_handler(
                v8::NamedPropertyHandlerConfiguration::new()
                    .getter(ns_struct_field_getter)
                    .setter(ns_struct_field_setter)
                    .enumerator(ns_struct_field_enumerator)
                    .data(ext),
            );

            let object = object_tmpl.new_instance(scope).unwrap();
            object.set_internal_field(0, ext.into());
            retval.set(object.into());
        },
    )
    .data(ext.into())
    .build(scope);
    tmpl.set_class_name(name);

    let func = tmpl.get_function(scope).unwrap();
    func.into()
}

pub(crate) fn init_meta(
    scope: &mut v8::ContextScope<v8::HandleScope<v8::Context>>,
    context: Local<v8::Context>,
) {
    use metadata::declarations::namespace_declaration::NamespaceDeclaration;
    let global = context.global(scope);
    let global_metadata = MetadataReader::find_by_name("").unwrap();
    let data = global_metadata.read();
    let ns = data.as_any().downcast_ref::<NamespaceDeclaration>();
    if let Some(global_namespaces) = ns {
        let full_name = global_namespaces.full_name();
        for ns in global_namespaces.children() {
            let full_name = if full_name.is_empty() {
                ns.to_string()
            } else {
                format!("{}.{}", full_name, ns)
            };

            let name: Local<v8::Name> = v8::String::new(scope, ns.as_str()).unwrap().into();
            if let Some(name_space) = MetadataReader::find_by_name(full_name.as_str()) {
                let object = create_ns_object(ns, name_space, scope);
                global.define_own_property(
                    scope,
                    name,
                    object,
                    v8::PropertyAttribute::READ_ONLY
                        | v8::PropertyAttribute::DONT_DELETE
                        | v8::PropertyAttribute::NONE,
                );
            }
        }
    }
}
