//! Node-API implementation of the ns_proxy layer: namespace / constructor / instance proxies.
//!
//! Where the rusty_v8 runtime uses named-property interceptors on object templates, this
//! implementation uses JS `Proxy` objects whose `get`/`set`/`has`/`construct` traps are napi
//! closures — the pattern proven by `windows-napi/proxy-test.js` and used by napi-ios for
//! dynamic member access. Marshaling interop: the instance `get` trap answers the `handle` key
//! with a pointer external, which is the FIRST thing `try_get_external_handle` checks — so
//! instance proxies work as arguments to WinRT calls without `napi_wrap` (a Proxy would hide the
//! wrap from `napi_unwrap` anyway).

use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;

use napi::{CallContext, Env, JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue, ValueType};
use parking_lot::RwLock;
use windows::core::{IInspectable, IUnknown, Interface};
use windows::Win32::System::WinRT::IActivationFactory;

use crate::class_helpers::{class_method_matches, find_class_methods, find_class_property};
use crate::error::{generic_error, AnyError};
use crate::napi_engine::invoke::{
    invoke_instance_owned, invoke_interface_method, invoke_interface_property, invoke_property,
};
use crate::napi_engine::value::{as_unknown, clear_pending_exception, external_from_ptr};
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::property_declaration::PropertyDeclaration;
use metadata::meta_data_reader::MetadataReader;
use windows::core::GUID;

/// A method resolved on an interface tree, with the declaring interface's IID + type args.
struct ResolvedMethod {
    method: MethodDeclaration,
    iid: GUID,
    type_args: Vec<String>,
}
/// A property resolved on an interface tree.
struct ResolvedProperty {
    property: PropertyDeclaration,
    iid: GUID,
    type_args: Vec<String>,
}

fn method_name_matches(m: &MethodDeclaration, name: &str) -> bool {
    let on = m.overload_name();
    (!on.is_empty() && on == name) || m.name() == name
}

/// Walk an interface (self + implemented/required interfaces, recursively) for a method.
/// Each interface contributes its own IID + generic type args, so calls QI to the right slot.
fn resolve_iface_method(
    decl: &dyn BaseClassDeclarationImpl,
    iid: GUID,
    type_args: &[String],
    name: &str,
) -> Option<ResolvedMethod> {
    if let Some(m) = decl.methods().iter().find(|m| method_name_matches(m, name)) {
        return Some(ResolvedMethod {
            method: m.clone(),
            iid,
            type_args: type_args.to_vec(),
        });
    }
    for iface in decl.implemented_interfaces() {
        let child_iid = iface.id();
        let child_args = crate::extract_generic_type_args(iface.full_name());
        if let Some(found) = resolve_iface_method(iface, child_iid, &child_args, name) {
            return Some(found);
        }
    }
    None
}

fn resolve_iface_property(
    decl: &dyn BaseClassDeclarationImpl,
    iid: GUID,
    type_args: &[String],
    name: &str,
) -> Option<ResolvedProperty> {
    if let Some(p) = decl.properties().iter().find(|p| p.name() == name) {
        return Some(ResolvedProperty {
            property: p.clone(),
            iid,
            type_args: type_args.to_vec(),
        });
    }
    for iface in decl.implemented_interfaces() {
        let child_iid = iface.id();
        let child_args = crate::extract_generic_type_args(iface.full_name());
        if let Some(found) = resolve_iface_property(iface, child_iid, &child_args, name) {
            return Some(found);
        }
    }
    None
}

fn iface_has_member(
    decl: &dyn BaseClassDeclarationImpl,
    iid: GUID,
    type_args: &[String],
    name: &str,
) -> bool {
    resolve_iface_method(decl, iid, type_args, name).is_some()
        || resolve_iface_property(decl, iid, type_args, name).is_some()
}

/// View a declaration as `&dyn BaseClassDeclarationImpl` when it is an interface (plain or
/// generic-instance) — the forms that carry the member tables the resolvers walk.
fn as_iface_base(any: &dyn std::any::Any) -> Option<&dyn BaseClassDeclarationImpl> {
    if let Some(gi) = any.downcast_ref::<GenericInterfaceInstanceDeclaration>() {
        Some(gi)
    } else {
        any.downcast_ref::<InterfaceDeclaration>()
            .map(|i| i as &dyn BaseClassDeclarationImpl)
    }
}

/// The interface context (IID + generic type args) for a declaration, or None when it is a
/// class (which uses the MethodCall QI-by-declaring-interface path instead).
fn interface_context(declaration: &Decl) -> Option<(GUID, Vec<String>)> {
    let lock = declaration.read();
    if let Some(gi) = lock
        .as_any()
        .downcast_ref::<GenericInterfaceInstanceDeclaration>()
    {
        Some((gi.id(), crate::extract_generic_type_args(gi.full_name())))
    } else {
        lock.as_any()
            .downcast_ref::<InterfaceDeclaration>()
            .map(|i| (i.id(), Vec::new()))
    }
}

pub(crate) type Decl = Arc<RwLock<dyn Declaration>>;

/// Event registration bookkeeping — napi analog of `crate::EVENT_REGISTRY` (which stores
/// `v8::Global`s). Keyed by COM identity so registrations survive re-wrapping of the same
/// underlying object.
struct NapiEventRegistration {
    token: i64,
    env: napi::sys::napi_env,
    handler_ref: napi::sys::napi_ref,
}

impl Drop for NapiEventRegistration {
    fn drop(&mut self) {
        unsafe {
            let _ = napi::sys::napi_delete_reference(self.env, self.handler_ref);
        }
    }
}

thread_local! {
    static NAPI_EVENT_REGISTRY: std::cell::RefCell<
        ahash::AHashMap<usize, std::collections::HashMap<String, NapiEventRegistration>>,
    > = std::cell::RefCell::new(ahash::AHashMap::new());

    /// COM identity → napi_ref of the instance proxy, so the same underlying WinRT object
    /// always yields the same JS proxy (`x.Foo === x.Foo`, event bookkeeping stays coherent).
    /// Napi analog of the v8 INSTANCE_CACHE.
    static NAPI_INSTANCE_CACHE: std::cell::RefCell<ahash::AHashMap<usize, InstanceCacheEntry>>
        = std::cell::RefCell::new(ahash::AHashMap::new());

    /// Monotonic serial stamped on every InstanceState and its cache entry. Lets a state's Drop
    /// evict *only* the entry it created — never a newer proxy that reused the same COM address.
    static NAPI_INSTANCE_SERIAL: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };

    /// napi_refs whose owning cache entry was dropped during a GC finalizer. `napi_delete_reference`
    /// MUST NOT run inside a finalizer (the V8 shim's CheckGCAccess aborts the process), so eviction
    /// stashes the ref here and it is deleted at the next safe call point via [`flush_ref_graveyard`].
    static REF_GRAVEYARD: std::cell::RefCell<Vec<(napi::sys::napi_env, napi::sys::napi_ref)>>
        = const { std::cell::RefCell::new(Vec::new()) };
}

/// Delete any napi_refs deferred from finalizer-time evictions. Called from normal (non-finalizer)
/// entry points so `napi_delete_reference` never runs inside a GC callback.
fn flush_ref_graveyard() {
    REF_GRAVEYARD.with(|g| {
        for (env, r) in g.borrow_mut().drain(..) {
            unsafe {
                let _ = napi::sys::napi_delete_reference(env, r);
            }
        }
    });
}

