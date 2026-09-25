use crate::error::AnyError;
use crate::helpers::{ffi_native_type_from_signature, strip_generic_suffix};
use crate::method_call::{is_scalar_return_type, PointerPlan, QiCache};
#[cfg(feature = "classic")]
use crate::value::{
    append_struct_field_bytes, ffi_parse_bool_arg, ffi_parse_buffer_arg_with_length,
    ffi_parse_f32_arg, ffi_parse_f64_arg, ffi_parse_function_arg, ffi_parse_i16_arg,
    ffi_parse_i32_arg, ffi_parse_i64_arg, ffi_parse_i8_arg, ffi_parse_isize_arg,
    ffi_parse_pointer_arg, ffi_parse_query_interface_arg, ffi_parse_string_arg,
    ffi_parse_struct_arg, ffi_parse_u16_arg, ffi_parse_u32_arg, ffi_parse_u64_arg,
    ffi_parse_u8_arg, ffi_parse_usize_arg, read_value_from_ptr, set_out_param_value,
    try_unwrap_out_param, write_v8_value_to_ptr,
};
use crate::value::{NativeType, NativeValue};
use crate::ReturnKind;
use libffi::middle::*;
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
#[cfg(feature = "classic")]
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::Declaration;
use metadata::declarations::declaration::DeclarationKind;
#[cfg(feature = "classic")]
use metadata::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::parameter_declaration::ParameterDeclaration;
use metadata::declarations::property_declaration::PropertyDeclaration;
use metadata::declarations::struct_declaration::StructDeclaration;
use metadata::declaring_interface_for_method::Metadata;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::Arc;
use windows::core::{IInspectable, IUnknown, Interface, GUID, HRESULT};
use windows::Win32::System::WinRT::IActivationFactory;
use windows::Win32::System::WinRT::Metadata::CorTokenType;

#[inline]
pub(crate) fn align_up(n: usize, a: usize) -> usize {
    if a <= 1 {
        n
    } else {
        (n + a - 1) / a * a
    }
}

/// (size, alignment) for a field signature — engine-neutral; reused by the napi struct reader.
pub(crate) fn sig_size_align_pub(sig: &str) -> (usize, usize) {
    sig_size_align(sig)
}

/// (size, alignment) in bytes for a struct field signature. Recurses into nested structs; WinRT enums
/// are 4-byte Int32; primitives use their native size.
fn sig_size_align(sig: &str) -> (usize, usize) {
    if sig.contains('.') {
        let lookup = strip_generic_suffix(sig);
        if let Some(decl) = MetadataReader::find_by_name(lookup) {
            let lock = decl.read();
            match lock.kind() {
                DeclarationKind::Struct => {
                    if let Some(sd) = lock.as_any().downcast_ref::<StructDeclaration>() {
                        return struct_size_align(sd);
                    }
                }
                DeclarationKind::Enum => return (4, 4),
                _ => {}
            }
        }
        return (4, 4); // unknown dotted ref in a value struct — treat as Int32-sized enum
    }
    if let Ok(nt) = NativeType::try_from(sig) {
        let s = nt.size().max(1);
        return (s, s);
    }
    (4, 4)
}

/// (size, alignment) for a whole struct: standard C layout — fields placed at aligned offsets, total
/// rounded up to the struct's alignment (the max field alignment).
fn struct_size_align(sd: &StructDeclaration) -> (usize, usize) {
    let mut size = 0usize;
    let mut align = 1usize;
    for f in sd.fields().iter() {
        if let Some(m) = f.base().metadata() {
            let ts = Signature::to_string(m, &f.type_());
            let (fs, fa) = sig_size_align(&ts);
            size = align_up(size, fa) + fs;
            if fa > align {
                align = fa;
            }
        }
    }
    (align_up(size, align), align)
}

/// Serializes a JS object into a WinRT value-struct's byte layout, honoring field alignment, trailing
/// padding, nested structs (recursed), and enum fields (Int32). The naive field-concatenation path zeroed
/// nested-struct/enum fields and dropped padding — which silently broke e.g. `Duration { TimeSpan; Type }`
/// (a 0-tick / instant animation) and `GridLength { Value; GridUnitType }`.
#[cfg(feature = "classic")]
pub(crate) fn append_struct_object_bytes(
    buf: &mut Vec<u8>,
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    sd: &StructDeclaration,
) {
    let start = buf.len();
    let mut max_align = 1usize;
    for f in sd.fields().iter() {
        let m = match f.base().metadata() {
            Some(m) => m,
            None => continue,
        };
        let ts = Signature::to_string(m, &f.type_());
        let fname = f.name().to_string();
        let fv = v8::String::new(scope, fname.as_str())
            .and_then(|k| obj.get(scope, k.into()))
            .unwrap_or_else(|| v8::undefined(scope).into());

        let (fsize, falign) = sig_size_align(&ts);
        if falign > max_align {
            max_align = falign;
        }
        let pad = align_up(buf.len(), falign) - buf.len();
        buf.extend(std::iter::repeat(0u8).take(pad));

        let is_struct = ts.contains('.')
            && MetadataReader::find_by_name(strip_generic_suffix(&ts))
                .map(|d| d.read().kind() == DeclarationKind::Struct)
                .unwrap_or(false);
        if is_struct {
            let lookup = strip_generic_suffix(&ts);
            if !fv.is_null_or_undefined() && fv.is_object() {
                if let (Some(decl), Some(fobj)) =
                    (MetadataReader::find_by_name(lookup), fv.to_object(scope))
                {
                    let lock = decl.read();
                    if let Some(nsd) = lock.as_any().downcast_ref::<StructDeclaration>() {
                        append_struct_object_bytes(buf, scope, fobj, nsd);
                        continue;
                    }
                }
            }
            buf.extend(std::iter::repeat(0u8).take(fsize));
            continue;
        }

        // Enum fields are dotted but Int32-valued; primitives use their resolved native type.
        let nt = if ts.contains('.') {
            NativeType::I32
        } else {
            NativeType::try_from(ts.as_str()).unwrap_or(NativeType::I32)
        };
        append_struct_field_bytes(buf, scope, fv, &nt);
    }
    let total = buf.len() - start;
    let tail = align_up(total, max_align) - total;
    buf.extend(std::iter::repeat(0u8).take(tail));
}

/// Serializes a WinRT struct's fields into `buf` off a napi object, using the same layout rules
/// as `append_struct_object_bytes` (field alignment, nested structs, enum-as-Int32, trailing
/// padding) so the two backends agree on the wire format.
#[cfg(feature = "napi_engine")]
pub(crate) fn append_struct_object_bytes_napi(
    env: &napi::Env,
    buf: &mut Vec<u8>,
    obj: &napi::JsObject,
    sd: &StructDeclaration,
) {
    use napi::{JsObject, JsUnknown, ValueType};

    let start = buf.len();
    let mut max_align = 1usize;
    for f in sd.fields().iter() {
        let m = match f.base().metadata() {
            Some(m) => m,
            None => continue,
        };
        let ts = Signature::to_string(m, &f.type_());
        let fname = f.name().to_string();
        let fv: Option<JsUnknown> = obj.get_named_property::<JsUnknown>(fname.as_str()).ok();

        let (fsize, falign) = sig_size_align(&ts);
        if falign > max_align {
            max_align = falign;
        }
        let pad = align_up(buf.len(), falign) - buf.len();
        buf.extend(std::iter::repeat(0u8).take(pad));

        let is_struct = ts.contains('.')
            && MetadataReader::find_by_name(strip_generic_suffix(&ts))
                .map(|d| d.read().kind() == DeclarationKind::Struct)
                .unwrap_or(false);
        if is_struct {
            let lookup = strip_generic_suffix(&ts);
            let field_obj = fv.as_ref().and_then(|v| match v.get_type() {
                Ok(ValueType::Object) => Some(unsafe { v.cast::<JsObject>() }),
                _ => None,
            });
            if let (Some(decl), Some(fobj)) = (MetadataReader::find_by_name(lookup), field_obj) {
                let lock = decl.read();
                if let Some(nsd) = lock.as_any().downcast_ref::<StructDeclaration>() {
                    append_struct_object_bytes_napi(env, buf, &fobj, nsd);
                    continue;
                }
            }
            buf.extend(std::iter::repeat(0u8).take(fsize));
            continue;
        }

        // Enum fields are dotted but Int32-valued; primitives use their resolved native type.
        let nt = if ts.contains('.') {
            NativeType::I32
        } else {
            NativeType::try_from(ts.as_str()).unwrap_or(NativeType::I32)
        };
        match fv {
            Some(v) => crate::napi_engine::value::append_struct_field_bytes(env, buf, &v, &nt),
            None => buf.extend(std::iter::repeat(0u8).take(nt.size())),
        }
    }
    let total = buf.len() - start;
    let tail = align_up(total, max_align) - total;
    buf.extend(std::iter::repeat(0u8).take(tail));
}

