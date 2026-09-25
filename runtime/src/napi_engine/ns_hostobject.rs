//! Host-object WinRT wrapping — the fast path of the hybrid object model.
//!
//! The engine-neutral backend can't use a native property interceptor (Node-API has none), so
//! *dynamic* member access goes through a JS `Proxy` (see [`super::ns_proxy`]). But most WinRT usage
//! is ordinary method/property/event access on a *class* instance whose member set is fully known
//! from metadata — there the Proxy's per-access trap round-trip is pure overhead.
//!
//! This module builds, per class, a shared **prototype** object carrying all instance methods
//! (functions), properties (accessors), and events (accessors), defined ONCE. An instance is then a
//! plain object linked to that prototype, so `o.Method` / `o.Prop` is an inline-cached prototype
//! lookup with no trap. The constructor is a real function: static members are own properties, and
//! its `.prototype` is the shared prototype (so `x instanceof Class` works natively).
//!
//! Hybrid boundary ([`should_host`]): host objects are used for non-collection **class** instances.
//! Interface instances, indexable collections (`v[i]` / `Symbol.iterator` / `length`), and namespace
//! objects stay Proxy — those are the genuinely dynamic cases a static prototype can't serve. The
//! COM-identity cache, eviction, and finalizer-graveyard in `ns_proxy` cover both representations.
//!
//! Kill switch: `NSWIN_NO_HOSTOBJ=1` forces the full-Proxy path.

use std::cell::{OnceCell, RefCell};
use std::sync::OnceLock;

use ahash::AHashMap;
use napi::{sys, CallContext, Env, JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue};
use smallvec::SmallVec;
use windows::core::{IInspectable, IUnknown, Interface};
use windows::Win32::System::WinRT::IActivationFactory;

use crate::class_helpers::{extend_class_methods_grouped, extend_class_properties};
use crate::error::{generic_error, AnyError};
use crate::method_call::{MethodCall, PreparedCall, QiCache};
use crate::napi_engine::invoke::{
    invoke_host_method, invoke_host_property, invoke_prepared_method, invoke_prepared_property,
    invoke_property, invoke_static_method,
};
use crate::napi_engine::ns_proxy::{
    construct_with_args, evict_instance, read_winrt_event_napi, wire_winrt_event_napi, Decl,
};
use crate::napi_engine::value::{
    as_unknown, get_named_property_raw, host_handle_from_external, raw_typeof, HANDLE_KEY,
};
use crate::property_call::{PreparedProperty, PropertyCall};
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::{Declaration, DeclarationKind};
use metadata::declarations::event_declaration::EventDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::meta_data_reader::MetadataReader;

thread_local! {
    /// class name → shared prototype object (napi_ref, strong; one per class for the process).
    static HOST_PROTOS: RefCell<AHashMap<String, napi::sys::napi_ref>> = RefCell::new(AHashMap::new());
    /// class name → constructor function (napi_ref, strong).
    static HOST_CTORS: RefCell<AHashMap<String, napi::sys::napi_ref>> = RefCell::new(AHashMap::new());
    /// Cached `Object`, `Object.create`, `Object.defineProperty` (per env/thread).
    static OBJECT_HELPERS: RefCell<Option<ObjectHelpers>> = const { RefCell::new(None) };
    /// class name → host-eligibility (non-collection class), memoized to avoid re-walking metadata
    /// for the `GetAt` check on every instance wrap.
    static HOST_ELIGIBLE: RefCell<AHashMap<String, bool>> = RefCell::new(AHashMap::new());
}

struct ObjectHelpers {
    object: napi::sys::napi_ref,
    create: napi::sys::napi_ref,
    define_property: napi::sys::napi_ref,
}

/// COM reference owned by a host instance, released on GC (via a `create_external` finalizer). It is
/// the instance's single `handle` external: arg-marshaling reads its pointer (see
/// `value::ptr_from_external`), and its Drop releases the COM ref and evicts the identity-cache entry
/// (the same finalizer-time contract as `InstanceState`).
pub(crate) struct HostHandle {
    instance: IUnknown,
    /// Interface pointers already queried on `instance`, so repeated calls through this wrapper
    /// borrow them instead of paying a QueryInterface plus refcount traffic per call.
    qi: QiCache,
    identity: Option<usize>,
    serial: u64,
}