fn next_instance_serial() -> u64 {
    NAPI_INSTANCE_SERIAL.with(|s| {
        let v = s.get();
        s.set(v.wrapping_add(1));
        v
    })
}

struct InstanceCacheEntry {
    env: napi::sys::napi_env,
    proxy_ref: napi::sys::napi_ref,
    /// The wrapped class name. A COM pointer address can be reused for a *different* WinRT type
    /// after the old object is freed; if a weak ref hands back a stale proxy for that address
    /// (observed on JSC), returning it would be the wrong type. We validate this on every hit.
    type_name: String,
    /// Serial of the InstanceState that owns this entry. On that state's Drop we remove the entry
    /// only if the serial still matches — so a newer proxy for a reused COM address is never evicted.
    serial: u64,
}

impl Drop for InstanceCacheEntry {
    fn drop(&mut self) {
        // Entries are dropped from InstanceState::Drop, which runs inside a GC finalizer. Deleting
        // the napi_ref here would call into napi mid-GC (fatal on the V8 shim), so defer it.
        REF_GRAVEYARD.with(|g| g.borrow_mut().push((self.env, self.proxy_ref)));
    }
}

/// Return the cached proxy for `identity`, if the weak ref is still live AND it wraps `class_name`.
/// The type guard rejects stale proxies for a reused COM address (a wrong-type hit crashes as e.g.
/// `SetNamedValue is not a function`); a miss simply re-wraps and overwrites the entry.
fn cached_instance(env: &Env, identity: usize, class_name: &str) -> Option<JsObject> {
    flush_ref_graveyard();
    NAPI_INSTANCE_CACHE.with(|c| {
        let cache = c.borrow();
        let entry = cache.get(&identity)?;
        if entry.type_name != class_name {
            return None;
        }
        let mut out: napi::sys::napi_value = std::ptr::null_mut();
        let st = unsafe {
            napi::sys::napi_get_reference_value(env.raw(), entry.proxy_ref, &mut out)
        };
        if st == napi::sys::Status::napi_ok && !out.is_null() {
            Some(unsafe { JsObject::from_raw_unchecked(env.raw(), out) })
        } else {
            None
        }
    })
}

/// Cache `proxy` under `identity` as a weak ref (refcount 0 — GC may reclaim; the entry is
/// re-created on the next wrap if so).
fn cache_instance(env: &Env, identity: usize, class_name: &str, serial: u64, proxy: &JsObject) {
    flush_ref_graveyard();
    let mut proxy_ref: napi::sys::napi_ref = std::ptr::null_mut();
    let st = unsafe { napi::sys::napi_create_reference(env.raw(), proxy.raw(), 0, &mut proxy_ref) };
    if st == napi::sys::Status::napi_ok {
        NAPI_INSTANCE_CACHE.with(|c| {
            c.borrow_mut().insert(
                identity,
                InstanceCacheEntry {
                    env: env.raw(),
                    proxy_ref,
                    type_name: class_name.to_string(),
                    serial,
                },
            );
        });
    }
}

/// Evict the cache entry for `identity` iff it still belongs to `serial`. Called from
/// InstanceState::Drop — i.e. the moment this proxy's WinRT instance is released and its COM
/// address becomes reusable. This closes the finalizer-ordering window where (on JSC) a stale
/// weak ref for a freed-then-reused address still resolved non-null, handing back a dead proxy
/// (`o.SetNamedValue is not a function`). The serial guard prevents evicting a newer proxy that
/// already re-cached the same address.
pub(crate) fn evict_instance(identity: usize, serial: u64) {
    NAPI_INSTANCE_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.get(&identity).map(|e| e.serial) == Some(serial) {
            cache.remove(&identity);
        }
    });
}

/// COM identity key (canonical IUnknown pointer) for registry lookups.
fn com_identity_key(instance: &IUnknown) -> Option<usize> {
    crate::com_identity(instance).map(|k| k as usize)
}

/// Wires a WinRT event: unsubscribe any prior handler, then wrap `value` as a NapiDelegate and
/// call the add_ method; token + handler ref go into the registry.
pub(crate) fn wire_winrt_event_napi(
    env: &Env,
    name: &str,
    instance: &IUnknown,
    add_method: &metadata::declarations::method_declaration::MethodDeclaration,
    remove_method: &metadata::declarations::method_declaration::MethodDeclaration,
    value: &JsUnknown,
) -> napi::Result<()> {
    use crate::method_call::MethodCall;

    let identity = com_identity_key(instance);

    // Replace semantics: remove the previous registration first.
    if let Some(id) = identity {
        let old = NAPI_EVENT_REGISTRY.with(|r| {
            r.borrow_mut()
                .get_mut(&id)
                .and_then(|events| events.remove(name))
        });
        if let Some(old) = old {
            let mut mc = MethodCall::new(
                remove_method,
                remove_method.is_sealed(),
                instance.clone(),
                false,
            );
            let _ = mc.call_with_event_token(old.token);
        }
    }

    // Assigning null/undefined just unsubscribes.
    let vt = value.get_type().unwrap_or(ValueType::Undefined);
    if vt == ValueType::Null || vt == ValueType::Undefined {
        return Ok(());
    }

    // Pre-wrapped delegate ({handle: External}) or a plain JS function → NapiDelegate.
    let handle_ptr: Option<*mut c_void> = if vt == ValueType::Object {
        let obj: JsObject = unsafe { value.cast() };
        obj.get_named_property::<JsUnknown>("handle")
            .ok()
            .and_then(|hv| crate::napi_engine::value::ptr_from_external(env, &hv))
    } else {
        None
    };
    let effective_ptr: Option<*mut c_void> = handle_ptr.or_else(|| {
        if vt != ValueType::Function {
            return None;
        }
        let func: JsFunction = unsafe { value.cast() };
        let (guid, param_types) = crate::delegate_info_from_add_method(add_method)?;
        // The delegate's IID-form name, derived exactly as `delegate_info_from_add_method`
        // derives it, keys the parameters' sealed-class declarations so the handler receives
        // typed arguments without a per-event runtime-class lookup.
        let param_classes = add_method
            .parameters()
            .first()
            .and_then(|p| {
                let iid_name =
                    metadata::signature::Signature::to_iid_string(p.metadata()?, &p.type_());
                (!iid_name.is_empty()).then(|| {
                    crate::napi_engine::delegate::delegate_param_classes(&iid_name, &param_types)
                })
            })
            .unwrap_or_default();
        crate::napi_engine::delegate::make_napi_delegate_typed(
            env,
            &func,
            guid,
            param_types,
            param_classes,
        )
    });

    if let Some(delegate_ptr) = effective_ptr {
        let mut mc = MethodCall::new(add_method, add_method.is_sealed(), instance.clone(), false);
        let (ret, token) = mc.call_with_raw_ptr(delegate_ptr);
        if ret.is_err() {
            return Err(napi::Error::from_reason(format!(
                "Event add '{}' failed: {} (0x{:08X})",
                name,
                ret.message(),
                ret.0 as u32
            )));
        }
        if let Some(id) = identity {
            let mut handler_ref: napi::sys::napi_ref = std::ptr::null_mut();
            let status = unsafe {
                napi::sys::napi_create_reference(env.raw(), value.raw(), 1, &mut handler_ref)
            };
            if status == napi::sys::Status::napi_ok {
                NAPI_EVENT_REGISTRY.with(|r| {
                    r.borrow_mut().entry(id).or_default().insert(
                        name.to_string(),
                        NapiEventRegistration {
                            token,
                            env: env.raw(),
                            handler_ref,
                        },
                    );
                });
            }
        }
    }
    Ok(())
}