pub(crate) fn substitute_type_vars(s: &str, type_args: &[String]) -> String {
    if type_args.is_empty() {
        return s.to_string();
    }
    let mut result = s.to_string();
    for (i, arg) in type_args.iter().enumerate() {
        result = result.replace(&format!("Var!{}", i), arg.as_str());
    }
    result
}

/// Immutable per-property-method-type data cached after first construction.
/// Class path key: (method_token as u64) << 1 | (is_initializer as u64).
/// Interface path key: (token, declaring IID, is_setter) — the IID disambiguates
/// generic interface instantiations that share metadata tokens.
struct PropertyStaticInfo {
    cif: Rc<Cif>,
    iid: GUID,
    /// Vtable slot index. The function pointer is re-read from the QI'd
    /// interface's vtable per construction — implementations differ per class
    /// even when the metadata token is shared (interface-declared members).
    index: usize,
    parameter_types: Vec<NativeType>,
    parse_parameter_types: Vec<NativeType>,
    parameters: Vec<ParameterDeclaration>,
    return_type: String,
    is_void: bool,
    is_sealed: bool,
    declaration: Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>>,
    number_of_parameters: usize,
    number_of_abi_parameters: usize,
    return_kind: ReturnKind,
    param_sigs: Vec<String>,
    type_args: Vec<String>,
    /// Per-parameter marshaling plan, aligned with `parse_parameter_types` (see
    /// `method_call::PointerPlan`). Only consulted for in-params parsed as `Pointer`.
    param_plans: Vec<crate::method_call::PointerPlan>,
    /// Return-slot classification, resolved once: scalar and string returns are written into
    /// the call's stable return buffer instead of the pointer slot.
    is_scalar_return: bool,
    is_string_return: bool,
}

/// Resolve the Pointer-parameter marshaling plans for a property/interface call's static info.
/// PropertyCall never applied the TypeName synthesis, so `typename_special` is false.
fn pointer_plans_for(
    parse_parameter_types: &[NativeType],
    parameters: &[ParameterDeclaration],
    param_sigs: &[String],
    type_args: &[String],
) -> Vec<crate::method_call::PointerPlan> {
    use crate::method_call::PointerPlan;
    parse_parameter_types
        .iter()
        .zip(parameters.iter())
        .zip(param_sigs.iter())
        .map(|((nt, parameter), sig)| {
            let is_out = parameter.is_out() || sig.starts_with("ByRef ");
            if !is_out && matches!(nt, NativeType::Pointer) {
                PointerPlan::for_parameter(sig, parameter, type_args, false)
            } else {
                PointerPlan::Plain
            }
        })
        .collect()
}

thread_local! {
    // Keyed by (metadata scope, token | flags): tokens are only unique within one .winmd scope
    // (see METHOD_STATIC_INFO_CACHE in method_call.rs for the observed cross-winmd collision).
    static PROPERTY_STATIC_INFO_CACHE: RefCell<ahash::AHashMap<(usize, u64), Rc<PropertyStaticInfo>>>
        = RefCell::new(ahash::AHashMap::new());
    /// Cache for the interface-routed constructors (generic vectors, maps, …).
    /// Keyed by (method token | is_setter bit, declaring interface IID).
    static INTERFACE_STATIC_INFO_CACHE: RefCell<ahash::AHashMap<(u64, u128), Rc<PropertyStaticInfo>>>
        = RefCell::new(ahash::AHashMap::new());
}

pub struct PropertyCall {
    si: Rc<PropertyStaticInfo>,
    is_initializer: bool,
    is_setter: bool,
    /// Original (pre-QI) interface, kept alive for the duration of the call object when this
    /// call did its own QueryInterface; `None` when the interface is borrowed.
    #[allow(dead_code)]
    parent_interface: Option<IUnknown>,
    /// The interface the call goes through, already QI'd to `si.iid`. Owned (released on drop)
    /// when this call object did the QueryInterface itself; borrowed from a `QiCache` or a
    /// [`PreparedProperty`] otherwise, whose owner outlives the call.
    interface: ManuallyDrop<IUnknown>,
    owns_interface: bool,
    func: *mut c_void,
    /// Argument slots for the call; inline storage covers the common arities, so building a
    /// call does not allocate.
    argument_buf: SmallVec<[NativeValue; 8]>,
    /// Parse type per argument slot, aligned with `argument_buf`.
    argument_parse_types: SmallVec<[Option<NativeType>; 8]>,
    /// Scratch buffer used when a WinRT property/method returns a value type
    /// or scalar that must be written into caller-owned storage.
    return_value_buf: [u8; 128],
}

impl PropertyCall {
    /// Drop the previous call's argument state, releasing the HSTRINGs built for `String`
    /// in-parameters first (the union slots would otherwise leak them).
    fn reset_args(&mut self) {
        unsafe { crate::ffi::release_string_args(&mut self.argument_buf, &self.argument_parse_types) };
        self.argument_buf.clear();
        self.argument_parse_types.clear();
    }
}

impl Drop for PropertyCall {
    fn drop(&mut self) {
        self.reset_args();
        if self.owns_interface {
            unsafe { ManuallyDrop::drop(&mut self.interface) };
        }
    }
}

/// A property accessor resolved once on a fixed target (see `method_call::PreparedCall`): the
/// target's interface pointer already QI'd to the accessor's interface plus its vtable slot.
/// Static property call sites keep one per accessor on the class activation factory.
#[derive(Clone)]
pub(crate) struct PreparedProperty {
    si: Rc<PropertyStaticInfo>,
    interface: IUnknown,
    func: *mut c_void,
    is_setter: bool,
    is_initializer: bool,
}

impl PropertyCall {
    /// Static info for an accessor already seen on this thread, if any.
    fn cached_static_info(
        property: &PropertyDeclaration,
        is_setter: bool,
        is_initializer: bool,
    ) -> Option<Rc<PropertyStaticInfo>> {
        let method = if is_setter {
            property.setter()?
        } else {
            property.getter()
        };
        let scope_key = method
            .metadata()
            .map(|m| windows::core::Interface::as_raw(m) as usize)
            .unwrap_or(0);
        let cache_key = (
            scope_key,
            ((method.token().0 as u64) << 1) | (is_initializer as u64),
        );
        PROPERTY_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned())
    }

    /// A call over an interface pointer that some longer-lived owner holds: no QueryInterface
    /// and no refcount traffic. The vtable slot is still read per call object (interface-declared
    /// accessors share a metadata token across implementing classes).
    fn from_borrowed(
        si: Rc<PropertyStaticInfo>,
        is_setter: bool,
        is_initializer: bool,
        interface_ptr: *mut c_void,
    ) -> Self {
        let vtable = unsafe { *(interface_ptr as *const *const *mut c_void) };
        let func = unsafe { *vtable.add(si.index) };
        Self {
            si,
            is_initializer,
            is_setter,
            parent_interface: None,
            interface: ManuallyDrop::new(unsafe { IUnknown::from_raw(interface_ptr) }),
            owns_interface: false,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        }
    }

    /// Build an accessor call on `instance` through the wrapper's `qi` cache, so repeated
    /// property reads and writes on one wrapper skip the per-call QueryInterface. The first
    /// access of an accessor on the thread still goes through [`PropertyCall::new`] to build
    /// its static info. `None` when the instance does not implement the accessor's interface.
    pub(crate) fn new_cached(
        property: &PropertyDeclaration,
        is_setter: bool,
        instance: &IUnknown,
        qi: &QiCache,
    ) -> Option<Self> {
        let Some(si) = Self::cached_static_info(property, is_setter, false) else {
            return Self::new(property, is_setter, instance.clone(), false);
        };
        let ptr = qi.interface(instance, &si.iid)?;
        Some(Self::from_borrowed(si, is_setter, false, ptr))
    }

    /// Resolve `property`'s getter or setter on `target` once for reuse through
    /// [`PropertyCall::from_prepared`]. `None` when the target does not implement it.
    pub(crate) fn prepare(
        property: &PropertyDeclaration,
        is_setter: bool,
        target: IUnknown,
    ) -> Option<PreparedProperty> {
        let mut call = Self::new(property, is_setter, target, false)?;
        // Move the interface out; the call object must not release it on drop.
        call.owns_interface = false;
        let interface = unsafe { ManuallyDrop::take(&mut call.interface) };
        Some(PreparedProperty {
            si: Rc::clone(&call.si),
            interface,
            func: call.func,
            is_setter: call.is_setter,
            is_initializer: call.is_initializer,
        })
    }

    /// A call built from a [`PreparedProperty`]: borrows its interface pointer and vtable slot.
    pub(crate) fn from_prepared(prepared: &PreparedProperty) -> Self {
        Self {
            si: Rc::clone(&prepared.si),
            is_initializer: prepared.is_initializer,
            is_setter: prepared.is_setter,
            parent_interface: None,
            interface: ManuallyDrop::new(unsafe {
                IUnknown::from_raw(prepared.interface.as_raw())
            }),
            owns_interface: false,
            func: prepared.func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        }
    }
}