impl HostHandle {
    /// The raw COM pointer this handle carries (borrowed — do not release).
    pub(crate) fn ptr(&self) -> *mut std::ffi::c_void {
        self.instance.as_raw()
    }

    /// The COM reference this handle owns (borrowed).
    pub(crate) fn instance(&self) -> &IUnknown {
        &self.instance
    }

    /// The per-instance QueryInterface cache.
    pub(crate) fn qi(&self) -> &QiCache {
        &self.qi
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        if let Some(id) = self.identity {
            evict_instance(id, self.serial);
        }
    }
}

fn napi_err(e: crate::error::AnyError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Host objects are on unless `NSWIN_NO_HOSTOBJ` is set. Cached (env can't change mid-process).
pub fn host_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NSWIN_NO_HOSTOBJ").is_none())
}

/// True iff the class *is* a keyed map — its default interface is IMap/IMapView/IObservableMap/
/// IPropertySet (PropertySet, ValueSet, StringMap, ApplicationDataContainerSettings, …). Those
/// stay on the Proxy path for `m[key]` / `key in m` keyed sugar. Classes that merely also
/// implement IMap alongside a richer identity (e.g. JsonObject → IJsonObject) stay host objects.
fn default_interface_is_map(class: &ClassDeclaration) -> bool {
    class
        .default_interface_full_name()
        .map(|n| {
            // The IMap prefix also covers IMapView.
            n.starts_with("Windows.Foundation.Collections.IMap")
                || n.starts_with("Windows.Foundation.Collections.IObservableMap")
                || n.starts_with("Windows.Foundation.Collections.IPropertySet")
        })
        .unwrap_or(false)
}

/// Memoized: is `name` a non-collection class (host-eligible)? Collections need the Proxy's
/// `v[i]` / `Symbol.iterator` / `length` traps and keyed maps its `m[key]` traps, so both are
/// excluded.
fn class_is_host_eligible(class: &ClassDeclaration, name: &str) -> bool {
    if let Some(v) = HOST_ELIGIBLE.with(|c| c.borrow().get(name).copied()) {
        return v;
    }
    let v = !crate::class_helpers::class_method_matches(class, "GetAt")
        && !default_interface_is_map(class);
    HOST_ELIGIBLE.with(|c| c.borrow_mut().insert(name.to_string(), v));
    v
}

/// True iff a class named `name` with `declaration` should be wrapped as a host object.
pub fn should_host(name: &str, declaration: &Decl) -> bool {
    if !host_enabled() {
        return false;
    }
    let lock = declaration.read();
    match lock.as_any().downcast_ref::<ClassDeclaration>() {
        Some(class) => class_is_host_eligible(class, name),
        None => false,
    }
}

/// True iff `declaration` is an interface (interface-typed returns often carry a concrete class
/// object; we resolve that at runtime via [`runtime_class_host_target`]).
pub fn is_interface(declaration: &Decl) -> bool {
    matches!(
        declaration.read().kind(),
        DeclarationKind::Interface
            | DeclarationKind::GenericInterface
            | DeclarationKind::GenericInterfaceInstance
    )
}

/// For an interface-typed instance, ask the concrete object for its runtime class and, if that class
/// is host-eligible, return its (name, declaration) so it can be host-wrapped instead of Proxy'd.
pub fn runtime_class_host_target(instance: &IUnknown) -> Option<(String, Decl)> {
    if !host_enabled() {
        return None;
    }
    let inspectable = instance.cast::<IInspectable>().ok()?;
    let name = unsafe { inspectable.GetRuntimeClassName() }.ok()?.to_string();
    let declaration = MetadataReader::find_by_name(&name)?;
    let eligible = {
        let lock = declaration.read();
        match lock.as_any().downcast_ref::<ClassDeclaration>() {
            Some(class) => class_is_host_eligible(class, &name),
            None => false,
        }
    };
    if eligible {
        Some((name, declaration))
    } else {
        None
    }
}

fn get_ref<T: NapiValue>(env: &Env, r: napi::sys::napi_ref) -> Option<T> {
    let mut out: napi::sys::napi_value = std::ptr::null_mut();
    let st = unsafe { napi::sys::napi_get_reference_value(env.raw(), r, &mut out) };
    if st == napi::sys::Status::napi_ok && !out.is_null() {
        Some(unsafe { T::from_raw_unchecked(env.raw(), out) })
    } else {
        None
    }
}

fn make_ref<T: NapiRaw>(env: &Env, v: &T) -> napi::sys::napi_ref {
    let mut r: napi::sys::napi_ref = std::ptr::null_mut();
    unsafe {
        napi::sys::napi_create_reference(env.raw(), v.raw(), 1, &mut r);
    }
    r
}

fn object_helpers(env: &Env) -> napi::Result<(JsObject, JsFunction, JsFunction)> {
    let refs = OBJECT_HELPERS.with(|c| c.borrow().as_ref().map(|h| (h.object, h.create, h.define_property)));
    if let Some((o, c, d)) = refs {
        if let (Some(o), Some(c), Some(d)) = (get_ref(env, o), get_ref(env, c), get_ref(env, d)) {
            return Ok((o, c, d));
        }
    }
    let global = env.get_global()?;
    // `Object` is a Function; get_named_property::<JsObject> rejects Functions, so cast from unknown.
    let object_unknown: JsUnknown = global.get_named_property("Object")?;
    let object: JsObject = unsafe { object_unknown.cast() };
    let create: JsFunction = object.get_named_property("create")?;
    let define: JsFunction = object.get_named_property("defineProperty")?;
    OBJECT_HELPERS.with(|c| {
        *c.borrow_mut() = Some(ObjectHelpers {
            object: make_ref(env, &object),
            create: make_ref(env, &create),
            define_property: make_ref(env, &define),
        });
    });
    Ok((object, create, define))
}

/// `Object.defineProperty(target, name, { get, set?, configurable: true })`.
fn define_accessor(
    env: &Env,
    target: &JsObject,
    name: &str,
    getter: JsFunction,
    setter: Option<JsFunction>,
) -> napi::Result<()> {
    let (object, _create, define) = object_helpers(env)?;
    let mut desc = env.create_object()?;
    desc.set_named_property("get", getter)?;
    if let Some(s) = setter {
        desc.set_named_property("set", s)?;
    }
    desc.set_named_property("configurable", env.get_boolean(true)?)?;
    let name_js = env.create_string(name)?;
    define.call(
        Some(&object),
        &[
            as_unknown(env, unsafe { JsUnknown::from_raw_unchecked(env.raw(), target.raw()) }),
            as_unknown(env, name_js),
            as_unknown(env, desc),
        ],
    )?;
    Ok(())
}

/// The `HostHandle` behind `this.handle`, borrowed for the duration of the callback (the JS
/// receiver keeps its external alive across the call). Raw Node-API calls only: a static key
/// for the property read, one `napi_typeof` guard (the JSC shim's `napi_get_value_external`
/// assumes an object and must never see a primitive), one `napi_get_value_external`, and a
/// `TypeId` compare against `HostHandle`; nothing allocates.
fn this_handle<'a>(ctx: &'a CallContext) -> napi::Result<&'a HostHandle> {
    let env: &Env = &ctx.env;
    let this = unsafe { ctx.this_unchecked::<JsObject>().raw() };
    let missing = || napi::Error::from_reason("host instance missing handle");
    let handle = get_named_property_raw(env, this, HANDLE_KEY).ok_or_else(missing)?;
    if raw_typeof(env, handle) != Some(sys::ValueType::napi_external) {
        return Err(missing());
    }
    host_handle_from_external(env, handle).ok_or_else(missing)
}