/// The currently registered handler for `name` on `instance`, or null.
pub(crate) fn read_winrt_event_napi(env: &Env, instance: &IUnknown, name: &str) -> napi::Result<JsUnknown> {
    if let Some(id) = com_identity_key(instance) {
        let found = NAPI_EVENT_REGISTRY.with(|r| {
            r.borrow().get(&id).and_then(|events| {
                events.get(name).map(|e| {
                    let mut out: napi::sys::napi_value = std::ptr::null_mut();
                    let st = unsafe {
                        napi::sys::napi_get_reference_value(env.raw(), e.handler_ref, &mut out)
                    };
                    (st, out)
                })
            })
        });
        if let Some((st, out)) = found {
            if st == napi::sys::Status::napi_ok && !out.is_null() {
                return Ok(unsafe { JsUnknown::from_raw_unchecked(env.raw(), out) });
            }
        }
    }
    Ok(as_unknown(env, env.get_null()?))
}

fn napi_err(e: AnyError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

fn undefined_js(env: &Env) -> napi::Result<JsUnknown> {
    Ok(as_unknown(env, env.get_undefined()?))
}

/// `new Proxy(target, handler)` via the host's global Proxy constructor.
fn make_proxy(env: &Env, target: JsUnknown, handler: JsObject) -> napi::Result<JsObject> {
    let global = env.get_global()?;
    let proxy_ctor: JsFunction = global.get_named_property("Proxy")?;
    proxy_ctor.new_instance(&[target, as_unknown(env, handler)])
}

/// `a === b` (napi_strict_equals).
pub(crate) fn strict_equals(env: &Env, a: napi::sys::napi_value, b: napi::sys::napi_value) -> bool {
    let mut eq = false;
    unsafe { napi::sys::napi_strict_equals(env.raw(), a, b, &mut eq) };
    eq
}

/// `Object.setPrototypeOf(obj, proto)` via the host's global Object.
pub(crate) fn set_prototype_of(
    env: &Env,
    obj: &JsUnknown,
    proto: &JsUnknown,
) -> napi::Result<()> {
    let global = env.get_global()?;
    let object_unknown: JsUnknown = global.get_named_property("Object")?;
    let object: JsObject = unsafe { object_unknown.cast() };
    let set_proto: JsFunction = object.get_named_property("setPrototypeOf")?;
    let obj2 = unsafe { JsUnknown::from_raw_unchecked(env.raw(), obj.raw()) };
    let proto2 = unsafe { JsUnknown::from_raw_unchecked(env.raw(), proto.raw()) };
    set_proto.call(None, &[obj2, proto2])?;
    Ok(())
}

/// The ctor-proxy target's `.prototype` object, materialized on first use: engines differ on
/// whether napi-created functions carry an automatic `prototype` property (V8 does, the
/// QuickJS shim does not), and `class Sub extends CtorProxy` requires reading an object here.
fn ctor_target_prototype(env: &Env, target: &JsObject) -> napi::Result<JsUnknown> {
    if let Ok(existing) = target.get_named_property::<JsUnknown>("prototype") {
        if matches!(existing.get_type(), Ok(ValueType::Object)) {
            return Ok(existing);
        }
    }
    let fresh = env.create_object()?;
    let raw = unsafe { fresh.raw() };
    // The target is typed as a function; set through raw sys to sidestep JsObject casting.
    if let Ok(key) = std::ffi::CString::new("prototype") {
        unsafe {
            napi::sys::napi_set_named_property(env.raw(), target.raw(), key.as_ptr(), raw);
        }
    }
    Ok(unsafe { JsUnknown::from_raw_unchecked(env.raw(), raw) })
}

/// Subclass support (`class Sub extends WinRTClass`): when a construct trap / host ctor runs
/// with a `new.target` whose `.prototype` differs from the class's own, the freshly wrapped
/// instance is re-linked to `new.target.prototype` (which itself chains to the class prototype,
/// per class semantics) so subclass overrides and additions resolve.
pub(crate) fn adopt_new_target_prototype(
    env: &Env,
    instance: &JsObject,
    new_target: &JsUnknown,
    own_prototype: &JsUnknown,
) -> napi::Result<()> {
    if !matches!(
        new_target.get_type(),
        Ok(ValueType::Function) | Ok(ValueType::Object)
    ) {
        return Ok(());
    }
    let nt_obj = unsafe { JsObject::from_raw_unchecked(env.raw(), new_target.raw()) };
    let nt_proto: JsUnknown = nt_obj.get_named_property("prototype")?;
    if !matches!(nt_proto.get_type(), Ok(ValueType::Object)) {
        return Ok(());
    }
    if strict_equals(env, unsafe { nt_proto.raw() }, unsafe { own_prototype.raw() }) {
        return Ok(());
    }
    let inst = unsafe { JsUnknown::from_raw_unchecked(env.raw(), instance.raw()) };
    set_prototype_of(env, &inst, &nt_proto)
}

/// The trap's property-key argument as a string; None for symbols (console probes
/// Symbol.iterator etc. — a WinRT proxy ignores those).
fn trap_prop(ctx: &CallContext) -> napi::Result<Option<String>> {
    let prop = ctx.get::<JsUnknown>(1)?;
    if !matches!(prop.get_type(), Ok(ValueType::String)) {
        return Ok(None);
    }
    let s: napi::JsString = unsafe { prop.cast() };
    Ok(Some(s.into_utf8()?.as_str()?.to_owned()))
}

fn args_array_to_vec(env: &Env, arr: &JsUnknown) -> napi::Result<Vec<JsUnknown>> {
    let mut out = Vec::new();
    let mut len = 0u32;
    unsafe {
        if napi::sys::napi_get_array_length(env.raw(), arr.raw(), &mut len)
            != napi::sys::Status::napi_ok
        {
            clear_pending_exception(env);
            return Ok(out);
        }
        for i in 0..len {
            let mut v: napi::sys::napi_value = std::ptr::null_mut();
            if napi::sys::napi_get_element(env.raw(), arr.raw(), i, &mut v)
                == napi::sys::Status::napi_ok
            {
                out.push(JsUnknown::from_raw_unchecked(env.raw(), v));
            }
        }
    }
    Ok(out)
}

/// If the ctor-proxy trap's property key is `Symbol.hasInstance`, return an `instanceof`
/// predicate for `class_name` so `x instanceof <WinRTClass>` works (parity with napi-android's
/// instanceof tests). Without this, `instanceof` throws (the ctor proxy has no `.prototype`).
fn try_has_instance(
    env: &Env,
    ctx: &CallContext,
    class_name: &str,
) -> napi::Result<Option<JsUnknown>> {
    let key = ctx.get::<JsUnknown>(1)?;
    if !matches!(key.get_type(), Ok(ValueType::Symbol)) {
        return Ok(None);
    }
    // Compare the key against the well-known `Symbol.hasInstance`. `Symbol` is a Function (not a
    // plain Object), so fetch it untyped and view it as an object to read `.hasInstance`.
    let symbol_unknown: JsUnknown = env.get_global()?.get_named_property("Symbol")?;
    let symbol = unsafe { JsObject::from_raw_unchecked(env.raw(), symbol_unknown.raw()) };
    let has_instance: JsUnknown = symbol.get_named_property("hasInstance")?;
    let mut eq = false;
    unsafe { napi::sys::napi_strict_equals(env.raw(), key.raw(), has_instance.raw(), &mut eq) };
    if !eq {
        return Ok(None);
    }
    let cls = class_name.to_string();
    let f = env.create_function_from_closure("[Symbol.hasInstance]", move |c: CallContext| {
        let env = &c.env;
        let inst = c.get::<JsUnknown>(0)?;
        let is = instance_type_name(env, &inst).as_deref() == Some(cls.as_str());
        Ok(as_unknown(env, env.get_boolean(is)?))
    })?;
    Ok(Some(as_unknown(env, f)))
}

/// Read `inst.__typeName__` when `inst` is an object exposing it (our WinRT proxies do); else None.
fn instance_type_name(env: &Env, inst: &JsUnknown) -> Option<String> {
    if !matches!(inst.get_type(), Ok(ValueType::Object)) {
        return None;
    }
    let obj = unsafe { JsObject::from_raw_unchecked(env.raw(), inst.raw()) };
    let tn: JsUnknown = obj.get_named_property("__typeName__").ok()?;
    if !matches!(tn.get_type(), Ok(ValueType::String)) {
        return None;
    }
    let s = unsafe { tn.cast::<napi::JsString>() };
    Some(s.into_utf8().ok()?.as_str().ok()?.to_owned())
}

/// Resolve `full_name` and produce the right JS value: nested namespace proxy, class ctor
/// proxy, or undefined for unknown/unported kinds.
fn resolve_member(env: &Env, full_name: &str) -> napi::Result<JsUnknown> {
    let Some(declaration) = MetadataReader::find_by_name(full_name) else {
        return undefined_js(env);
    };
    let kind = declaration.read().kind();
    match kind {
        DeclarationKind::Namespace => Ok(as_unknown(
            env,
            create_namespace_proxy(env, full_name)?,
        )),
        DeclarationKind::Class => Ok(as_unknown(
            env,
            create_ctor_proxy(env, full_name, declaration)?,
        )),
        DeclarationKind::Enum => Ok(as_unknown(env, create_enum_object(env, &declaration)?)),
        // Interfaces / structs arrive with later slices.
        _ => undefined_js(env),
    }
}

/// A WinRT enum as a plain JS object of name → numeric value (Int32/UInt32, same
/// representation the v8 interceptor produced).
fn create_enum_object(env: &Env, declaration: &Decl) -> napi::Result<JsObject> {
    use metadata::declarations::enum_declaration::EnumDeclaration;
    use metadata::value::Value;

    let mut obj = env.create_object()?;
    let lock = declaration.read();
    if let Some(dec) = lock.as_any().downcast_ref::<EnumDeclaration>() {
        for member in dec.enums() {
            let name = member.name().to_string();
            match member.value() {
                Value::Int32(v) => {
                    obj.set_named_property(&name, env.create_int32(v)?)?;
                }
                Value::Uint32(v) => {
                    obj.set_named_property(&name, env.create_uint32(v)?)?;
                }
                _ => {}
            }
        }
    }
    Ok(obj)
}

/// A lazy namespace proxy: `Windows.Data.Json` resolves children through metadata on access.
pub fn create_namespace_proxy(env: &Env, full_name: &str) -> napi::Result<JsObject> {
    let mut handler = env.create_object()?;
    let ns_name = full_name.to_string();

    let get_ns = ns_name.clone();
    let get_fn = env.create_function_from_closure("get", move |ctx: CallContext| {
        let env = &ctx.env;
        let Some(prop) = trap_prop(&ctx)? else {
            return undefined_js(env);
        };
        if prop == "__typeName__" {
            return Ok(as_unknown(env, env.create_string(&get_ns)?));
        }
        resolve_member(env, &format!("{}.{}", get_ns, prop))
    })?;
    handler.set_named_property("get", get_fn)?;

    let has_ns = ns_name.clone();
    let has_fn = env.create_function_from_closure("has", move |ctx: CallContext| {
        let Some(prop) = trap_prop(&ctx)? else {
            return Ok(false);
        };
        Ok(MetadataReader::find_by_name(&format!("{}.{}", has_ns, prop)).is_some())
    })?;
    handler.set_named_property("has", has_fn)?;

    let target = as_unknown(env, env.create_object()?);
    make_proxy(env, target, handler)
}

/// A class constructor proxy: `new` activates an instance; property access resolves statics.
pub fn create_ctor_proxy(env: &Env, class_name: &str, declaration: Decl) -> napi::Result<JsObject> {
    // Hybrid: host-eligible classes get a real constructor function (static members as own props,
    // `.prototype` shared with instances → native `instanceof`). Non-host classes stay Proxy.
    if crate::napi_engine::ns_hostobject::should_host(class_name, &declaration) {
        let ctor =
            crate::napi_engine::ns_hostobject::build_host_ctor(env, class_name, declaration)?;
        return Ok(unsafe { JsObject::from_raw_unchecked(env.raw(), ctor.raw()) });
    }

    let mut handler = env.create_object()?;
    let name = class_name.to_string();

    // construct(target, args, newTarget) → activated + wrapped instance.
    let ctor_name = name.clone();
    let ctor_decl = declaration.clone();
    let construct_fn = env.create_function_from_closure("construct", move |ctx: CallContext| {
        let env = &ctx.env;
        let args_arr = ctx.get::<JsUnknown>(1)?;
        let args = args_array_to_vec(env, &args_arr)?;
        let instance = if args.is_empty() {
            activate_instance(&ctor_name).map_err(napi_err)?
        } else {
            construct_with_args(env, &ctor_name, &ctor_decl, &args).map_err(napi_err)?
        };
        let obj = create_instance_proxy(env, &ctor_name, ctor_decl.clone(), instance)?;
        // `class Sub extends WinRTClass`: super() arrives here with newTarget = Sub; link the
        // instance to Sub.prototype. Direct `new` has newTarget.prototype === target.prototype
        // (the get trap serves it) and is left alone.
        let target = ctx.get::<JsObject>(0)?;
        let own_proto = ctor_target_prototype(env, &target)?;
        if ctx.length > 2 {
            let new_target = ctx.get::<JsUnknown>(2)?;
            adopt_new_target_prototype(env, &obj, &new_target, &own_proto)?;
        }
        Ok(as_unknown(env, obj))
    })?;
    handler.set_named_property("construct", construct_fn)?;

    // get(target, prop) → static method functions (properties/events in later slices).
    let get_name = name.clone();
    let get_decl = declaration.clone();
    let get_fn = env.create_function_from_closure("get", move |ctx: CallContext| {
        let env = &ctx.env;
        // `x instanceof <WinRTClass>` — served before trap_prop (which drops symbol keys).
        if let Some(hi) = try_has_instance(env, &ctx, &get_name)? {
            return Ok(hi);
        }
        let Some(prop) = trap_prop(&ctx)? else {
            return undefined_js(env);
        };
        if prop == "__typeName__" {
            return Ok(as_unknown(env, env.create_string(&get_name)?));
        }
        if prop == "prototype" {
            // The callable target's own `.prototype` object — `class Sub extends WinRTClass`
            // reads it during class definition (must be an object), and the construct trap
            // compares against it to detect subclass construction.
            let target = ctx.get::<JsObject>(0)?;
            return ctor_target_prototype(env, &target);
        }
        // Fast path: a previously-resolved static-method function cached on the target. This skips
        // the metadata walk AND fresh-closure creation on repeat `Class.Method` access — the single
        // biggest napi-vs-classic overhead (see docs/benchmarks.md member_resolve). Static methods
        // are stable so caching is sound; static *properties* (live getters) are never cached.
        let target = ctx.get::<JsObject>(0)?;
        if let Ok(cached) = target.get_named_property::<JsUnknown>(&prop) {
            if matches!(cached.get_type(), Ok(ValueType::Function)) {
                return Ok(cached);
            }
        }
        let (methods, property, is_sealed) = {
            let lock = get_decl.read();
            let Some(class) = lock.as_any().downcast_ref::<ClassDeclaration>() else {
                return undefined_js(env);
            };
            let all_static: Vec<_> = find_class_methods(class, &prop)
                .into_iter()
                .filter(|m| m.is_static())
                .collect();
            (
                all_static,
                find_class_property(class, &prop).filter(|p| p.is_static()),
                class.is_sealed(),
            )
        };
        if !methods.is_empty() {
            let m_class = get_name.clone();
            let f = env.create_function_from_closure(&prop.clone(), move |ctx: CallContext| {
                let env = &ctx.env;
                let mut args = Vec::with_capacity(ctx.length);
                for i in 0..ctx.length {
                    args.push(ctx.get::<JsUnknown>(i)?);
                }
                // Several WinRT methods share one public name across arities (e.g.
                // Launcher.LaunchUriAsync(uri) vs (uri, options)): each is a distinct ABI
                // method on a distinct interface, so the overload must be picked by the
                // argument count actually supplied at this call, not once at property-access
                // time (that previously called the wrong vtable slot for any not-first-listed
                // overload: a null-pointer crash inside the WinRT-side DLL, not a catchable
                // JS error).
                let method = methods
                    .iter()
                    .find(|m| m.number_of_parameters() == args.len())
                    .unwrap_or(&methods[0]);
                crate::napi_engine::invoke::invoke_static_method(
                    env, &m_class, method, is_sealed, &args,
                )
                .map_err(napi_err)
            })?;
            // Cache the resolved method on the target for subsequent accesses.
            if let Ok(key) = std::ffi::CString::new(prop.as_str()) {
                unsafe {
                    napi::sys::napi_set_named_property(env.raw(), target.raw(), key.as_ptr(), f.raw());
                }
            }
            return Ok(as_unknown(env, f));
        }
        if let Some(property) = property {
            // Static property getter: PropertyCall over the activation factory.
            let factory = crate::class_activation_factory(&get_name)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?;
            return invoke_property(env, factory, &property, None).map_err(napi_err);
        }
        undefined_js(env)
    })?;
    handler.set_named_property("get", get_fn)?;

    // Target must be callable for the construct trap to fire.
    let display = name.clone();
    let target = env.create_function_from_closure("WinRTClass", move |_ctx| {
        Err::<JsUnknown, napi::Error>(napi::Error::from_reason(format!(
            "{} is a WinRT class constructor — use `new`",
            display
        )))
    })?;
    make_proxy(env, as_unknown(env, target), handler)
}

/// Parameterized construction: match an initializer by arity, invoke it as an initializer
/// MethodCall over the factory, then QI the produced instance to canonical IUnknown identity.
pub(crate) fn construct_with_args(
    env: &Env,
    class_name: &str,
    declaration: &Decl,
    args: &[JsUnknown],
) -> Result<IUnknown, AnyError> {
    use crate::method_call::MethodCall;

    let factory = crate::class_activation_factory(class_name)
        .map_err(|e| generic_error(format!("activation factory failed: {e}")))?;
    let (initializers, is_sealed) = {
        let lock = declaration.read();
        let class = lock
            .as_any()
            .downcast_ref::<ClassDeclaration>()
            .ok_or_else(|| generic_error(format!("{class_name} is not a runtime class")))?;
        (
            class.initializers().iter().cloned().collect::<Vec<_>>(),
            class.is_sealed(),
        )
    };

    for ctor in &initializers {
        if ctor.number_of_parameters() != args.len() {
            continue;
        }
        let mut method = MethodCall::new(ctor, is_sealed, factory.clone(), true);
        let (ret, result, _outs) = method.call_napi(env, args);
        if ret.is_err() {
            let detail = crate::error::format_hresult_message(ret);
            return Err(generic_error(detail));
        }
        if result.is_null() {
            return Err(generic_error(format!(
                "{class_name} constructor returned null"
            )));
        }
        let raw = unsafe { IUnknown::from_raw(result) };
        // Canonical identity QI, as in the rusty_v8 ctor path.
        let instance = raw
            .cast::<IUnknown>()
            .map_err(|e| generic_error(format!("constructed-instance QI failed: {e}")))?;
        return Ok(instance);
    }
    Err(generic_error(format!(
        "{class_name}: no constructor takes {} argument(s)",
        args.len()
    )))
}

pub(crate) fn activate_instance(class_name: &str) -> Result<IUnknown, AnyError> {
    let factory = crate::class_activation_factory(class_name)
        .map_err(|e| generic_error(format!("activation factory failed: {e}")))?;
    let af: IActivationFactory = factory
        .cast()
        .map_err(|e| generic_error(format!("{class_name} is not default-activatable: {e}")))?;
    let inspectable: IInspectable = unsafe { af.ActivateInstance() }
        .map_err(|e| generic_error(format!("ActivateInstance failed for {class_name}: {e}")))?;
    inspectable
        .cast::<IUnknown>()
        .map_err(|e| generic_error(format!("activated-instance cast failed: {e}")))
}

/// Backing state for an instance proxy, shared by its traps (Rc'd into each closure).
struct InstanceState {
    class_name: String,
    declaration: Decl,
    instance: IUnknown,
    /// Some((iid, type_args)) when `declaration` is an interface/generic-interface instance
    /// (async ops, collections); None for classes.
    iface: Option<(GUID, Vec<String>)>,
    /// COM identity + serial of this state's cache entry. When the last closure holding this Rc is
    /// finalized (releasing `instance`), Drop evicts the matching cache entry so a reused COM
    /// address can't resolve this now-dead proxy. See [`evict_instance`].
    identity: Option<usize>,
    serial: u64,
}

impl Drop for InstanceState {
    fn drop(&mut self) {
        if let Some(id) = self.identity {
            evict_instance(id, self.serial);
        }
    }
}

/// True iff `v` is the well-known `Symbol.iterator`.
fn is_symbol_iterator(env: &Env, v: &JsUnknown) -> napi::Result<bool> {
    if !matches!(v.get_type(), Ok(ValueType::Symbol)) {
        return Ok(false);
    }
    let global = env.get_global()?;
    // `Symbol` is a callable object (Function); fetch untyped, then read `.iterator`.
    let symbol_ctor: JsUnknown = global.get_named_property("Symbol")?;
    let symbol_obj: JsObject = unsafe { symbol_ctor.cast() };
    let iterator: JsUnknown = symbol_obj.get_named_property("iterator")?;
    let mut result = false;
    unsafe {
        napi::sys::napi_strict_equals(env.raw(), v.raw(), iterator.raw(), &mut result);
    }
    Ok(result)
}

/// True iff the instance exposes a method named `name` — via its interface tree for an
/// interface instance, or its class hierarchy for a class instance.
fn instance_method_exists(state: &InstanceState, name: &str) -> bool {
    let lock = state.declaration.read();
    if let Some((iid, type_args)) = &state.iface {
        as_iface_base(lock.as_any())
            .map(|b| resolve_iface_method(b, *iid, type_args, name).is_some())
            .unwrap_or(false)
    } else {
        lock.as_any()
            .downcast_ref::<ClassDeclaration>()
            .map(|c| class_method_matches(c, name))
            .unwrap_or(false)
    }
}

/// Call the instance method `name` with `args` (class or interface dispatch); `Ok(None)` when
/// the method does not exist on this instance.
fn instance_call_named(
    env: &Env,
    state: &InstanceState,
    name: &str,
    args: &[JsUnknown],
) -> napi::Result<Option<JsUnknown>> {
    if let Some((iid, type_args)) = &state.iface {
        let rm = {
            let lock = state.declaration.read();
            as_iface_base(lock.as_any())
                .and_then(|b| resolve_iface_method(b, *iid, type_args, name))
        };
        return match rm {
            Some(rm) => Ok(Some(
                invoke_interface_method(
                    env,
                    state.instance.clone(),
                    &rm.method,
                    rm.iid,
                    rm.type_args.clone(),
                    args,
                )
                .map_err(napi_err)?,
            )),
            None => Ok(None),
        };
    }
    if instance_method_exists(state, name) {
        Ok(Some(
            invoke_instance_owned(
                env,
                state.instance.clone(),
                &state.class_name,
                &state.declaration,
                name,
                args,
            )
            .map_err(napi_err)?,
        ))
    } else {
        Ok(None)
    }
}

/// True iff the instance exposes `GetAt` (IVector/IVectorView) — via its interface tree for an
/// interface instance, or its class hierarchy for a class instance.
fn is_indexable_collection(state: &InstanceState) -> bool {
    instance_method_exists(state, "GetAt")
}

/// True iff the instance exposes keyed access (IMap/IMapView/IPropertySet — `Lookup`/`HasKey`).
fn is_keyed_map(state: &InstanceState) -> bool {
    instance_method_exists(state, "Lookup") && instance_method_exists(state, "HasKey")
}

/// `HasKey(key)` for keyed-map instances; `Ok(None)` when the instance is not a keyed map.
fn map_has_key(env: &Env, state: &InstanceState, key: &str) -> napi::Result<Option<bool>> {
    if !is_keyed_map(state) {
        return Ok(None);
    }
    let key_js = as_unknown(env, env.create_string(key)?);
    match instance_call_named(env, state, "HasKey", &[key_js])? {
        Some(v) => Ok(Some(v.coerce_to_bool()?.get_value()?)),
        None => Ok(None),
    }
}

/// Keyed read sugar: `m[key]` → `Lookup(key)` when the map has the key; `Ok(None)` (→ undefined)
/// otherwise. Member names resolve first, so this only sees keys that are not WinRT members.
fn map_lookup(env: &Env, state: &InstanceState, key: &str) -> napi::Result<Option<JsUnknown>> {
    if map_has_key(env, state, key)? != Some(true) {
        return Ok(None);
    }
    let key_js = as_unknown(env, env.create_string(key)?);
    instance_call_named(env, state, "Lookup", &[key_js])
}

/// Read `prop` off the instance proxy's plain target (walking its prototype chain). This is how
/// subclass members (`class Sub extends WinRTClass` re-links the target's prototype to
/// `Sub.prototype`) and expando properties resolve; `Ok(None)` when absent/undefined.
fn target_get(ctx: &CallContext, prop: &str) -> napi::Result<Option<JsUnknown>> {
    let target = ctx.get::<JsObject>(0)?;
    let v: JsUnknown = target.get_named_property(prop)?;
    if matches!(v.get_type(), Ok(ValueType::Undefined)) {
        Ok(None)
    } else {
        Ok(Some(v))
    }
}

/// Keyed write sugar: `m[key] = v` → `Insert(key, v)`. Returns whether the instance took the
/// write (i.e. it exposes `Insert` — IMapView does not).
fn map_insert(
    env: &Env,
    state: &InstanceState,
    key: &str,
    value: &JsUnknown,
) -> napi::Result<bool> {
    if !is_keyed_map(state) || !instance_method_exists(state, "Insert") {
        return Ok(false);
    }
    let key_js = as_unknown(env, env.create_string(key)?);
    let value_js = unsafe { JsUnknown::from_raw_unchecked(env.raw(), value.raw()) };
    instance_call_named(env, state, "Insert", &[key_js, value_js])?;
    Ok(true)
}

/// Read the collection's `Size` property (class or interface dispatch); None if absent.
fn collection_size(env: &Env, state: &InstanceState) -> napi::Result<Option<JsUnknown>> {
    if let Some((iid, type_args)) = &state.iface {
        let rp = {
            let lock = state.declaration.read();
            as_iface_base(lock.as_any())
                .and_then(|b| resolve_iface_property(b, *iid, type_args, "Size"))
        };
        return match rp {
            Some(rp) => Ok(Some(
                invoke_interface_property(
                    env,
                    state.instance.clone(),
                    &rp.property,
                    rp.iid,
                    rp.type_args,
                    None,
                )
                .map_err(napi_err)?,
            )),
            None => Ok(None),
        };
    }
    let prop = {
        let lock = state.declaration.read();
        lock.as_any()
            .downcast_ref::<ClassDeclaration>()
            .and_then(|c| find_class_property(c, "Size"))
    };
    match prop {
        Some(p) if !p.is_static() => Ok(Some(
            invoke_property(env, state.instance.clone(), &p, None).map_err(napi_err)?,
        )),
        _ => Ok(None),
    }
}

/// Call the collection's `GetAt(index)` (class or interface dispatch); None if absent.
fn collection_get_at(
    env: &Env,
    state: &InstanceState,
    index: u32,
) -> napi::Result<Option<JsUnknown>> {
    let idx = as_unknown(env, env.create_uint32(index)?);
    instance_call_named(env, state, "GetAt", &[idx])
}

/// Build the `[Symbol.iterator]` factory for an IVector/IVectorView proxy: materializes
/// elements via Size + GetAt into a JS array and returns that array's iterator.
fn make_collection_iterator_fn(
    env: &Env,
    state: Rc<InstanceState>,
) -> napi::Result<JsUnknown> {
    let f = env.create_function_from_closure("[Symbol.iterator]", move |ctx: CallContext| {
        let env = &ctx.env;
        let Some(size_js) = collection_size(env, &state)? else {
            return Err(napi::Error::from_reason("not an indexable collection"));
        };
        let count = size_js.coerce_to_number()?.get_uint32()?;

        let mut array = env.create_array_with_length(count as usize)?;
        for i in 0..count {
            let Some(el) = collection_get_at(env, &state, i)? else {
                return Err(napi::Error::from_reason("GetAt failed during iteration"));
            };
            array.set_element(i, el)?;
        }
        // Return the array's own iterator (standard array-iterator protocol).
        let values: JsFunction = array.get_named_property("values")?;
        values.call(Some(&array), &[] as &[JsUnknown])
    })?;
    Ok(as_unknown(env, f))
}

/// Wrap a WinRT instance in a JS Proxy. State (class declaration + owned COM reference) lives
/// in the trap closures; `handle` exposes the pointer external for marshaling.
pub fn create_instance_proxy(
    env: &Env,
    class_name: &str,
    declaration: Decl,
    instance: IUnknown,
) -> napi::Result<JsObject> {
    let identity = com_identity_key(&instance);

    // Hybrid fast path: wrap non-collection *class* instances as host objects (shared prototype,
    // no Proxy trap). The effective type is the class from the declaration when it is itself a
    // host-eligible class (`new` path), or — for interface-typed returns like `IJsonValue` whose
    // concrete object is a class (`JsonValue`) — resolved from the runtime class name. Interfaces
    // proper and indexable collections fall through to the Proxy below.
    let host_target: Option<(String, Decl)> =
        if crate::napi_engine::ns_hostobject::should_host(class_name, &declaration) {
            Some((class_name.to_string(), declaration.clone()))
        } else if crate::napi_engine::ns_hostobject::is_interface(&declaration) {
            crate::napi_engine::ns_hostobject::runtime_class_host_target(&instance)
        } else {
            None
        };
    if let Some((eff_name, _eff_decl)) = host_target {
        if let Some(id) = identity {
            if let Some(existing) = cached_instance(env, id, &eff_name) {
                return Ok(existing);
            }
        }
        let serial = next_instance_serial();
        let obj = crate::napi_engine::ns_hostobject::build_host_instance(
            env, &eff_name, instance, serial, identity,
        )?;
        if let Some(id) = identity {
            cache_instance(env, id, &eff_name, serial, &obj);
        }
        return Ok(obj);
    }

    // Proxy path (interfaces, collections, or host disabled). Identity by the declared type.
    if let Some(id) = identity {
        if let Some(existing) = cached_instance(env, id, class_name) {
            return Ok(existing);
        }
    }
    let serial = next_instance_serial();
    let iface = interface_context(&declaration);
    let state = Rc::new(InstanceState {
        class_name: class_name.to_string(),
        declaration,
        instance,
        iface,
        identity,
        serial,
    });

    let mut handler = env.create_object()?;

    let get_state = state.clone();
    let get_fn = env.create_function_from_closure("get", move |ctx: CallContext| {
        let env = &ctx.env;
        let Some(prop) = trap_prop(&ctx)? else {
            // Non-string key. Support `Symbol.iterator` on collections (for-of / spread) by
            // materializing elements through length + GetAt.
            let raw = ctx.get::<JsUnknown>(1)?;
            if is_symbol_iterator(env, &raw)? && is_indexable_collection(&get_state) {
                return make_collection_iterator_fn(env, get_state.clone());
            }
            return undefined_js(env);
        };

        // Collection ergonomics (class OR interface): `v.length` → Size, `v[i]` → GetAt(i).
        if prop == "length" {
            if let Some(size) = collection_size(env, &get_state)? {
                return Ok(size);
            }
        }
        if let Ok(index) = prop.parse::<u32>() {
            if let Some(el) = collection_get_at(env, &get_state, index)? {
                return Ok(el);
            }
        }
        match prop.as_str() {
            "handle" => {
                let ptr = get_state.instance.as_raw() as *mut c_void;
                return external_from_ptr(env, ptr).map_err(napi_err);
            }
            "__typeName__" => {
                return Ok(as_unknown(env, env.create_string(&get_state.class_name)?));
            }
            "toString" => {
                let name = get_state.class_name.clone();
                let f = env.create_function_from_closure("toString", move |ctx: CallContext| {
                    Ok(ctx.env.create_string(&name)?)
                })?;
                return Ok(as_unknown(env, f));
            }
            _ => {}
        }

        // Interface instance: resolve method/property across the interface tree.
        if let Some((iid, type_args)) = &get_state.iface {
            let lock = get_state.declaration.read();
            let Some(base) = as_iface_base(lock.as_any()) else {
                return undefined_js(env);
            };
            if let Some(rm) = resolve_iface_method(base, *iid, type_args, &prop) {
                drop(lock);
                let m_state = get_state.clone();
                let f =
                    env.create_function_from_closure(&prop.clone(), move |ctx: CallContext| {
                        let env = &ctx.env;
                        let mut args = Vec::with_capacity(ctx.length);
                        for i in 0..ctx.length {
                            args.push(ctx.get::<JsUnknown>(i)?);
                        }
                        invoke_interface_method(
                            env,
                            m_state.instance.clone(),
                            &rm.method,
                            rm.iid,
                            rm.type_args.clone(),
                            &args,
                        )
                        .map_err(napi_err)
                    })?;
                return Ok(as_unknown(env, f));
            }
            if let Some(rp) = resolve_iface_property(base, *iid, type_args, &prop) {
                drop(lock);
                return invoke_interface_property(
                    env,
                    get_state.instance.clone(),
                    &rp.property,
                    rp.iid,
                    rp.type_args,
                    None,
                )
                .map_err(napi_err);
            }
            drop(lock);
            // Subclass members / expandos live on the target's prototype chain.
            if let Some(v) = target_get(&ctx, &prop)? {
                return Ok(v);
            }
            // Keyed-map read sugar: `m[key]` → Lookup(key) for keys that are not WinRT members.
            if let Some(v) = map_lookup(env, &get_state, &prop)? {
                return Ok(v);
            }
            return undefined_js(env);
        }

        // Class instance path. Resolve every same-named overload once, at closure-build time.
        // The returned function then skips the per-call name→metadata walk entirely, but still
        // picks among overloads by arity on each call (see the static-method site above for why:
        // same WinRT pattern, same wrong-vtable-slot crash otherwise).
        let (resolved_methods, method_is_sealed) = {
            let lock = get_state.declaration.read();
            match lock.as_any().downcast_ref::<ClassDeclaration>() {
                Some(c) => (find_class_methods(c, &prop), c.is_sealed()),
                None => (Vec::new(), true),
            }
        };
        if !resolved_methods.is_empty() {
            let m_state = get_state.clone();
            let f = env.create_function_from_closure(&prop.clone(), move |ctx: CallContext| {
                let env = &ctx.env;
                let mut args = Vec::with_capacity(ctx.length);
                for i in 0..ctx.length {
                    args.push(ctx.get::<JsUnknown>(i)?);
                }
                let method = resolved_methods
                    .iter()
                    .find(|m| m.number_of_parameters() == args.len())
                    .unwrap_or(&resolved_methods[0]);
                crate::napi_engine::invoke::invoke_instance_method_owned(
                    env,
                    m_state.instance.clone(),
                    method,
                    method_is_sealed,
                    &args,
                )
                .map_err(napi_err)
            })?;
            return Ok(as_unknown(env, f));
        }
        // Instance property getter via PropertyCall.
        let property = {
            let lock = get_state.declaration.read();
            lock.as_any()
                .downcast_ref::<ClassDeclaration>()
                .and_then(|c| find_class_property(c, &prop))
        };
        if let Some(property) = property {
            if !property.is_static() {
                return invoke_property(env, get_state.instance.clone(), &property, None)
                    .map_err(napi_err);
            }
        }
        // Event read: the currently registered handler (or null).
        let is_event = {
            let lock = get_state.declaration.read();
            lock.as_any()
                .downcast_ref::<ClassDeclaration>()
                .map(|c| crate::class_helpers::find_event_methods(c, &prop).is_some())
                .unwrap_or(false)
        };
        if is_event {
            return read_winrt_event_napi(env, &get_state.instance, &prop);
        }
        // Subclass members / expandos live on the target's prototype chain.
        if let Some(v) = target_get(&ctx, &prop)? {
            return Ok(v);
        }
        // Keyed-map read sugar: `m[key]` → Lookup(key) for keys that are not WinRT members.
        if let Some(v) = map_lookup(env, &get_state, &prop)? {
            return Ok(v);
        }
        undefined_js(env)
    })?;
    handler.set_named_property("get", get_fn)?;

    // set(target, prop, value) → property setter via PropertyCall.
    let set_state = state.clone();
    let set_fn = env.create_function_from_closure("set", move |ctx: CallContext| {
        let env = &ctx.env;
        let Some(prop) = trap_prop(&ctx)? else {
            return Ok(true);
        };
        let value = ctx.get::<JsUnknown>(2)?;

        // Interface instance: settable interface property (e.g. IAsyncAction.Completed).
        if let Some((iid, type_args)) = &set_state.iface {
            let lock = set_state.declaration.read();
            let rp = as_iface_base(lock.as_any())
                .and_then(|base| resolve_iface_property(base, *iid, type_args, &prop))
                // Read-only properties fall through to keyed/expando handling.
                .filter(|rp| rp.property.setter().is_some());
            drop(lock);
            if let Some(rp) = rp {
                invoke_interface_property(
                    env,
                    set_state.instance.clone(),
                    &rp.property,
                    rp.iid,
                    rp.type_args,
                    Some(&value),
                )
                .map_err(napi_err)?;
            } else if !map_insert(env, &set_state, &prop, &value)? {
                // Keyed-map write sugar handled it above; otherwise expandos (and subclass
                // instance fields assigned in constructors) land on the plain target.
                let mut target = ctx.get::<JsObject>(0)?;
                target.set_named_property(&prop, value)?;
            }
            return Ok(true);
        }

        // Events take precedence (v8 setter order): `obj.Click = fn` wires add_Click.
        let event_methods = {
            let lock = set_state.declaration.read();
            lock.as_any()
                .downcast_ref::<ClassDeclaration>()
                .and_then(|c| crate::class_helpers::find_event_methods(c, &prop))
        };
        if let Some((add_method, remove_method)) = event_methods {
            wire_winrt_event_napi(
                env,
                &prop,
                &set_state.instance,
                &add_method,
                &remove_method,
                &value,
            )?;
            return Ok(true);
        }
        let property = {
            let lock = set_state.declaration.read();
            lock.as_any()
                .downcast_ref::<ClassDeclaration>()
                .and_then(|c| find_class_property(c, &prop))
        };
        if let Some(property) = property {
            // Only writable properties take the assignment; a read-only name (e.g. `Size` on a
            // map) falls through to the keyed/expando handling below instead of panicking in
            // PropertyCall::new's setter unwrap.
            if !property.is_static() && property.setter().is_some() {
                invoke_property(env, set_state.instance.clone(), &property, Some(&value))
                    .map_err(napi_err)?;
                return Ok(true);
            }
        }
        // Keyed-map write sugar: `m[key] = v` → Insert(key, v).
        if map_insert(env, &set_state, &prop, &value)? {
            return Ok(true);
        }
        // Expandos (and subclass instance fields assigned in constructors) land on the plain
        // target, where the get trap's target fallback finds them again.
        let mut target = ctx.get::<JsObject>(0)?;
        target.set_named_property(&prop, value)?;
        Ok(true)
    })?;
    handler.set_named_property("set", set_fn)?;

    let has_state = state.clone();
    let has_fn = env.create_function_from_closure("has", move |ctx: CallContext| {
        let env = &ctx.env;
        let Some(prop) = trap_prop(&ctx)? else {
            return Ok(false);
        };
        if matches!(prop.as_str(), "handle" | "__typeName__" | "toString") {
            return Ok(true);
        }
        let is_member = {
            let lock = has_state.declaration.read();
            if let Some((iid, type_args)) = &has_state.iface {
                as_iface_base(lock.as_any())
                    .map(|base| iface_has_member(base, *iid, type_args, &prop))
                    .unwrap_or(false)
            } else {
                lock.as_any()
                    .downcast_ref::<ClassDeclaration>()
                    .map(|c| class_method_matches(c, &prop))
                    .unwrap_or(false)
            }
        };
        if is_member {
            return Ok(true);
        }
        // Keyed-map sugar: `key in m` → HasKey(key).
        Ok(map_has_key(env, &has_state, &prop)?.unwrap_or(false))
    })?;
    handler.set_named_property("has", has_fn)?;

    let target = as_unknown(env, env.create_object()?);
    let proxy = make_proxy(env, target, handler)?;
    if let Some(id) = identity {
        cache_instance(env, id, class_name, serial, &proxy);
    }
    Ok(proxy)
}

/// Read a WinRT value-struct from raw bytes into a plain JS object (natural ergonomics; it
/// round-trips back to bytes through `append_struct_object_bytes_napi` when passed as an arg).
/// Napi analog of `create_struct_object_from_raw`, but with correct field alignment so read
/// and write are symmetric. Nested structs recurse; enum fields become numbers.
pub fn create_struct_object_from_raw(
    env: &Env,
    declaration: &Decl,
    raw_data: *const u8,
) -> napi::Result<JsObject> {
    use metadata::declarations::struct_declaration::StructDeclaration;
    use metadata::signature::Signature;

    let mut obj = env.create_object()?;
    let lock = declaration.read();
    let Some(struct_dec) = lock.as_any().downcast_ref::<StructDeclaration>() else {
        return Ok(obj);
    };

    let mut offset = 0usize;
    for field in struct_dec.fields() {
        let Some(metadata) = field.base().metadata() else {
            continue;
        };
        let ts = Signature::to_string(metadata, &field.type_());
        let fname = field.name().to_string();
        let (fsize, falign) = crate::property_call::sig_size_align_pub(&ts);
        offset = crate::property_call::align_up(offset, falign);
        let field_ptr = unsafe { raw_data.add(offset) };

        let is_struct = ts.contains('.')
            && MetadataReader::find_by_name(crate::helpers::strip_generic_suffix(&ts))
                .map(|d| d.read().kind() == DeclarationKind::Struct)
                .unwrap_or(false);
        if is_struct {
            if let Some(nested) =
                MetadataReader::find_by_name(crate::helpers::strip_generic_suffix(&ts))
            {
                let child = create_struct_object_from_raw(env, &nested, field_ptr)?;
                obj.set_named_property(&fname, child)?;
            }
        } else if ts.contains('.') {
            // Enum field: 4-byte Int32 value.
            let v = unsafe { std::ptr::read_unaligned(field_ptr as *const i32) };
            obj.set_named_property(&fname, env.create_int32(v)?)?;
        } else if let Ok(nt) = crate::value::NativeType::try_from(ts.as_str()) {
            // SAFETY: field_ptr points at `fsize` valid bytes for `nt`.
            let js = unsafe {
                crate::napi_engine::value::read_value_from_ptr(env, field_ptr as *const _, &nt)
            }
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
            obj.set_named_property(&fname, js)?;
        }
        offset += fsize;
    }
    Ok(obj)
}

/// Try to wrap a raw COM pointer as a typed instance proxy by asking the object for its
/// runtime class name — napi analog of `ns_proxy::try_wrap_inspectable_pointer`. The pointer
/// is borrowed; the proxy takes its own reference (AddRef via clone).
pub fn try_wrap_inspectable_pointer(env: &Env, raw: *mut c_void) -> Option<JsObject> {
    if raw.is_null() {
        return None;
    }
    let owned: IUnknown = unsafe {
        let borrowed = std::mem::ManuallyDrop::new(IUnknown::from_raw(raw));
        (*borrowed).clone()
    };
    let inspectable = owned.cast::<IInspectable>().ok()?;
    let class_name = unsafe { inspectable.GetRuntimeClassName() }.ok()?;
    let name_str = class_name.to_string();
    let declaration = MetadataReader::find_by_name(&name_str)?;
    if declaration.read().kind() != DeclarationKind::Class {
        return None;
    }
    // The wrapped proxy owns `owned`; the caller's original reference stays theirs.
    create_instance_proxy(env, &name_str, declaration, owned).ok()
}