#[inline]
fn call_failure() -> HRESULT {
    HRESULT(0x8000_4005u32 as i32)
}

impl PropertyCall {
    pub fn is_void(&self) -> bool {
        self.si.is_void
    }

    pub fn return_type(&self) -> &str {
        self.si.return_type.as_str()
    }

    pub(crate) fn return_kind(&self) -> &ReturnKind {
        &self.si.return_kind
    }

    #[cfg(feature = "classic")]
    pub fn parse_types_debug(&self) -> &[NativeType] {
        &self.si.parse_parameter_types
    }

    #[cfg(feature = "classic")]
    pub fn abi_types_debug(&self) -> &[NativeType] {
        &self.si.parameter_types
    }

    fn from_static_info(
        si: Rc<PropertyStaticInfo>,
        interface: IUnknown,
        is_setter: bool,
        is_initializer: bool,
    ) -> Option<Self> {
        let vtable = interface.vtable();
        let mut interface_ptr: *mut c_void = std::ptr::null_mut();
        let result = unsafe {
            ((*vtable).QueryInterface)(
                interface.as_raw(),
                &si.iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };
        if result.is_err() || interface_ptr.is_null() {
            return None;
        }
        let queried_interface = unsafe { IUnknown::from_raw(interface_ptr) };
        let vtable_ptr: *mut *mut c_void =
            unsafe { std::mem::transmute(queried_interface.vtable()) };
        let func = unsafe { *vtable_ptr.add(si.index) };
        Some(Self {
            si,
            is_initializer,
            is_setter,
            parent_interface: Some(interface),
            interface: ManuallyDrop::new(queried_interface),
            owns_interface: true,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        })
    }

    pub fn new(
        property: &PropertyDeclaration,
        is_setter: bool,
        interface: IUnknown,
        is_initializer: bool,
    ) -> Option<Self> {
        let method = if is_setter {
            property.setter().unwrap()
        } else {
            property.getter()
        };

        // Fast path: reuse cached static info; only the QI is per-instance work.
        let scope_key = method
            .metadata()
            .map(|m| windows::core::Interface::as_raw(m) as usize)
            .unwrap_or(0);
        let cache_key = (
            scope_key,
            ((method.token().0 as u64) << 1) | (is_initializer as u64),
        );
        if let Some(si) = PROPERTY_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned()) {
            return Self::from_static_info(si, interface, is_setter, is_initializer);
        }

        let number_of_parameters = method.number_of_parameters();

        let mut index = 0 as usize;

        let mut declaration: Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>> = None;

        let iid = match Metadata::find_declaring_interface_for_method(method, &mut index) {
            None => {
                if let Some(metadata) = method.metadata() {
                    let containing_type = CorTokenType(
                        Metadata::get_method_containing_class_token(metadata, method.token())
                            as i32,
                    );

                    if containing_type.0 != 0 {
                        let interface_declaration = Arc::new(RwLock::new(
                            InterfaceDeclaration::new(Some(metadata), containing_type),
                        ));
                        index =
                            Metadata::find_method_index(metadata, containing_type, method.token());
                        let iid = interface_declaration.read().id();
                        declaration = Some(interface_declaration);
                        iid
                    } else {
                        index = 0;
                        IActivationFactory::IID
                    }
                } else {
                    index = 0;
                    IActivationFactory::IID
                }
            }
            Some(interface) => {
                let iid;
                {
                    let ii_lock = interface.read();

                    let kind = ii_lock.base().kind();

                    match kind {
                        DeclarationKind::GenericInterfaceInstance => {
                            let ii = ii_lock
                                .as_declaration()
                                .as_any()
                                .downcast_ref::<GenericInterfaceInstanceDeclaration>()?;
                            iid = ii.id();
                        }
                        _ => {
                            let ii = ii_lock
                                .as_declaration()
                                .as_any()
                                .downcast_ref::<InterfaceDeclaration>()?;
                            iid = ii.id();
                        }
                    }
                }
                declaration = Some(interface);
                iid
            }
        };

        let _pre_index = index;

        let vtable = interface.vtable();

        let mut interface_ptr: *mut c_void = std::ptr::null_mut();

        let result = unsafe {
            ((*vtable).QueryInterface)(
                interface.as_raw(),
                &iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };

        if result.is_err() || interface_ptr.is_null() {
            return None;
        }

        let is_sealed = method.is_sealed();

        let is_composition = !is_sealed;

        let is_void = method.is_void();

        let signature = method.return_type();

        let return_type = Signature::to_string(method.metadata()?, &signature);

        let other_params: usize = if is_initializer {
            if is_sealed {
                2
            } else {
                4
            }
        } else {
            if is_void {
                1
            } else {
                2
            }
        };

        let mut parameter_types: Vec<NativeType> =
            Vec::with_capacity(number_of_parameters + other_params + 4);
        let mut parse_parameter_types: Vec<NativeType> = Vec::with_capacity(number_of_parameters);
        let mut param_sigs: Vec<String> = Vec::with_capacity(number_of_parameters);
        parameter_types.push(NativeType::Pointer);

        for parameter in method.parameters().iter() {
            let type_ = parameter.type_();
            let metadata = parameter.metadata()?;

            let signature = Signature::to_string(metadata, &type_);

            let parse_native_type = NativeType::try_from(signature.as_str()).ok()?;
            parse_parameter_types.push(parse_native_type);
            param_sigs.push(signature.clone());
            if parameter.is_out() || signature.trim().starts_with("ByRef ") {
                parameter_types.push(NativeType::Pointer);
            } else {
                let abi_native = crate::helpers::struct_native_type_for_sig(signature.as_str())
                    .unwrap_or_else(|| ffi_native_type_from_signature(signature.as_str()));
                if matches!(abi_native, NativeType::Buffer) {
                    parameter_types.push(NativeType::U32);
                    parameter_types.push(NativeType::Buffer);
                } else {
                    parameter_types.push(abi_native);
                }
            }
        }

        if is_initializer {
            if is_composition {
                parameter_types.push(NativeType::Pointer);
                parameter_types.push(NativeType::Pointer);
            }

            parameter_types.push(NativeType::Pointer);
        } else {
            if !is_void {
                parameter_types.push(NativeType::Pointer);
            }
        }

        let number_of_abi_parameters = parameter_types.len();

        let params = parameter_types
            .iter()
            .cloned()
            .map(libffi::middle::Type::try_from)
            .collect::<std::result::Result<Vec<Type>, AnyError>>();

        let params = params.ok()?;

        let cif = Rc::new(Cif::new(params, Type::i32()));

        let parent_interface = interface.clone();

        let interface = unsafe { IUnknown::from_raw(interface_ptr as *mut c_void) };
        let vtable_struct = interface.vtable();
        let vtable_ptr: *mut *mut c_void = unsafe { std::mem::transmute(vtable_struct) };

        let mut inspectable_ptr: *mut c_void = std::ptr::null_mut();
        let supports_iinspectable = unsafe {
            ((*vtable_struct).QueryInterface)(
                interface.as_raw(),
                &IInspectable::IID,
                &mut inspectable_ptr as *mut _ as *mut *mut c_void,
            )
        };

        let base_offset = if supports_iinspectable.is_ok() && !inspectable_ptr.is_null() {
            unsafe {
                IUnknown::from_raw(inspectable_ptr as *mut c_void);
            }
            6
        } else {
            3
        };

        index = index.saturating_add(base_offset);

        let func = unsafe { *vtable_ptr.offset(index as isize) };

        let parameters = method.parameters().to_vec();
        let return_kind = crate::classify_return(&return_type, is_void);
        let param_plans =
            pointer_plans_for(&parse_parameter_types, &parameters, &param_sigs, &[]);

        let is_scalar_return = is_scalar_return_type(&return_type);
        let is_string_return = return_type == "String";
        let static_info = Rc::new(PropertyStaticInfo {
            cif,
            iid,
            index,
            parameter_types,
            parse_parameter_types,
            parameters,
            return_type,
            is_void: method.is_void(),
            is_sealed,
            declaration,
            number_of_parameters,
            number_of_abi_parameters,
            return_kind,
            param_sigs,
            type_args: Vec::new(),
            param_plans,
            is_scalar_return,
            is_string_return,
        });
        PROPERTY_STATIC_INFO_CACHE
            .with(|c| c.borrow_mut().insert(cache_key, Rc::clone(&static_info)));

        Some(Self {
            si: static_info,
            is_initializer,
            is_setter,
            parent_interface: Some(parent_interface),
            interface: ManuallyDrop::new(interface),
            owns_interface: true,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        })
    }

    /// Constructor for calling properties directly on an interface (not via a class).
    /// Uses the provided `declaring_iid` for QI and applies `type_args` substitution
    /// so that Var!N placeholders in generic interface signatures resolve correctly.
    pub fn new_for_interface(
        property: &PropertyDeclaration,
        is_setter: bool,
        interface: IUnknown,
        is_initializer: bool,
        declaring_iid: GUID,
        type_args: Vec<String>,
    ) -> Option<Self> {
        let method = if is_setter {
            property.setter().unwrap()
        } else {
            property.getter()
        };

        // Fast path: static info cached per (token, declaring IID) — generic
        // instantiations share tokens, so the IID is part of the key.
        let cache_key = (
            ((method.token().0 as u64) << 1) | (is_setter as u64),
            declaring_iid.to_u128(),
        );
        if let Some(si) = INTERFACE_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned())
        {
            return Self::from_static_info(si, interface, is_setter, is_initializer);
        }

        let number_of_parameters = method.number_of_parameters();
        let mut index = 0_usize;

        // Derive vtable index from the method's position in its containing interface.
        if let Some(metadata) = method.metadata() {
            let containing_type = CorTokenType(Metadata::get_method_containing_class_token(
                metadata,
                method.token(),
            ) as i32);
            if containing_type.0 != 0 {
                index = Metadata::find_method_index(metadata, containing_type, method.token());
            }
        }

        let vtable = interface.vtable();

        let mut interface_ptr: *mut c_void = std::ptr::null_mut();
        let result = unsafe {
            ((*vtable).QueryInterface)(
                interface.as_raw(),
                &declaring_iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };

        if result.is_err() || interface_ptr.is_null() {
            return None;
        }

        let is_sealed = method.is_sealed();
        let is_void = method.is_void();

        let signature = method.return_type();
        let raw_return_type = Signature::to_string(method.metadata()?, &signature);
        let return_type = substitute_type_vars(&raw_return_type, &type_args);
        let return_kind = crate::classify_return(&return_type, is_void);

        let other_params: usize = if is_void { 1 } else { 2 };

        let mut parameter_types: Vec<NativeType> =
            Vec::with_capacity(number_of_parameters + other_params + 2);
        let mut parse_parameter_types: Vec<NativeType> = Vec::with_capacity(number_of_parameters);
        let mut param_sigs: Vec<String> = Vec::with_capacity(number_of_parameters);
        parameter_types.push(NativeType::Pointer);

        for parameter in method.parameters().iter() {
            let type_ = parameter.type_();
            let metadata = parameter.metadata()?;
            let raw_sig = Signature::to_string(metadata, &type_);
            let signature = substitute_type_vars(&raw_sig, &type_args);

            let parse_native_type = NativeType::try_from(signature.as_str()).ok()?;
            parse_parameter_types.push(parse_native_type);
            param_sigs.push(signature.clone());
            if parameter.is_out() || signature.trim().starts_with("ByRef ") {
                parameter_types.push(NativeType::Pointer);
            } else {
                let abi_native = crate::helpers::struct_native_type_for_sig(signature.as_str())
                    .unwrap_or_else(|| {
                        crate::helpers::ffi_native_type_from_signature(signature.as_str())
                    });
                if matches!(abi_native, NativeType::Buffer) {
                    parameter_types.push(NativeType::U32);
                    parameter_types.push(NativeType::Buffer);
                } else {
                    parameter_types.push(abi_native);
                }
            }
        }

        if !is_void {
            parameter_types.push(NativeType::Pointer);
        }

        let number_of_abi_parameters = parameter_types.len();

        let params = parameter_types
            .iter()
            .cloned()
            .map(libffi::middle::Type::try_from)
            .collect::<std::result::Result<Vec<Type>, _>>();
        let params = params.ok()?;

        let cif = Rc::new(Cif::new(params, Type::i32()));

        let parent_interface = interface.clone();
        let interface = unsafe { IUnknown::from_raw(interface_ptr as *mut c_void) };
        let vtable_struct = interface.vtable();
        let vtable_ptr: *mut *mut c_void = unsafe { std::mem::transmute(vtable_struct) };

        let mut inspectable_ptr: *mut c_void = std::ptr::null_mut();
        let supports_iinspectable = unsafe {
            ((*vtable_struct).QueryInterface)(
                interface.as_raw(),
                &IInspectable::IID,
                &mut inspectable_ptr as *mut _ as *mut *mut c_void,
            )
        };

        let base_offset = if supports_iinspectable.is_ok() && !inspectable_ptr.is_null() {
            unsafe {
                IUnknown::from_raw(inspectable_ptr as *mut c_void);
            }
            6
        } else {
            3
        };

        index = index.saturating_add(base_offset);
        let func = unsafe { *vtable_ptr.offset(index as isize) };

        let parameters = method.parameters().to_vec();
        let param_plans =
            pointer_plans_for(&parse_parameter_types, &parameters, &param_sigs, &type_args);
        let is_scalar_return = is_scalar_return_type(&return_type);
        let is_string_return = return_type == "String";
        let static_info = Rc::new(PropertyStaticInfo {
            cif,
            iid: declaring_iid,
            index,
            parameter_types,
            parse_parameter_types,
            parameters,
            return_type,
            is_void,
            is_sealed,
            declaration: None,
            number_of_parameters,
            number_of_abi_parameters,
            return_kind,
            param_sigs,
            type_args,
            param_plans,
            is_scalar_return,
            is_string_return,
        });
        INTERFACE_STATIC_INFO_CACHE
            .with(|c| c.borrow_mut().insert(cache_key, Rc::clone(&static_info)));

        Some(Self {
            si: static_info,
            is_initializer,
            is_setter,
            parent_interface: Some(parent_interface),
            interface: ManuallyDrop::new(interface),
            owns_interface: true,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        })
    }

    /// Call a plain method (not a property getter/setter) directly on an interface,
    /// using `declaring_iid` for QI and substituting `type_args` for Var!N placeholders.
    pub fn new_method_for_interface(
        method: &metadata::declarations::method_declaration::MethodDeclaration,
        interface: IUnknown,
        declaring_iid: GUID,
        type_args: Vec<String>,
    ) -> Option<Self> {
        // Fast path: static info cached per (token, declaring IID).
        let cache_key = ((method.token().0 as u64) << 1, declaring_iid.to_u128());
        if let Some(si) = INTERFACE_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned())
        {
            return Self::from_static_info(si, interface, false, false);
        }

        let number_of_parameters = method.number_of_parameters();
        let mut index = 0_usize;

        if let Some(metadata) = method.metadata() {
            let containing_type = windows::Win32::System::WinRT::Metadata::CorTokenType(
                metadata::declaring_interface_for_method::Metadata::get_method_containing_class_token(metadata, method.token()) as i32,
            );
            if containing_type.0 != 0 {
                index = metadata::declaring_interface_for_method::Metadata::find_method_index(
                    metadata,
                    containing_type,
                    method.token(),
                );
            }
        }

        let vtable = interface.vtable();
        let mut interface_ptr: *mut c_void = std::ptr::null_mut();
        let result = unsafe {
            ((*vtable).QueryInterface)(
                interface.as_raw(),
                &declaring_iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };

        if result.is_err() || interface_ptr.is_null() {
            return None;
        }

        let is_sealed = method.is_sealed();
        let is_void = method.is_void();

        let signature = method.return_type();
        let raw_return_type = Signature::to_string(method.metadata()?, &signature);
        let return_type = substitute_type_vars(&raw_return_type, &type_args);
        let return_kind = crate::classify_return(&return_type, is_void);

        let other_params: usize = if is_void { 1 } else { 2 };

        let mut parameter_types: Vec<NativeType> =
            Vec::with_capacity(number_of_parameters + other_params + 2);
        let mut parse_parameter_types: Vec<NativeType> = Vec::with_capacity(number_of_parameters);
        let mut param_sigs: Vec<String> = Vec::with_capacity(number_of_parameters);
        parameter_types.push(NativeType::Pointer);

        for parameter in method.parameters().iter() {
            let type_ = parameter.type_();
            let metadata = parameter.metadata()?;
            let raw_sig = Signature::to_string(metadata, &type_);
            let sig = substitute_type_vars(&raw_sig, &type_args);
            let parse_native_type = NativeType::try_from(sig.as_str()).ok()?;
            parse_parameter_types.push(parse_native_type);
            param_sigs.push(sig.clone());
            if parameter.is_out() || sig.trim().starts_with("ByRef ") {
                parameter_types.push(NativeType::Pointer);
            } else {
                let abi_native = crate::helpers::struct_native_type_for_sig(sig.as_str())
                    .unwrap_or_else(|| {
                        crate::helpers::ffi_native_type_from_signature(sig.as_str())
                    });
                if matches!(abi_native, NativeType::Buffer) {
                    parameter_types.push(NativeType::U32);
                    parameter_types.push(NativeType::Buffer);
                } else {
                    parameter_types.push(abi_native);
                }
            }
        }

        if !is_void {
            parameter_types.push(NativeType::Pointer);
        }

        let number_of_abi_parameters = parameter_types.len();

        let params = parameter_types
            .iter()
            .cloned()
            .map(libffi::middle::Type::try_from)
            .collect::<std::result::Result<Vec<Type>, _>>();
        let params = params.ok()?;

        let cif = Rc::new(Cif::new(params, Type::i32()));

        let parent_interface = interface.clone();
        let interface = unsafe { IUnknown::from_raw(interface_ptr as *mut c_void) };
        let vtable_struct = interface.vtable();
        let vtable_ptr: *mut *mut c_void = unsafe { std::mem::transmute(vtable_struct) };

        let mut inspectable_ptr: *mut c_void = std::ptr::null_mut();
        let supports_iinspectable = unsafe {
            ((*vtable_struct).QueryInterface)(
                interface.as_raw(),
                &IInspectable::IID,
                &mut inspectable_ptr as *mut _ as *mut *mut c_void,
            )
        };

        let base_offset = if supports_iinspectable.is_ok() && !inspectable_ptr.is_null() {
            unsafe {
                IUnknown::from_raw(inspectable_ptr as *mut c_void);
            }
            6
        } else {
            3
        };

        index = index.saturating_add(base_offset);
        let func = unsafe { *vtable_ptr.offset(index as isize) };

        let parameters = method.parameters().to_vec();
        let param_plans =
            pointer_plans_for(&parse_parameter_types, &parameters, &param_sigs, &type_args);
        let is_scalar_return = is_scalar_return_type(&return_type);
        let is_string_return = return_type == "String";
        let static_info = Rc::new(PropertyStaticInfo {
            cif,
            iid: declaring_iid,
            index,
            parameter_types,
            parse_parameter_types,
            parameters,
            return_type,
            is_void,
            is_sealed,
            declaration: None,
            number_of_parameters,
            number_of_abi_parameters,
            return_kind,
            param_sigs,
            type_args,
            param_plans,
            is_scalar_return,
            is_string_return,
        });
        INTERFACE_STATIC_INFO_CACHE
            .with(|c| c.borrow_mut().insert(cache_key, Rc::clone(&static_info)));

        Some(Self {
            si: static_info,
            is_initializer: false,
            is_setter: false,
            parent_interface: Some(parent_interface),
            interface: ManuallyDrop::new(interface),
            owns_interface: true,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
        })
    }

    #[cfg(feature = "classic")]
    pub fn call<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        args: &v8::FunctionCallbackArguments,
    ) -> (HRESULT, *mut c_void, Vec<v8::Local<'s, v8::Value>>) {
        // Avoid heap allocation for the common 0-2 parameter case by using stack arrays.
        // Properties are almost always 0 params (getter) or 1 param (setter).
        match self.si.parse_parameter_types.len() {
            0 => self.call_with_values(scope, &[]),
            1 => self.call_with_values(scope, &[args.get(0)]),
            2 => self.call_with_values(scope, &[args.get(0), args.get(1)]),
            n => {
                let mut values = Vec::with_capacity(n);
                for index in 0..n {
                    values.push(args.get(index as i32));
                }
                self.call_with_values(scope, &values)
            }
        }
    }