/// Copy the callback's arguments into a stack buffer (four inline slots cover nearly every
/// WinRT signature).
fn collect_args(ctx: &CallContext) -> napi::Result<SmallVec<[JsUnknown; 4]>> {
    let mut args: SmallVec<[JsUnknown; 4]> = SmallVec::with_capacity(ctx.length);
    for i in 0..ctx.length {
        args.push(ctx.get::<JsUnknown>(i)?);
    }
    Ok(args)
}

/// The overload whose parameter count matches the supplied argument count, by index into
/// `overloads`; the first overload when none matches (the call then raises a JS error).
fn pick_overload(overloads: &[MethodDeclaration], argc: usize) -> usize {
    overloads
        .iter()
        .position(|m| m.number_of_parameters() == argc)
        .unwrap_or(0)
}

/// The value in `cell`, filling it through `prepare` on the first call. `None` when the cell
/// is empty and `prepare` produced nothing (the caller then takes its unprepared fallback and
/// retries the preparation on the next call).
fn get_or_prepare<T>(cell: &OnceCell<T>, prepare: impl FnOnce() -> Option<T>) -> Option<&T> {
    if let Some(v) = cell.get() {
        return Some(v);
    }
    let v = prepare()?;
    let _ = cell.set(v);
    cell.get()
}