    #[cfg(feature = "classic")]
    pub fn call_with_values<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        values: &[v8::Local<v8::Value>],
    ) -> (HRESULT, *mut c_void, Vec<v8::Local<'s, v8::Value>>) {
        let is_void = self.si.is_void;

        let is_value_type = matches!(
            self.si.return_kind,
            ReturnKind::Struct(_) | ReturnKind::Guid
        );

        let is_scalar_return = self.si.is_scalar_return;
        // HSTRING out-params must also land in a stable buffer; the local
        // `result` variable goes out of scope before the caller can read it.
        let is_string_return = self.si.is_string_return;

        self.reset_args();
        let mut queried_interfaces: SmallVec<[IUnknown; 2]> = SmallVec::new();
        let mut struct_scratch: Vec<Vec<u8>> = Vec::new();
        // param_index (usize) replaces Option<String> — references self.param_sigs[i] directly,
        // saving one String clone per out-param per call.
        let mut out_slots: Vec<(usize, NativeType, usize, Option<v8::Local<'s, v8::Object>>)> =
            Vec::new();

        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        self.argument_parse_types.push(None);

        for (i, native_type) in self.si.parse_parameter_types.iter().enumerate() {
            let value = values
                .get(i)
                .copied()
                .unwrap_or_else(|| v8::undefined(scope).into());

            let parameter = &self.si.parameters[i];
            let param_sig = &self.si.param_sigs[i];
            let is_sig_byref = param_sig.starts_with("ByRef ");
            if parameter.is_out() || is_sig_byref {
                let slot_index = self.argument_buf.len();
                let slot_size = match native_type {
                    NativeType::Struct(_) => native_type.size(),
                    NativeType::Pointer
                    | NativeType::Buffer
                    | NativeType::Function
                    | NativeType::String => std::mem::size_of::<usize>(),
                    _ => native_type.size(),
                };
                let mut buf: Vec<u8> = vec![0u8; slot_size];
                let ptr = buf.as_mut_ptr() as *mut c_void;
                struct_scratch.push(buf);
                self.argument_buf.push(NativeValue { pointer: ptr });
                self.argument_parse_types.push(None);
                let raw_init_val = value;
                let out_wrapper = try_unwrap_out_param(scope, raw_init_val);
                let (wrapper_obj, init_val) = match out_wrapper {
                    Some((obj, value)) => (Some(obj), value),
                    None => (None, raw_init_val),
                };

                // Initialize from caller-provided value if present (in/out semantics).
                if values.get(i).is_some() {
                    if !init_val.is_undefined() && !init_val.is_null() {
                        match write_v8_value_to_ptr(scope, init_val, ptr, native_type) {
                            Ok(_) => {}
                            Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                        }
                    }
                }

                out_slots.push((slot_index, native_type.clone(), i, wrapper_obj));
                continue;
            }

            let value = match *native_type {
                NativeType::Void => return (call_failure(), std::ptr::null_mut(), Vec::new()),
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
                NativeType::Pointer => {
                    // Signature classification precomputed into the per-parameter plan at
                    // static-info build time (see method_call::PointerPlan).
                    match &self.si.param_plans[i] {
                        // TypeName synthesis was never applied on the PropertyCall path
                        // (parity with the original decision tree).
                        PointerPlan::Plain | PointerPlan::TypeName => {
                            ffi_parse_pointer_arg(scope, value)
                        }
                        // IReference<T> parameters: box JS primitives with the correct Create* call
                        // so XAML receives the right typed IPropertyValue (e.g. IReference<Double>).
                        PointerPlan::IReference(inner) => {
                            if let Some(nv) = crate::value::box_as_ireference(scope, value, inner) {
                                Ok(nv)
                            } else {
                                ffi_parse_pointer_arg(scope, value)
                            }
                        }
                        PointerPlan::Struct(declaration) => {
                            // ArrayBuffer / ArrayBufferView → raw bytes pointer
                            if value.is_array_buffer() || value.is_array_buffer_view() {
                                ffi_parse_struct_arg(scope, value)
                            } else if value.is_object() {
                                let obj = value.to_object(scope).unwrap();
                                let mut sbuf: Vec<u8> = Vec::new();
                                {
                                    let lock = declaration.read();
                                    if let Some(sd) =
                                        lock.as_any().downcast_ref::<StructDeclaration>()
                                    {
                                        append_struct_object_bytes(&mut sbuf, scope, obj, sd);
                                    }
                                }
                                if sbuf.is_empty() {
                                    sbuf.push(0);
                                }
                                let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                struct_scratch.push(sbuf);
                                Ok(NativeValue { pointer: ptr })
                            } else {
                                ffi_parse_pointer_arg(scope, value)
                            }
                        }
                        // Delegate types: auto-wrap a JS function as a JsDelegate COM object,
                        // or extract the raw pointer from { handle: External } (NSWinRT.asDelegate result).
                        PointerPlan::Delegate(guid, delegate_param_types) => {
                            let handle_ptr = value.to_object(scope).and_then(|obj| {
                                let key = v8::String::new(scope, "handle")?;
                                let hv = obj.get(scope, key.into())?;
                                v8::Local::<v8::External>::try_from(hv)
                                    .ok()
                                    .map(|e| e.value())
                            });

                            if let Some(ptr) = handle_ptr {
                                Ok(NativeValue { pointer: ptr })
                            } else if let Ok(func) = v8::Local::<v8::Function>::try_from(value) {
                                use std::sync::atomic::AtomicU32;
                                let data = Box::new(crate::JsDelegateData {
                                    js_func: v8::Global::new(scope, func),
                                    param_types: delegate_param_types.clone(),
                                });
                                let delegate = Box::new(crate::JsDelegate {
                                    vtable: &crate::JS_DELEGATE_VTBL as *const _,
                                    ref_count: AtomicU32::new(1),
                                    guid: *guid,
                                    data: Box::into_raw(data),
                                });
                                Ok(NativeValue {
                                    pointer: Box::into_raw(delegate) as *mut c_void,
                                })
                            } else {
                                ffi_parse_pointer_arg(scope, value)
                            }
                        }
                        PointerPlan::Interface(iid) => {
                            // NB: a QI failure here propagates (parity with the original).
                            match ffi_parse_query_interface_arg(scope, value, iid) {
                                Ok((pointer, Some(interface_guard))) => {
                                    queried_interfaces.push(interface_guard);
                                    Ok(pointer)
                                }
                                Ok((pointer, None)) => Ok(pointer),
                                Err(error) => Err(error),
                            }
                        }
                    }
                }
                NativeType::Buffer => {
                    let parsed = ffi_parse_buffer_arg_with_length(scope, value);
                    let (buffer_value, byte_length) = match parsed {
                        Ok(value) => value,
                        Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                    };

                    self.argument_buf.push(NativeValue {
                        u32_value: byte_length,
                    });
                    self.argument_parse_types.push(Some(native_type.clone()));
                    self.argument_buf.push(buffer_value);
                    self.argument_parse_types.push(Some(native_type.clone()));
                    continue;
                }
                NativeType::Function => ffi_parse_function_arg(scope, value),
                NativeType::Struct(_) => {
                    // By-value struct param: accept a plain JS object { field: value } by serialising
                    // it to the struct's bytes (mirrors the Pointer path); else fall back to ArrayBuffer.
                    // libffi reads the value from the pointer, so the scratch buffer must outlive the call.
                    let mut handled: Option<NativeValue> = None;
                    if value.is_object()
                        && !value.is_array_buffer()
                        && !value.is_array_buffer_view()
                    {
                        let sig: &str = &self.si.param_sigs[i];
                        let lookup = crate::helpers::strip_generic_suffix(sig);
                        if let (Some(decl), Some(obj)) =
                            (MetadataReader::find_by_name(lookup), value.to_object(scope))
                        {
                            let is_struct = decl.read().kind() == DeclarationKind::Struct;
                            if is_struct {
                                let mut sbuf: Vec<u8> = Vec::new();
                                {
                                    let lock = decl.read();
                                    if let Some(sd) =
                                        lock.as_any().downcast_ref::<StructDeclaration>()
                                    {
                                        append_struct_object_bytes(&mut sbuf, scope, obj, sd);
                                    }
                                }
                                if sbuf.is_empty() {
                                    sbuf.push(0);
                                }
                                let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                struct_scratch.push(sbuf);
                                handled = Some(NativeValue { pointer: ptr });
                            }
                        }
                    }
                    match handled {
                        Some(nv) => Ok(nv),
                        None => ffi_parse_struct_arg(scope, value),
                    }
                }
                NativeType::String => ffi_parse_string_arg(scope, value),
            };

            let value = match value {
                Ok(value) => value,
                Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
            };

            self.argument_buf.push(value);
            self.argument_parse_types.push(Some(native_type.clone()));
        }

        let mut result: *mut c_void = std::ptr::null_mut();

        if !self.is_initializer && !is_void {
            if is_value_type || is_scalar_return || is_string_return {
                let buf_ptr = self.return_value_buf.as_mut_ptr() as *mut c_void;
                self.argument_buf.push(NativeValue { pointer: buf_ptr });
                self.argument_parse_types.push(None);
            } else {
                self.argument_buf.push(NativeValue {
                    pointer: &mut result as *mut _ as *mut c_void,
                });
                self.argument_parse_types.push(None);
            }
        }

        let prep = match crate::ffi::prepare_string_storage(
            &self.argument_buf,
            &self.si.parameter_types,
            &self.argument_parse_types,
        ) {
            Ok(prep) => prep,
            Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
        };
        let call_args =
            crate::ffi::build_call_args(&prep, &self.argument_buf, &self.si.parameter_types);

        let ret = match catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si.cif.call(CodePtr::from_ptr(self.func), &call_args)
        })) {
            Ok(code) => code,
            Err(_) => {
                let msg =
                    format!("WinRT property call panicked during invocation: returning E_FAIL");
                crate::store_last_js_error(msg);
                return (call_failure(), std::ptr::null_mut(), Vec::new());
            }
        };

        // Detect RPC_E_WRONG_THREAD and surface the canonical OS message to
        // JS/tests so embedders can catch it directly.
        let hr = HRESULT(ret);
        const RPC_E_WRONG_THREAD: u32 = 0x8001010E;
        if (hr.0 as u32) == RPC_E_WRONG_THREAD {
            let msg = crate::error::format_hresult_message(hr);
            crate::store_last_js_error(msg.clone());
            if let Some(vmstr) = v8::String::new(scope, &msg) {
                let err = v8::Exception::error(scope, vmstr);
                scope.throw_exception(err);
            }
        }

        if !self.is_initializer
            && !is_void
            && (is_value_type || is_scalar_return || is_string_return)
        {
            result = self.return_value_buf.as_mut_ptr() as *mut c_void;
        }

        // Marshal out-parameters back into V8 values using the recorded slots.
        let mut out_values: Vec<v8::Local<'s, v8::Value>> = Vec::new();
        for (slot_index, parse_native_type, param_idx, wrapper_obj) in out_slots.into_iter() {
            let storage_ptr = unsafe {
                self.argument_buf
                    .get(slot_index)
                    .map(|v| v.pointer)
                    .unwrap_or(std::ptr::null_mut())
            };
            if storage_ptr.is_null() {
                let v: v8::Local<v8::Value> = v8::null(scope).into();
                if let Some(wrapper) = wrapper_obj {
                    let _ = set_out_param_value(scope, wrapper, v);
                } else {
                    out_values.push(v);
                }
                continue;
            }
            // Borrow the pre-cached signature string — no String allocation needed.
            let sig = &self.si.param_sigs[param_idx];
            unsafe {
                let v = match parse_native_type {
                    NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                        let inner =
                            std::ptr::read_unaligned(storage_ptr as *const usize) as *mut c_void;
                        if inner.is_null() {
                            v8::null(scope).into()
                        } else if !sig.is_empty() && sig.contains('.') {
                            let mut lookup = sig.as_str();
                            if let Some(stripped) = lookup.strip_prefix("ByRef ") {
                                lookup = stripped;
                            }
                            let lookup = strip_generic_suffix(lookup);
                            if let Some(declaration) = MetadataReader::find_by_name(lookup) {
                                if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                    crate::create_struct_object_from_raw(declaration, inner, scope)
                                        .into()
                                } else {
                                    let instance = unsafe { IUnknown::from_raw(inner) };
                                    crate::ns_proxy::create_ns_ctor_instance_object(
                                        sig.as_str(),
                                        None,
                                        None,
                                        declaration,
                                        Some(instance),
                                        scope,
                                    )
                                    .into()
                                }
                            } else {
                                read_value_from_ptr(
                                    inner as *const c_void,
                                    scope,
                                    NativeType::Pointer,
                                )
                            }
                        } else {
                            read_value_from_ptr(inner as *const c_void, scope, NativeType::Pointer)
                        }
                    }
                    _ => read_value_from_ptr(
                        storage_ptr as *const c_void,
                        scope,
                        parse_native_type.clone(),
                    ),
                };
                if let Some(wrapper) = wrapper_obj {
                    let _ = set_out_param_value(scope, wrapper, v);
                } else {
                    out_values.push(v);
                }
            }
        }

        (HRESULT(ret), result, out_values)
    }
}

/// Node-API entry point for invoking a WinRT property call: same pipeline and error contract as
/// `PropertyCall::call_with_values`, but JS values arrive as napi handles instead of V8 values.
/// Kept adjacent to that implementation since it shares the same private fields.
#[cfg(feature = "napi_engine")]
impl PropertyCall {
    pub fn call_napi(
        &mut self,
        env: &napi::Env,
        values: &[napi::JsUnknown],
    ) -> (HRESULT, *mut c_void, Vec<napi::JsUnknown>) {
        use crate::napi_engine::delegate::make_napi_delegate;
        use crate::napi_engine::value as nv;
        use napi::{JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue, ValueType};

        #[inline]
        fn dup_arg(env: &napi::Env, v: &JsUnknown) -> JsUnknown {
            unsafe { JsUnknown::from_raw_unchecked(env.raw(), v.raw()) }
        }
        #[inline]
        fn fail3() -> (HRESULT, *mut c_void, Vec<napi::JsUnknown>) {
            (call_failure(), std::ptr::null_mut(), Vec::new())
        }

        let is_void = self.si.is_void;
        let is_value_type = matches!(
            self.si.return_kind,
            ReturnKind::Struct(_) | ReturnKind::Guid
        );
        let is_scalar_return = self.si.is_scalar_return;
        let is_string_return = self.si.is_string_return;

        self.reset_args();
        let mut queried_interfaces: SmallVec<[IUnknown; 2]> = SmallVec::new();
        let mut struct_scratch: Vec<Vec<u8>> = Vec::new();
        let mut out_slots: Vec<(usize, NativeType, usize, Option<napi::JsObject>)> = Vec::new();

        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        self.argument_parse_types.push(None);

        for (i, native_type) in self.si.parse_parameter_types.iter().enumerate() {
            let value = match values.get(i) {
                Some(v) => dup_arg(env, v),
                None => match env.get_undefined() {
                    Ok(u) => unsafe { JsUnknown::from_raw_unchecked(env.raw(), u.raw()) },
                    Err(_) => return fail3(),
                },
            };
            let vt = value.get_type().unwrap_or(ValueType::Undefined);

            let parameter = &self.si.parameters[i];
            let param_sig = &self.si.param_sigs[i];
            let is_sig_byref = param_sig.starts_with("ByRef ");
            if parameter.is_out() || is_sig_byref {
                let slot_index = self.argument_buf.len();
                let slot_size = match native_type {
                    NativeType::Struct(_) => native_type.size(),
                    NativeType::Pointer
                    | NativeType::Buffer
                    | NativeType::Function
                    | NativeType::String => std::mem::size_of::<usize>(),
                    _ => native_type.size(),
                };
                let mut buf: Vec<u8> = vec![0u8; slot_size];
                let ptr = buf.as_mut_ptr() as *mut c_void;
                struct_scratch.push(buf);
                self.argument_buf.push(NativeValue { pointer: ptr });
                self.argument_parse_types.push(None);

                let (wrapper_obj, init_val) = match nv::try_unwrap_out_param(env, &value) {
                    Some((obj, inner)) => (Some(obj), inner),
                    None => (None, value),
                };

                if values.get(i).is_some() {
                    let ivt = init_val.get_type().unwrap_or(ValueType::Undefined);
                    if ivt != ValueType::Undefined && ivt != ValueType::Null {
                        match nv::write_js_value_to_ptr(env, &init_val, ptr, native_type) {
                            Ok(_) => {}
                            Err(_) => return fail3(),
                        }
                    }
                }

                out_slots.push((slot_index, native_type.clone(), i, wrapper_obj));
                continue;
            }

            let value = match *native_type {
                NativeType::Void => return fail3(),
                NativeType::Bool => nv::napi_parse_bool(&value),
                NativeType::U8 => nv::napi_parse_u8(&value),
                NativeType::I8 => nv::napi_parse_i8(&value),
                NativeType::U16 => nv::napi_parse_u16(&value),
                NativeType::I16 => nv::napi_parse_i16(&value),
                NativeType::U32 => nv::napi_parse_u32(&value),
                NativeType::I32 => nv::napi_parse_i32(&value),
                NativeType::U64 => nv::napi_parse_u64(&value),
                NativeType::I64 => nv::napi_parse_i64(&value),
                NativeType::USize => nv::napi_parse_usize(&value),
                NativeType::ISize => nv::napi_parse_isize(&value),
                NativeType::F32 => nv::napi_parse_f32(&value),
                NativeType::F64 => nv::napi_parse_f64(&value),
                NativeType::Pointer => {
                    // Classification precomputed into the per-parameter plan at static-info
                    // build time (see method_call::PointerPlan).
                    match &self.si.param_plans[i] {
                        // TypeName is never emitted for PropertyCall (no synthesis on this
                        // path — parity with the v8 original).
                        PointerPlan::Plain | PointerPlan::TypeName => {
                            nv::napi_parse_pointer(env, &value)
                        }
                        PointerPlan::IReference(inner) => {
                            if let Some(nvv) = nv::box_as_ireference(env, &value, inner) {
                                Ok(nvv)
                            } else {
                                nv::napi_parse_pointer(env, &value)
                            }
                        }
                        PointerPlan::Struct(declaration) => {
                            // ArrayBuffer/view → raw bytes; other objects → serialize
                            // field-by-field (v8 original used is_array_buffer checks).
                            let is_buffer_like = nv::napi_parse_struct(env, &value).is_ok();
                            if is_buffer_like {
                                nv::napi_parse_struct(env, &value)
                            } else if vt == ValueType::Object {
                                let obj: JsObject = unsafe { value.cast() };
                                let mut sbuf: Vec<u8> = Vec::new();
                                {
                                    let lock = declaration.read();
                                    if let Some(sd) =
                                        lock.as_any().downcast_ref::<StructDeclaration>()
                                    {
                                        append_struct_object_bytes_napi(env, &mut sbuf, &obj, sd);
                                    }
                                }
                                if sbuf.is_empty() {
                                    sbuf.push(0);
                                }
                                let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                struct_scratch.push(sbuf);
                                Ok(NativeValue { pointer: ptr })
                            } else {
                                nv::napi_parse_pointer(env, &value)
                            }
                        }
                        PointerPlan::Delegate(guid, delegate_param_types) => {
                            let handle_ptr = if vt == ValueType::Object {
                                let obj: JsObject = unsafe { value.cast() };
                                obj.get_named_property::<JsUnknown>("handle")
                                    .ok()
                                    .and_then(|hv| nv::ptr_from_external(env, &hv))
                            } else {
                                None
                            };

                            if let Some(ptr) = handle_ptr {
                                Ok(NativeValue { pointer: ptr })
                            } else if vt == ValueType::Function {
                                let func: JsFunction = unsafe { value.cast() };
                                match make_napi_delegate(
                                    env,
                                    &func,
                                    *guid,
                                    delegate_param_types.clone(),
                                ) {
                                    Some(ptr) => Ok(NativeValue { pointer: ptr }),
                                    None => nv::napi_parse_pointer(env, &value),
                                }
                            } else {
                                nv::napi_parse_pointer(env, &value)
                            }
                        }
                        PointerPlan::Interface(iid) => {
                            // NB: unlike call_napi in method_call.rs, a QI failure
                            // here propagates (matches the v8 original).
                            match nv::napi_parse_query_interface(env, &value, iid) {
                                Ok((pointer, Some(interface_guard))) => {
                                    queried_interfaces.push(interface_guard);
                                    Ok(pointer)
                                }
                                Ok((pointer, None)) => Ok(pointer),
                                Err(error) => Err(error),
                            }
                        }
                    }
                }
                NativeType::Buffer => {
                    let parsed = nv::napi_parse_buffer_with_length(env, &value);
                    let (buffer_value, byte_length) = match parsed {
                        Ok(value) => value,
                        Err(_) => return fail3(),
                    };
                    self.argument_buf.push(NativeValue {
                        u32_value: byte_length,
                    });
                    self.argument_parse_types.push(Some(native_type.clone()));
                    self.argument_buf.push(buffer_value);
                    self.argument_parse_types.push(Some(native_type.clone()));
                    continue;
                }
                NativeType::Function => nv::napi_parse_function(env, &value),
                NativeType::Struct(_) => {
                    // By-value struct: plain JS object → serialized bytes (scratch outlives
                    // the call); ArrayBuffer/view or wrapped instances → struct parse.
                    let mut handled: Option<NativeValue> = None;
                    let is_buffer_like = nv::napi_parse_struct(env, &value).is_ok();
                    if vt == ValueType::Object && !is_buffer_like {
                        let sig: &str = &self.si.param_sigs[i];
                        let lookup = crate::helpers::strip_generic_suffix(sig);
                        if let Some(decl) = MetadataReader::find_by_name(lookup) {
                            let is_struct = decl.read().kind() == DeclarationKind::Struct;
                            if is_struct {
                                let obj: JsObject = unsafe { value.cast() };
                                let mut sbuf: Vec<u8> = Vec::new();
                                {
                                    let lock = decl.read();
                                    if let Some(sd) =
                                        lock.as_any().downcast_ref::<StructDeclaration>()
                                    {
                                        append_struct_object_bytes_napi(env, &mut sbuf, &obj, sd);
                                    }
                                }
                                if sbuf.is_empty() {
                                    sbuf.push(0);
                                }
                                let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                struct_scratch.push(sbuf);
                                handled = Some(NativeValue { pointer: ptr });
                            }
                        }
                    }
                    match handled {
                        Some(nvv) => Ok(nvv),
                        None => nv::napi_parse_struct(env, &value),
                    }
                }
                NativeType::String => nv::napi_parse_string(&value),
            };

            let value = match value {
                Ok(value) => value,
                Err(_) => return fail3(),
            };

            self.argument_buf.push(value);
            self.argument_parse_types.push(Some(native_type.clone()));
        }

        let mut result: *mut c_void = std::ptr::null_mut();

        if !self.is_initializer && !is_void {
            if is_value_type || is_scalar_return || is_string_return {
                let buf_ptr = self.return_value_buf.as_mut_ptr() as *mut c_void;
                self.argument_buf.push(NativeValue { pointer: buf_ptr });
                self.argument_parse_types.push(None);
            } else {
                self.argument_buf.push(NativeValue {
                    pointer: &mut result as *mut _ as *mut c_void,
                });
                self.argument_parse_types.push(None);
            }
        }

        let prep = match crate::ffi::prepare_string_storage(
            &self.argument_buf,
            &self.si.parameter_types,
            &self.argument_parse_types,
        ) {
            Ok(prep) => prep,
            Err(_) => return fail3(),
        };
        let call_args =
            crate::ffi::build_call_args(&prep, &self.argument_buf, &self.si.parameter_types);

        let ret = match catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si.cif.call(CodePtr::from_ptr(self.func), &call_args)
        })) {
            Ok(code) => code,
            Err(_) => {
                let msg =
                    format!("WinRT property call panicked during invocation: returning E_FAIL");
                crate::store_last_js_error(msg);
                return fail3();
            }
        };

        let hr = HRESULT(ret);
        const RPC_E_WRONG_THREAD: u32 = 0x8001010E;
        if (hr.0 as u32) == RPC_E_WRONG_THREAD {
            let msg = crate::error::format_hresult_message(hr);
            crate::store_last_js_error(msg.clone());
            nv::throw_js_error(env, &msg);
        }

        if !self.is_initializer
            && !is_void
            && (is_value_type || is_scalar_return || is_string_return)
        {
            result = self.return_value_buf.as_mut_ptr() as *mut c_void;
        }

        // Marshal out-parameters back into JS values.
        // TODO(ns_proxy port): dotted-sig out params get typed wrappers
        // (create_struct_object_from_raw / create_ns_ctor_instance_object) once ported;
        // external fallback until then. Plain sigs keep exact v8 read parity.
        let mut out_values: Vec<napi::JsUnknown> = Vec::new();
        for (slot_index, parse_native_type, param_idx, wrapper_obj) in out_slots.into_iter() {
            let storage_ptr = unsafe {
                self.argument_buf
                    .get(slot_index)
                    .map(|v| v.pointer)
                    .unwrap_or(std::ptr::null_mut())
            };
            if storage_ptr.is_null() {
                let Ok(null_js) = env.get_null() else {
                    return fail3();
                };
                let v = unsafe { JsUnknown::from_raw_unchecked(env.raw(), null_js.raw()) };
                if let Some(mut wrapper) = wrapper_obj {
                    let _ = nv::set_out_param_value(&mut wrapper, v);
                } else {
                    out_values.push(v);
                }
                continue;
            }
            let sig = &self.si.param_sigs[param_idx];
            let v = unsafe {
                match parse_native_type {
                    NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                        let inner =
                            std::ptr::read_unaligned(storage_ptr as *const usize) as *mut c_void;
                        if inner.is_null() {
                            match env.get_null() {
                                Ok(n) => JsUnknown::from_raw_unchecked(env.raw(), n.raw()),
                                Err(_) => return fail3(),
                            }
                        } else if !sig.is_empty() && sig.contains('.') {
                            // Classes resolve to typed proxies; structs stay externals.
                            match crate::napi_engine::ns_proxy::try_wrap_inspectable_pointer(
                                env, inner,
                            ) {
                                Some(p) => {
                                    // The wrapper holds its own reference; drop the +1 the
                                    // callee wrote into the out slot.
                                    IUnknown::from_raw(inner);
                                    crate::napi_engine::value::as_unknown(env, p)
                                }
                                None => match nv::read_return_value(
                                    env,
                                    inner,
                                    &NativeType::Pointer,
                                ) {
                                    Ok(v) => v,
                                    Err(_) => return fail3(),
                                },
                            }
                        } else {
                            match nv::read_value_from_ptr(
                                env,
                                inner as *const c_void,
                                &NativeType::Pointer,
                            ) {
                                Ok(v) => v,
                                Err(_) => return fail3(),
                            }
                        }
                    }
                    _ => match nv::read_value_from_ptr(
                        env,
                        storage_ptr as *const c_void,
                        &parse_native_type,
                    ) {
                        Ok(v) => v,
                        Err(_) => return fail3(),
                    },
                }
            };
            if let Some(mut wrapper) = wrapper_obj {
                let _ = nv::set_out_param_value(&mut wrapper, v);
            } else {
                out_values.push(v);
            }
        }

        (HRESULT(ret), result, out_values)
    }
}