/// Default activation (`new Class()`): the class factory's `IActivationFactory` cast is kept in
/// `cell` after the first construction, so a repeat costs one `ActivateInstance` plus the
/// canonical-identity QI on the produced object.
fn activate_prepared(
    class_name: &str,
    cell: &OnceCell<IActivationFactory>,
) -> Result<IUnknown, AnyError> {
    let af = match cell.get() {
        Some(af) => af,
        None => {
            let factory = crate::class_activation_factory(class_name)
                .map_err(|e| generic_error(format!("activation factory failed: {e}")))?;
            let af: IActivationFactory = factory.cast().map_err(|e| {
                generic_error(format!("{class_name} is not default-activatable: {e}"))
            })?;
            let _ = cell.set(af);
            cell.get().expect("activation factory cell was just filled")
        }
    };
    let inspectable: IInspectable = unsafe { af.ActivateInstance() }
        .map_err(|e| generic_error(format!("ActivateInstance failed for {class_name}: {e}")))?;
    inspectable
        .cast::<IUnknown>()
        .map_err(|e| generic_error(format!("activated-instance cast failed: {e}")))
}

/// Parameterized construction (`new Class(args)`): pick the initializer by arity, prepare it on
/// the class factory once (`cells` is aligned with `initializers`), and call through the
/// prepared slot. The produced object is QI'd to its canonical IUnknown identity.
/// Falls back to `construct_with_args` when the initializer cannot be prepared.
fn construct_prepared(
    env: &Env,
    class_name: &str,
    declaration: &Decl,
    initializers: &[MethodDeclaration],
    cells: &[OnceCell<PreparedCall>],
    is_sealed: bool,
    args: &[JsUnknown],
) -> Result<IUnknown, AnyError> {
    let Some(idx) = initializers
        .iter()
        .position(|ctor| ctor.number_of_parameters() == args.len())
    else {
        return Err(generic_error(format!(
            "{class_name}: no constructor takes {} argument(s)",
            args.len()
        )));
    };
    let prepared = get_or_prepare(&cells[idx], || {
        let factory = crate::class_activation_factory(class_name).ok()?;
        MethodCall::prepare(&initializers[idx], is_sealed, factory, true)
    });
    let Some(prepared) = prepared else {
        return construct_with_args(env, class_name, declaration, args);
    };
    let mut method = MethodCall::from_prepared(prepared);
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
    raw.cast::<IUnknown>()
        .map_err(|e| generic_error(format!("constructed-instance QI failed: {e}")))
}

/// Enumerate this class's events across its interface/base tree (dedup by name).
fn collect_events(class: &ClassDeclaration) -> Vec<EventDeclaration> {
    fn walk(class: &ClassDeclaration, out: &mut Vec<EventDeclaration>, seen: &mut std::collections::HashSet<String>) {
        use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
        for e in class.events() {
            if seen.insert(e.name().to_string()) {
                out.push(e.clone());
            }
        }
        if let Some(di) = class.default_interface() {
            for e in di.events() {
                if seen.insert(e.name().to_string()) {
                    out.push(e.clone());
                }
            }
        }
        for iface in class.implemented_interfaces() {
            for e in iface.events() {
                if seen.insert(e.name().to_string()) {
                    out.push(e.clone());
                }
            }
        }
        if !class.base_full_name().is_empty() {
            if let Some(base) = MetadataReader::find_by_name(class.base_full_name()) {
                if let Some(bc) = base.read().as_any().downcast_ref::<ClassDeclaration>() {
                    walk(bc, out, seen);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(class, &mut out, &mut std::collections::HashSet::new());
    out
}

/// Build (or return cached) the shared prototype for `class_name`: instance methods (functions),
/// properties (accessors), events (accessors), plus `toString`.
fn class_prototype(env: &Env, class_name: &str) -> napi::Result<JsObject> {
    if let Some(r) = HOST_PROTOS.with(|c| c.borrow().get(class_name).copied()) {
        if let Some(p) = get_ref::<JsObject>(env, r) {
            return Ok(p);
        }
    }

    let declaration = MetadataReader::find_by_name(class_name)
        .ok_or_else(|| napi::Error::from_reason(format!("type not found: {class_name}")))?;
    let mut proto = env.create_object()?;

    // Snapshot metadata under the read lock, then build closures without holding it.
    let (methods_by_name, properties, events, is_sealed): (
        std::collections::HashMap<String, Vec<_>>,
        Vec<_>,
        Vec<_>,
        bool,
    ) = {
        let lock = declaration.read();
        let class = lock
            .as_any()
            .downcast_ref::<ClassDeclaration>()
            .ok_or_else(|| napi::Error::from_reason(format!("{class_name} is not a class")))?;
        let mut methods = std::collections::HashMap::new();
        extend_class_methods_grouped(class, &mut methods);
        let mut props = Vec::new();
        extend_class_properties(class, &mut props, &mut std::collections::HashSet::new());
        (methods, props, collect_events(class), class.is_sealed())
    };

    // Methods (instance) → prototype functions. Each closure captures every overload sharing
    // this public name (usually one) and, at call time, picks the one matching the argument
    // count actually supplied: WinRT overloads sharing a name are distinct ABI methods on
    // distinct interfaces (e.g. `IAsyncOperation`-returning methods with an options parameter),
    // so calling the wrong one is a wrong vtable slot, not just wrong behavior: it can crash
    // inside the WinRT-side DLL instead of raising a catchable JS error.
    for (js_name, overloads) in &methods_by_name {
        if overloads.iter().all(|m| m.is_static()) {
            continue;
        }
        let overloads: Vec<_> = overloads.iter().filter(|m| !m.is_static()).cloned().collect();
        let f = env.create_function_from_closure(js_name, move |ctx: CallContext| {
            let env = &ctx.env;
            let handle = this_handle(&ctx)?;
            let args = collect_args(&ctx)?;
            let method = &overloads[pick_overload(&overloads, args.len())];
            invoke_host_method(env, handle, method, is_sealed, &args).map_err(napi_err)
        })?;
        proto.set_named_property(js_name, f)?;
    }

    // Properties (instance) → accessors.
    for p in &properties {
        if p.is_static() {
            continue;
        }
        let name = p.name().to_string();
        let getter_prop = p.clone();
        let getter = env.create_function_from_closure(&name, move |ctx: CallContext| {
            let env = &ctx.env;
            let handle = this_handle(&ctx)?;
            invoke_host_property(env, handle, &getter_prop, None).map_err(napi_err)
        })?;
        let setter = if p.setter().is_some() {
            let setter_prop = p.clone();
            let sname = name.clone();
            Some(env.create_function_from_closure(&sname, move |ctx: CallContext| {
                let env = &ctx.env;
                let handle = this_handle(&ctx)?;
                let value = ctx.get::<JsUnknown>(0)?;
                invoke_host_property(env, handle, &setter_prop, Some(&value)).map_err(napi_err)
            })?)
        } else {
            None
        };
        define_accessor(env, &proto, &name, getter, setter)?;
    }

    // Events → accessors (`obj.Click` reads the handler, `obj.Click = fn` wires add/remove).
    for e in &events {
        let name = e.name().to_string();
        let add = e.add_method().clone();
        let remove = e.remove_method().clone();
        let gname = name.clone();
        let getter = env.create_function_from_closure(&name, move |ctx: CallContext| {
            let env = &ctx.env;
            let handle = this_handle(&ctx)?;
            read_winrt_event_napi(env, handle.instance(), &gname)
        })?;
        let sname = name.clone();
        let setter = env.create_function_from_closure(&name, move |ctx: CallContext| {
            let env = &ctx.env;
            let handle = this_handle(&ctx)?;
            let value = ctx.get::<JsUnknown>(0)?;
            wire_winrt_event_napi(env, &sname, handle.instance(), &add, &remove, &value)?;
            Ok(as_unknown(env, env.get_undefined()?))
        })?;
        define_accessor(env, &proto, &name, getter, Some(setter))?;
    }

    // toString → the class name (parity with the Proxy get-trap).
    let cls = class_name.to_string();
    let to_string = env.create_function_from_closure("toString", move |ctx: CallContext| {
        Ok(ctx.env.create_string(&cls)?)
    })?;
    proto.set_named_property("toString", to_string)?;
    // __typeName__ is the same for every instance → carry it once on the prototype, not per-instance.
    proto.set_named_property("__typeName__", env.create_string(class_name)?)?;

    HOST_PROTOS.with(|c| {
        c.borrow_mut().insert(class_name.to_string(), make_ref(env, &proto));
    });
    Ok(proto)
}

/// Build a host instance for an owned COM reference: a fresh object linked to the class prototype,
/// carrying `handle` (pointer external for arg-marshaling), `__typeName__`, and `__comref` (owns the
/// COM ref + evicts the cache on GC). `serial`/`identity` come from the caller's cache bookkeeping.
pub fn build_host_instance(
    env: &Env,
    class_name: &str,
    instance: IUnknown,
    serial: u64,
    identity: Option<usize>,
) -> napi::Result<JsObject> {
    let proto = class_prototype(env, class_name)?;
    let (object, create, _define) = object_helpers(env)?;
    let obj_val = create.call(Some(&object), &[proto])?;
    let mut obj: JsObject = unsafe { obj_val.cast() };

    // One external does both jobs: arg-marshaling reads its pointer (value::ptr_from_external),
    // and its finalizer releases the COM ref + evicts the identity cache on GC.
    let handle = env.create_external(
        HostHandle {
            instance,
            qi: QiCache::new(),
            identity,
            serial,
        },
        None,
    )?;
    obj.set_named_property("handle", handle)?;
    Ok(obj)
}

/// Wrap the native ctor body in a source-compiled JS function (see call site for why). The
/// factory is built through the global `Function` constructor: `Function('native',
/// 'sharedProto', 'clsName', BODY)` yields a function that closes over nothing, takes the three
/// values as parameters, and returns the public constructor.
fn make_ctor_wrapper(
    env: &Env,
    impl_fn: &JsFunction,
    proto: &JsObject,
    class_name: &str,
) -> napi::Result<JsFunction> {
    const BODY: &str = r#"'use strict';
function Ctor() {
    if (!new.target) { throw new TypeError(clsName + ' is a WinRT class constructor — use `new`'); }
    var obj = arguments.length === 0 ? native() : native.apply(null, arguments);
    var p = new.target.prototype;
    if (p && typeof p === 'object' && p !== sharedProto) {
        var q = Object.getPrototypeOf(p), ok = false;
        for (var i = 0; i < 32 && q; i++) { if (q === sharedProto) { ok = true; break; } q = Object.getPrototypeOf(q); }
        if (ok) { Object.setPrototypeOf(obj, p); }
    }
    return obj;
}
try { Object.defineProperty(Ctor, 'name', { value: clsName, configurable: true }); } catch (e) {}
return Ctor;"#;
    let global = env.get_global()?;
    let function_ctor_unknown: JsUnknown = global.get_named_property("Function")?;
    let function_ctor: JsFunction = unsafe { function_ctor_unknown.cast() };
    let factory_unknown = function_ctor.call(
        None,
        &[
            as_unknown(env, env.create_string("native")?),
            as_unknown(env, env.create_string("sharedProto")?),
            as_unknown(env, env.create_string("clsName")?),
            as_unknown(env, env.create_string(BODY)?),
        ],
    )?;
    let factory: JsFunction = unsafe { factory_unknown.cast() };
    let wrapper_unknown = factory.call(
        None,
        &[
            as_unknown(env, unsafe {
                JsUnknown::from_raw_unchecked(env.raw(), impl_fn.raw())
            }),
            as_unknown(env, unsafe {
                JsUnknown::from_raw_unchecked(env.raw(), proto.raw())
            }),
            as_unknown(env, env.create_string(class_name)?),
        ],
    )?;
    Ok(unsafe { wrapper_unknown.cast() })
}

/// Build (or return cached) the host constructor for `class_name`: `new` activates/constructs and
/// wraps (via `create_instance_proxy`, so the host/proxy decision stays centralized); static methods
/// are own function properties; static properties are own accessors; `.prototype` is the shared
/// class prototype so `x instanceof Class` works natively.
pub fn build_host_ctor(env: &Env, class_name: &str, declaration: Decl) -> napi::Result<JsFunction> {
    if let Some(r) = HOST_CTORS.with(|c| c.borrow().get(class_name).copied()) {
        if let Some(f) = get_ref::<JsFunction>(env, r) {
            return Ok(f);
        }
    }

    let proto = class_prototype(env, class_name)?;

    // The native constructor body: activate/construct the COM instance and wrap it. It is
    // always invoked as a PLAIN call by the JS wrapper below — never with `new` directly.
    let cls = class_name.to_string();
    let ctor_decl = declaration.clone();
    // Snapshot the initializers once; each gets a prepared-call slot filled on first use, and
    // the default-activation factory cast is cached the same way.
    let (initializers, ctor_sealed): (Vec<MethodDeclaration>, bool) = {
        let lock = declaration.read();
        match lock.as_any().downcast_ref::<ClassDeclaration>() {
            Some(class) => (
                class.initializers().iter().cloned().collect(),
                class.is_sealed(),
            ),
            None => (Vec::new(), true),
        }
    };
    let init_cells: Vec<OnceCell<PreparedCall>> =
        initializers.iter().map(|_| OnceCell::new()).collect();
    let activation_cell: OnceCell<IActivationFactory> = OnceCell::new();
    let impl_fn = env.create_function_from_closure(class_name, move |ctx: CallContext| {
        let env = &ctx.env;
        let args = collect_args(&ctx)?;
        let instance = if args.is_empty() {
            activate_prepared(&cls, &activation_cell).map_err(napi_err)?
        } else {
            construct_prepared(
                env,
                &cls,
                &ctor_decl,
                &initializers,
                &init_cells,
                ctor_sealed,
                &args,
            )
            .map_err(napi_err)?
        };
        let obj = crate::napi_engine::ns_proxy::create_instance_proxy(
            env,
            &cls,
            ctor_decl.clone(),
            instance,
        )?;
        Ok(as_unknown(env, obj))
    })?;

    // The public constructor is a REAL JS function (built through the global `Function`
    // constructor) so `new.target` is plain JS semantics on every engine — the napi
    // `napi_get_new_target` / callback-`this` routes are unreliable or absent on some
    // standalone shims (QuickJS passes new.target as `this`; JSC's C API has neither).
    // It enforces construct-only calls and, for `class Sub extends WinRTClass`, re-links the
    // wrapped instance to new.target.prototype when that prototype chains through the shared
    // class prototype.
    let ctor = make_ctor_wrapper(env, &impl_fn, &proto, class_name)?;

    let ctor_raw = unsafe { ctor.raw() };
    let mut ctor_obj = unsafe { JsObject::from_raw_unchecked(env.raw(), ctor_raw) };
    ctor_obj.set_named_property("prototype", proto)?;
    ctor_obj.set_named_property("__typeName__", env.create_string(class_name)?)?;

    // Static members.
    let (static_methods_by_name, static_props, is_sealed): (
        std::collections::HashMap<String, Vec<_>>,
        Vec<_>,
        bool,
    ) = {
        let lock = declaration.read();
        if let Some(class) = lock.as_any().downcast_ref::<ClassDeclaration>() {
            let mut methods = std::collections::HashMap::new();
            extend_class_methods_grouped(class, &mut methods);
            for overloads in methods.values_mut() {
                overloads.retain(|m| m.is_static());
            }
            methods.retain(|_, overloads| !overloads.is_empty());
            let mut props = Vec::new();
            extend_class_properties(class, &mut props, &mut std::collections::HashSet::new());
            (
                methods,
                props.into_iter().filter(|p| p.is_static()).collect(),
                class.is_sealed(),
            )
        } else {
            (std::collections::HashMap::new(), Vec::new(), true)
        }
    };

    // One property per public name; the closure picks the overload matching the argument count
    // supplied at call time (see the instance-method loop above for why this matters: e.g.
    // `Windows.System.Launcher.LaunchUriAsync` has 1/2/3-arg overloads on distinct interfaces,
    // and always resolving to whichever one metadata listed first crashes instead of erroring
    // when the caller's arg count picks a different one).
    // Each overload gets its own prepared-call slot (overloads sharing a name may live on
    // different static interfaces), resolved on the class activation factory on first use;
    // later calls borrow the prepared interface pointer and vtable slot.
    for (js_name, overloads) in &static_methods_by_name {
        let cls = class_name.to_string();
        let overloads = overloads.clone();
        let cells: Vec<OnceCell<PreparedCall>> =
            overloads.iter().map(|_| OnceCell::new()).collect();
        let f = env.create_function_from_closure(js_name, move |ctx: CallContext| {
            let env = &ctx.env;
            crate::napi_engine::invoke::ensure_winrt_initialized();
            let args = collect_args(&ctx)?;
            let idx = pick_overload(&overloads, args.len());
            let method = &overloads[idx];
            let prepared = get_or_prepare(&cells[idx], || {
                let factory = crate::class_activation_factory(&cls).ok()?;
                MethodCall::prepare(method, is_sealed, factory, false)
            });
            match prepared {
                Some(p) => invoke_prepared_method(env, p, &args).map_err(napi_err),
                None => invoke_static_method(env, &cls, method, is_sealed, &args).map_err(napi_err),
            }
        })?;
        ctor_obj.set_named_property(js_name, f)?;
    }

    for p in &static_props {
        // Static property getter/setter via the activation factory (PropertyCall over the
        // factory), each accessor prepared once on first use.
        let name = p.name().to_string();
        let cls = class_name.to_string();
        let getter_prop = p.clone();
        let getter_cell: OnceCell<PreparedProperty> = OnceCell::new();
        let getter = env.create_function_from_closure(&name, move |ctx: CallContext| {
            let env = &ctx.env;
            crate::napi_engine::invoke::ensure_winrt_initialized();
            let prepared = get_or_prepare(&getter_cell, || {
                let factory = crate::class_activation_factory(&cls).ok()?;
                PropertyCall::prepare(&getter_prop, false, factory)
            });
            match prepared {
                Some(p) => invoke_prepared_property(env, p, &getter_prop, None).map_err(napi_err),
                None => {
                    let factory = crate::class_activation_factory(&cls)
                        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
                    invoke_property(env, factory, &getter_prop, None).map_err(napi_err)
                }
            }
        })?;
        let setter = if p.setter().is_some() {
            let setter_prop = p.clone();
            let cls2 = class_name.to_string();
            let sname = name.clone();
            let setter_cell: OnceCell<PreparedProperty> = OnceCell::new();
            Some(env.create_function_from_closure(&sname, move |ctx: CallContext| {
                let env = &ctx.env;
                crate::napi_engine::invoke::ensure_winrt_initialized();
                let value = ctx.get::<JsUnknown>(0)?;
                let prepared = get_or_prepare(&setter_cell, || {
                    let factory = crate::class_activation_factory(&cls2).ok()?;
                    PropertyCall::prepare(&setter_prop, true, factory)
                });
                match prepared {
                    Some(p) => invoke_prepared_property(env, p, &setter_prop, Some(&value))
                        .map_err(napi_err),
                    None => {
                        let factory = crate::class_activation_factory(&cls2)
                            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
                        invoke_property(env, factory, &setter_prop, Some(&value)).map_err(napi_err)
                    }
                }
            })?)
        } else {
            None
        };
        define_accessor(env, &ctor_obj, &name, getter, setter)?;
    }

    HOST_CTORS.with(|c| {
        c.borrow_mut().insert(class_name.to_string(), make_ref(env, &ctor));
    });
    Ok(ctor)
}
