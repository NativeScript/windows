use crate::error::AnyError;
use crate::helpers::ffi_native_type_from_signature;
#[cfg(feature = "classic")]
use crate::value::{
    ffi_parse_bool_arg, ffi_parse_buffer_arg_with_length, ffi_parse_f32_arg, ffi_parse_f64_arg,
    ffi_parse_function_arg, ffi_parse_i16_arg, ffi_parse_i32_arg, ffi_parse_i64_arg,
    ffi_parse_i8_arg, ffi_parse_isize_arg, ffi_parse_pointer_arg, ffi_parse_query_interface_arg,
    ffi_parse_string_arg, ffi_parse_struct_arg, ffi_parse_u16_arg, ffi_parse_u32_arg,
    ffi_parse_u64_arg, ffi_parse_u8_arg, ffi_parse_usize_arg, read_value_from_ptr,
    set_out_param_value, try_unwrap_out_param, write_v8_value_to_ptr,
};
use crate::value::{NativeType, NativeValue};
use crate::ReturnKind;
#[cfg(feature = "classic")]
use crate::DeclarationFFI;
use libffi::middle::*;
use metadata::declarations::base_class_declaration::BaseClassDeclarationImpl;
use metadata::declarations::class_declaration::ClassDeclaration;
use metadata::declarations::declaration::Declaration;
use metadata::declarations::declaration::DeclarationKind;
use metadata::declarations::interface_declaration::generic_interface_declaration::GenericInterfaceDeclaration;
use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
use metadata::declarations::interface_declaration::InterfaceDeclaration;
use metadata::declarations::method_declaration::MethodDeclaration;
use metadata::declarations::parameter_declaration::ParameterDeclaration;
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
use windows::core::{IInspectable, IUnknown, Interface, GUID, HRESULT, HSTRING};
use windows::Win32::System::WinRT::IActivationFactory;
use windows::Win32::System::WinRT::Metadata::CorTokenType;

/// Precomputed marshaling decision for a `NativeType::Pointer` parameter. Every variant is a
/// pure function of the parameter's *signature*, so it is resolved once when the method's
/// static info is built instead of re-derived per call (signature string parsing, metadata
/// lookups, kind downcasts, and IID/delegate-info resolution were all previously on the hot
/// path of `call_napi`).
#[derive(Clone)]
pub(crate) enum PointerPlan {
    /// Plain pointer parse: non-WinRT signature, unresolvable type, or a resolvable kind
    /// that takes no special handling.
    Plain,
    /// `IReference<T>` parameter — box primitives via the typed Create* call (inner type name).
    IReference(String),
    /// `Windows.UI.Xaml.Interop.TypeName` struct — synthesize {Name, Kind} from a class ctor.
    TypeName,
    /// Other struct parameter — serialize field-by-field (declaration pre-resolved).
    Struct(Arc<RwLock<dyn Declaration>>),
    /// Delegate parameter — wrap a JS function with the precomputed (IID, invoke param types).
    Delegate(GUID, Vec<NativeType>),
    /// Interface/class parameter — QI the argument to this IID.
    Interface(GUID),
}

impl PointerPlan {
    /// Mirrors the (former) per-call decision tree of `call_napi`'s Pointer arm exactly —
    /// parity, not policy change. Only meaningful for in-params parsed as `Pointer`.
    ///
    /// `type_args` substitutes Var!N placeholders when resolving delegate info (interface-routed
    /// generic calls); `typename_special` opts into the `Windows.UI.Xaml.Interop.TypeName`
    /// synthesis (MethodCall's arm has it, PropertyCall's never did).
    pub(crate) fn for_parameter(
        signature: &str,
        parameter: &ParameterDeclaration,
        type_args: &[String],
        typename_special: bool,
    ) -> Self {
        if let Some(inner) = crate::helpers::ireference_inner_type(signature) {
            return PointerPlan::IReference(inner.to_string());
        }
        if !signature.contains('.') {
            return PointerPlan::Plain;
        }
        let lookup_name = crate::helpers::strip_generic_suffix(signature);
        let Some(declaration) = MetadataReader::find_by_name(lookup_name) else {
            return PointerPlan::Plain;
        };
        let kind = declaration.read().kind();
        match kind {
            DeclarationKind::Struct => {
                if typename_special
                    && declaration.read().full_name() == "Windows.UI.Xaml.Interop.TypeName"
                {
                    PointerPlan::TypeName
                } else {
                    PointerPlan::Struct(declaration)
                }
            }
            DeclarationKind::Delegate
            | DeclarationKind::GenericDelegate
            | DeclarationKind::GenericDelegateInstance => {
                let delegate_info = parameter.metadata().and_then(|meta| {
                    let raw_iid = Signature::to_iid_string(meta, &parameter.type_());
                    let iid_name = crate::property_call::substitute_type_vars(&raw_iid, type_args);
                    crate::delegate_info_from_type_sig(&iid_name)
                });
                match delegate_info {
                    Some((guid, param_types)) => PointerPlan::Delegate(guid, param_types),
                    None => PointerPlan::Plain,
                }
            }
            _ => {
                let iid = {
                    let lock = declaration.read();
                    match lock.kind() {
                        DeclarationKind::Interface => lock
                            .as_any()
                            .downcast_ref::<InterfaceDeclaration>()
                            .map(|interface| interface.id()),
                        DeclarationKind::GenericInterface => lock
                            .as_any()
                            .downcast_ref::<GenericInterfaceDeclaration>()
                            .map(|interface| interface.id()),
                        DeclarationKind::GenericInterfaceInstance => lock
                            .as_any()
                            .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                            .map(|interface| interface.id()),
                        DeclarationKind::Class => lock
                            .as_any()
                            .downcast_ref::<ClassDeclaration>()
                            .and_then(|class| class.default_interface())
                            .map(|iface| iface.id()),
                        _ => None,
                    }
                };
                match iid {
                    Some(iid) => PointerPlan::Interface(iid),
                    None => PointerPlan::Plain,
                }
            }
        }
    }
}

struct MethodStaticInfo {
    cif: Rc<Cif>,
    iid: GUID,
    pre_index: usize,
    /// Vtable slot index for the method on its declaring interface. The actual
    /// function pointer is re-read from the QI'd interface's vtable on every
    /// construction: caching the pointer itself would be wrong for methods
    /// declared on interfaces, where each implementing class has its own slot
    /// implementation behind the same metadata token.
    index: usize,
    method_name: String,
    parameter_types: Vec<NativeType>,
    parse_parameter_types: Vec<NativeType>,
    parameters: Vec<ParameterDeclaration>,
    return_type: String,
    is_void: bool,
    declaration: Option<Arc<RwLock<dyn BaseClassDeclarationImpl>>>,
    number_of_parameters: usize,
    return_kind: ReturnKind,
    param_sigs: Vec<String>,
    /// Per-parameter marshaling plan, aligned with `parse_parameter_types`. Only consulted
    /// for in-params whose parse type is `Pointer`; every other slot is `Plain`.
    param_plans: Vec<PointerPlan>,
    /// Return-slot classification, resolved once: scalar and string returns are written into
    /// the call's stable return buffer instead of the pointer slot.
    is_scalar_return: bool,
    is_string_return: bool,
}

impl MethodStaticInfo {
    fn error_stub() -> Rc<Self> {
        Rc::new(Self {
            cif: Rc::new(Cif::new(vec![], Type::i32())),
            iid: IActivationFactory::IID,
            pre_index: 0,
            index: 0,
            method_name: String::new(),
            parameter_types: Vec::new(),
            parse_parameter_types: Vec::new(),
            parameters: Vec::new(),
            return_type: String::new(),
            is_void: true,
            declaration: None,
            number_of_parameters: 0,
            return_kind: ReturnKind::Void,
            param_sigs: Vec::new(),
            param_plans: Vec::new(),
            is_scalar_return: false,
            is_string_return: false,
        })
    }
}

/// Return types marshaled through the call's stable scalar buffer rather than the pointer slot.
pub(crate) fn is_scalar_return_type(return_type: &str) -> bool {
    matches!(
        return_type,
        "UInt8"
            | "Int8"
            | "UInt16"
            | "Int16"
            | "UInt32"
            | "Int32"
            | "UInt64"
            | "Int64"
            | "USize"
            | "ISize"
            | "Single"
            | "Double"
            | "Boolean"
            | "Char16"
    )
}

thread_local! {
    // Keyed by (metadata scope, token | flags): metadata tokens are only unique within one
    // .winmd scope, so the raw IMetaDataImport2 pointer disambiguates same-token methods from
    // different winmds (e.g. a Windows.Data.Json method vs IMap::HasKey — observed collision).
    static METHOD_STATIC_INFO_CACHE: RefCell<ahash::AHashMap<(usize, u64), Rc<MethodStaticInfo>>>
        = RefCell::new(ahash::AHashMap::new());
}

pub struct MethodCall {
    si: Rc<MethodStaticInfo>,
    is_initializer: bool,
    is_sealed: bool,
    /// The interface the call goes through, already QI'd to `si.iid`. Owned (released on drop)
    /// when this call object did the QueryInterface itself; borrowed from a [`QiCache`] or a
    /// [`PreparedCall`] otherwise, whose owner outlives the call.
    interface: ManuallyDrop<IUnknown>,
    owns_interface: bool,
    func: *mut c_void,
    /// Scratch buffer used when a WinRT method returns a value type (e.g. GUID, Rect)
    /// that is larger than, or cannot be safely aliased through, a single pointer slot.
    return_value_buf: [u8; 128],
    /// Argument slots for the call; inline storage covers the common arities, so building a
    /// call does not allocate.
    argument_buf: SmallVec<[NativeValue; 8]>,
    /// Parse type per argument slot, aligned with `argument_buf`.
    argument_parse_types: SmallVec<[Option<NativeType>; 8]>,
    /// Set when construction failed (e.g. QueryInterface returned E_NOINTERFACE for the IID).
    /// call() returns E_FAIL immediately instead of panicking.
    init_error: Option<String>,
}

impl MethodCall {
    /// Drop the previous call's argument state, releasing the HSTRINGs built for `String`
    /// in-parameters first (the union slots would otherwise leak them).
    fn reset_args(&mut self) {
        unsafe { crate::ffi::release_string_args(&mut self.argument_buf, &self.argument_parse_types) };
        self.argument_buf.clear();
        self.argument_parse_types.clear();
    }
}

impl Drop for MethodCall {
    fn drop(&mut self) {
        self.reset_args();
        if self.owns_interface {
            unsafe { ManuallyDrop::drop(&mut self.interface) };
        }
    }
}

/// Per-wrapper cache of QueryInterface results: the interface pointers that calls through one
/// JS wrapper have already resolved on its instance, keyed by IID. Repeated calls on the wrapper
/// reuse the pointer instead of paying a cross-DLL QueryInterface plus refcount churn per call.
/// The cache lives next to the wrapper's own instance reference (`DeclarationFFI` on classic,
/// `HostHandle` on napi), so a pointer borrowed from it stays valid as long as the wrapper does.
pub(crate) struct QiCache(RefCell<SmallVec<[QiEntry; 4]>>);

/// One cached QueryInterface result. `owned` holds the reference the query returned, except
/// when the result is the very pointer the wrapper already holds (the common default-interface
/// case): that reference is released at once, since the wrapper's own keeps the pointer alive.
struct QiEntry {
    iid: GUID,
    ptr: *mut c_void,
    #[allow(dead_code)]
    owned: Option<IUnknown>,
}

impl QiCache {
    pub(crate) fn new() -> Self {
        Self(RefCell::new(SmallVec::new()))
    }

    /// Interface pointer for `iid` on `instance`, borrowed from the cache; queried and cached on
    /// a miss. `None` when the object does not implement `iid`.
    pub(crate) fn interface(&self, instance: &IUnknown, iid: &GUID) -> Option<*mut c_void> {
        if let Some(e) = self.0.borrow().iter().find(|e| e.iid == *iid) {
            return Some(e.ptr);
        }
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let hr = unsafe {
            (instance.vtable().QueryInterface)(
                instance.as_raw(),
                iid,
                &mut ptr as *mut _ as *mut *mut c_void,
            )
        };
        if hr.is_err() || ptr.is_null() {
            return None;
        }
        let queried = unsafe { IUnknown::from_raw(ptr) };
        let owned = if ptr == instance.as_raw() {
            drop(queried);
            None
        } else {
            Some(queried)
        };
        self.0.borrow_mut().push(QiEntry {
            iid: *iid,
            ptr,
            owned,
        });
        Some(ptr)
    }
}

impl Clone for QiCache {
    /// A cloned wrapper state starts with an empty cache; entries are re-resolved on demand.
    fn clone(&self) -> Self {
        Self::new()
    }
}

/// A method resolved once on a fixed target: the target's interface pointer already QI'd to the
/// method's interface plus its vtable slot. Activation factories are process-lifetime singletons
/// with a fixed vtable, so a static-method or constructor call site can keep one of these and
/// build every call from it without re-resolving the factory or querying it again.
#[derive(Clone)]
pub(crate) struct PreparedCall {
    si: Rc<MethodStaticInfo>,
    interface: IUnknown,
    func: *mut c_void,
    is_sealed: bool,
    is_initializer: bool,
}

impl MethodCall {
    /// Static info for a method already seen on this thread, if any.
    fn cached_static_info(
        method: &MethodDeclaration,
        is_sealed: bool,
    ) -> Option<Rc<MethodStaticInfo>> {
        let scope_key = windows::core::Interface::as_raw(method.metadata()?) as usize;
        let cache_key = (scope_key, ((method.token().0 as u64) << 1) | (is_sealed as u64));
        METHOD_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned())
    }

    /// A call over an interface pointer that some longer-lived owner holds: no QueryInterface
    /// and no refcount traffic. The vtable slot is still read per call object, because methods
    /// declared on interfaces share a metadata token across implementing classes.
    fn from_borrowed(
        si: Rc<MethodStaticInfo>,
        is_sealed: bool,
        is_initializer: bool,
        interface_ptr: *mut c_void,
    ) -> Self {
        let vtable = unsafe { *(interface_ptr as *const *const *mut c_void) };
        let func = unsafe { *vtable.add(si.index) };
        Self {
            si,
            is_initializer,
            is_sealed,
            interface: ManuallyDrop::new(unsafe { IUnknown::from_raw(interface_ptr) }),
            owns_interface: false,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
            init_error: None,
        }
    }

    /// Build a call on `instance` through the wrapper's `qi` cache, so repeated calls on one
    /// wrapper skip the per-call QueryInterface. The first call of a method on the thread still
    /// goes through [`MethodCall::new`] to build its static info.
    pub(crate) fn new_cached(
        method: &MethodDeclaration,
        is_sealed: bool,
        instance: &IUnknown,
        qi: &QiCache,
    ) -> Self {
        let Some(si) = Self::cached_static_info(method, is_sealed) else {
            return Self::new(method, is_sealed, instance.clone(), false);
        };
        match qi.interface(instance, &si.iid) {
            Some(ptr) => Self::from_borrowed(si, is_sealed, false, ptr),
            None => {
                let iid = si.iid;
                Self::new_init_error(
                    instance.clone(),
                    false,
                    is_sealed,
                    iid,
                    format!("QueryInterface failed (cached) for IID {:?}", iid),
                )
            }
        }
    }

    /// Resolve `method` on `target` once for reuse through [`MethodCall::from_prepared`].
    /// `None` when the target does not implement the method's interface.
    pub(crate) fn prepare(
        method: &MethodDeclaration,
        is_sealed: bool,
        target: IUnknown,
        is_initializer: bool,
    ) -> Option<PreparedCall> {
        let mut call = Self::new(method, is_sealed, target, is_initializer);
        if call.init_error.is_some() {
            return None;
        }
        // Move the interface out; the call object must not release it on drop.
        call.owns_interface = false;
        let interface = unsafe { ManuallyDrop::take(&mut call.interface) };
        Some(PreparedCall {
            si: Rc::clone(&call.si),
            interface,
            func: call.func,
            is_sealed: call.is_sealed,
            is_initializer: call.is_initializer,
        })
    }

    /// A call built from a [`PreparedCall`]: borrows its interface pointer and vtable slot.
    pub(crate) fn from_prepared(prepared: &PreparedCall) -> Self {
        Self {
            si: Rc::clone(&prepared.si),
            is_initializer: prepared.is_initializer,
            is_sealed: prepared.is_sealed,
            interface: ManuallyDrop::new(unsafe {
                IUnknown::from_raw(prepared.interface.as_raw())
            }),
            owns_interface: false,
            func: prepared.func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
            init_error: None,
        }
    }
}

#[inline]
fn call_failure() -> HRESULT {
    HRESULT(0x8000_4005u32 as i32)
}

impl MethodCall {
    fn new_init_error(
        interface: IUnknown,
        is_initializer: bool,
        is_sealed: bool,
        _iid: GUID,
        error_msg: String,
    ) -> Self {
        Self {
            si: MethodStaticInfo::error_stub(),
            is_initializer,
            is_sealed,
            interface: ManuallyDrop::new(interface),
            owns_interface: true,
            func: std::ptr::null_mut(),
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
            init_error: Some(error_msg),
        }
    }

    pub fn is_void(&self) -> bool {
        self.si.is_void
    }

    pub fn return_type(&self) -> &str {
        self.si.return_type.as_str()
    }

    pub(crate) fn return_kind(&self) -> &ReturnKind {
        &self.si.return_kind
    }

    pub fn init_error_message(&self) -> Option<&str> {
        self.init_error.as_deref()
    }

    pub fn new(
        method: &MethodDeclaration,
        is_sealed: bool,
        interface: IUnknown,
        is_initializer: bool,
    ) -> Self {
        let default_iid = IActivationFactory::IID;
        let Some(method_metadata) = method.metadata() else {
            return Self::new_init_error(
                interface,
                is_initializer,
                is_sealed,
                default_iid,
                "MethodCall::new missing metadata for method".to_string(),
            );
        };

        // Fast path: if we've seen this method+sealed combination, reuse cached info.
        // Skips find_declaring_interface_for_method + Cif::new + param processing.
        let scope_key = windows::core::Interface::as_raw(method_metadata) as usize;
        let cache_key = (scope_key, ((method.token().0 as u64) << 1) | (is_sealed as u64));
        if let Some(si) = METHOD_STATIC_INFO_CACHE.with(|c| c.borrow().get(&cache_key).cloned()) {
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
                let hr = if result.is_err() {
                    result.0
                } else {
                    0x8000_4002u32 as i32
                };
                return Self::new_init_error(
                    interface,
                    is_initializer,
                    is_sealed,
                    si.iid,
                    format!(
                        "QueryInterface failed (cached) for IID {:?}: {}",
                        si.iid,
                        crate::error::format_hresult_message(HRESULT(hr))
                    ),
                );
            }
            let queried_interface = unsafe { IUnknown::from_raw(interface_ptr) };
            // Re-read the function pointer from this instance's vtable: methods
            // declared on interfaces share a metadata token across implementing
            // classes, so the pointer must come from the actual object.
            let vtable_ptr: *mut *mut c_void =
                unsafe { std::mem::transmute(queried_interface.vtable()) };
            let func = unsafe { *vtable_ptr.add(si.index) };
            return Self {
                si,
                is_initializer,
                is_sealed,
                interface: ManuallyDrop::new(queried_interface),
                owns_interface: true,
                func,
                return_value_buf: [0u8; 128],
                argument_buf: SmallVec::new(),
                argument_parse_types: SmallVec::new(),
                init_error: None,
            };
        }

        let signature = method.return_type();

        let return_type = Signature::to_string(method_metadata, &signature);

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
                                .downcast_ref::<GenericInterfaceInstanceDeclaration>();
                            iid = ii
                                .map(|value| value.id())
                                .unwrap_or(IActivationFactory::IID);
                        }
                        _ => {
                            let ii = ii_lock
                                .as_declaration()
                                .as_any()
                                .downcast_ref::<InterfaceDeclaration>();
                            iid = ii
                                .map(|value| value.id())
                                .unwrap_or(IActivationFactory::IID);
                        }
                    }
                }
                declaration = Some(interface);
                iid
            }
        };

        let pre_index = index;

        let vtable = interface.vtable();

        let mut interface_ptr: *mut c_void = std::ptr::null_mut();

        let result = unsafe {
            ((*vtable).QueryInterface)(
                interface.as_raw(),
                &iid,
                &mut interface_ptr as *mut _ as *mut *mut c_void,
            )
        };

        // QueryInterface can legitimately fail (E_NOINTERFACE) when the IID resolved
        // from metadata is zeroed/missing. Panic here crashes the process via
        // panic_cannot_unwind because we are inside a V8 callback — store the error
        // and surface it through call() as a JS error instead.
        if result.is_err() || interface_ptr.is_null() {
            let hr = if result.is_err() {
                result.0
            } else {
                0x8000_4002u32 as i32
            }; // E_NOINTERFACE
            let error_msg = format!(
                "QueryInterface failed for IID {:?}: {}",
                iid,
                crate::error::format_hresult_message(HRESULT(hr))
            );
            return Self::new_init_error(interface, is_initializer, is_sealed, iid, error_msg);
        }

        let is_composition = !is_sealed;

        let is_void = method.is_void();

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
            let Some(metadata) = parameter.metadata() else {
                return Self::new_init_error(
                    interface,
                    is_initializer,
                    is_sealed,
                    iid,
                    "MethodCall::new missing parameter metadata".to_string(),
                );
            };

            let signature = Signature::to_string(metadata, &type_);

            let parse_native_type = match NativeType::try_from(signature.as_str()) {
                Ok(value) => value,
                Err(_) => {
                    return Self::new_init_error(
                        interface,
                        is_initializer,
                        is_sealed,
                        iid,
                        format!("Unsupported parameter type signature: {}", signature),
                    );
                }
            };
            parse_parameter_types.push(parse_native_type.clone());
            param_sigs.push(signature.clone());
            // If this parameter is an out (ByRef) parameter, represent its
            // ABI as a pointer to the underlying storage so callers allocate
            // space and pass the address for the callee to write into.
            if parameter.is_out() || signature.trim().starts_with("ByRef ") {
                parameter_types.push(NativeType::Pointer);
            } else {
                // If the parsed parameter is a WinRT `String`, treat its ABI as
                // `NativeType::String` (handle-sized) rather than the generic
                // pointer returned by the signature helper. This ensures the
                // libffi CIF is constructed with the correct usize-sized type.
                if matches!(parse_native_type, NativeType::String) {
                    parameter_types.push(NativeType::String);
                } else {
                    // WinRT structs are passed by value; resolve to a proper struct
                    // type so libffi dereferences the data pointer instead of
                    // forwarding the raw heap address as the argument value.
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

        let params = match parameter_types
            .iter()
            .cloned()
            .map(libffi::middle::Type::try_from)
            .collect::<std::result::Result<Vec<Type>, AnyError>>()
        {
            Ok(value) => value,
            Err(err) => {
                return Self::new_init_error(
                    interface,
                    is_initializer,
                    is_sealed,
                    iid,
                    format!("Failed to create libffi parameter types: {}", err),
                );
            }
        };

        let cif = Rc::new(Cif::new(params, Type::i32()));

        // Take ownership of the specific interface pointer returned by QueryInterface
        // so we can inspect its vtable directly to compute the correct function slot.
        let queried_interface = unsafe { IUnknown::from_raw(interface_ptr as *mut c_void) };
        let vtable_struct = queried_interface.vtable();
        let vtable_ptr: *mut *mut c_void = unsafe { std::mem::transmute(vtable_struct) };

        // If the queried interface supports IInspectable, its vtable includes the
        // 6 IInspectable slots; otherwise it's an IUnknown-only vtable with 3 slots.
        let mut inspectable_ptr: *mut c_void = std::ptr::null_mut();
        let supports_iinspectable = unsafe {
            ((*vtable_struct).QueryInterface)(
                queried_interface.as_raw(),
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
        let is_scalar_return = is_scalar_return_type(&return_type);
        let is_string_return = return_type == "String";

        // Resolve each Pointer in-param's marshaling plan once; out/ByRef params and
        // non-pointer types never consult their slot.
        let param_plans: Vec<PointerPlan> = parse_parameter_types
            .iter()
            .zip(parameters.iter())
            .zip(param_sigs.iter())
            .map(|((nt, parameter), sig)| {
                let is_out = parameter.is_out() || sig.starts_with("ByRef ");
                if !is_out && matches!(nt, NativeType::Pointer) {
                    PointerPlan::for_parameter(sig, parameter, &[], true)
                } else {
                    PointerPlan::Plain
                }
            })
            .collect();

        // Store static info in cache so future calls on the same method type skip the slow path.
        let static_info = Rc::new(MethodStaticInfo {
            cif,
            iid,
            pre_index,
            index,
            method_name: method.name().to_string(),
            parameter_types,
            parse_parameter_types,
            parameters,
            return_type,
            is_void,
            declaration,
            number_of_parameters,
            return_kind,
            param_sigs,
            param_plans,
            is_scalar_return,
            is_string_return,
        });
        METHOD_STATIC_INFO_CACHE
            .with(|c| c.borrow_mut().insert(cache_key, Rc::clone(&static_info)));

        Self {
            si: static_info,
            is_initializer,
            is_sealed,
            interface: ManuallyDrop::new(queried_interface),
            owns_interface: true,
            func,
            return_value_buf: [0u8; 128],
            argument_buf: SmallVec::new(),
            argument_parse_types: SmallVec::new(),
            init_error: None,
        }
    }

    #[cfg(feature = "classic")]
    pub fn call<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        args: &v8::FunctionCallbackArguments,
    ) -> (HRESULT, *mut c_void, Vec<v8::Local<'s, v8::Value>>) {
        if self.init_error.is_some() {
            return (
                HRESULT(0x8000_4005u32 as i32),
                std::ptr::null_mut(),
                Vec::new(),
            );
        }

        // Snapshot fields before the mutable borrow of argument_buf begins.
        let is_initializer = self.is_initializer;
        let is_sealed = self.is_sealed;
        let is_void = self.si.is_void;
        let is_value_type = matches!(
            self.si.return_kind,
            ReturnKind::Struct(_) | ReturnKind::Guid
        );

        let is_scalar_return = self.si.is_scalar_return;
        // HSTRING out-params must also land in a stable buffer so the returned
        // pointer remains valid after this call frame is unwound.
        let is_string_return = self.si.is_string_return;

        self.reset_args();
        let mut queried_interfaces: SmallVec<[IUnknown; 2]> = SmallVec::new();
        let mut struct_scratch: Vec<Vec<u8>> = Vec::new();
        // Track out-parameter slots: (argument_buf index, parse_native_type, param_index, wrapper)
        // param_index references self.param_sigs to avoid a String allocation per out-param per call.
        let mut out_slots: Vec<(usize, NativeType, usize, Option<v8::Local<'s, v8::Object>>)> =
            Vec::new();

        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        self.argument_parse_types.push(None);

        for (i, native_type) in self.si.parse_parameter_types.iter().enumerate() {
            let parameter = &self.si.parameters[i];
            // Use the pre-computed signature string (cached at construction time) to avoid
            // calling Signature::to_string() + metadata table reads on every invocation.
            let param_sig = &self.si.param_sigs[i];
            let is_sig_byref = param_sig.starts_with("ByRef ");

            // Handle out (ByRef) parameters by allocating stable storage that
            // the callee can write into. Also treat a missing caller argument
            // for a `ByRef` signature as an implicit out-slot (e.g. TryParse).
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
                // Default parse type is None (out-only). If we initialized
                // from the caller's JS value below, we'll set it accordingly.
                self.argument_parse_types.push(None);

                let raw_init_val = args.get(i as i32);
                let out_wrapper = try_unwrap_out_param(scope, raw_init_val);
                let (wrapper_obj, init_val) = match out_wrapper {
                    Some((obj, value)) => (Some(obj), value),
                    None => (None, raw_init_val),
                };

                // Try to initialize from caller-provided argument if present.
                if (args.length() as usize) > i {
                    if !init_val.is_undefined() && !init_val.is_null() {
                        match write_v8_value_to_ptr(scope, init_val, ptr, native_type) {
                            Ok(_) => {}
                            Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
                        }
                    }
                }

                // Store the parameter index so the marshaling loop can read si.param_sigs[i]
                // directly instead of carrying an owned String through the out_slots vector.
                out_slots.push((slot_index, native_type.clone(), i, wrapper_obj));
                continue;
            }

            let value = args.get(i as i32);

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
                    // Signature classification (IReference/struct/delegate/interface) resolved
                    // once into the per-parameter plan at static-info build time (PointerPlan).
                    match &self.si.param_plans[i] {
                        PointerPlan::Plain => ffi_parse_pointer_arg(scope, value),
                        // IReference<T> parameters: box JS primitives with the correct Create* call
                        // so XAML receives the right typed IPropertyValue (e.g. IReference<Double>).
                        PointerPlan::IReference(inner) => {
                            if let Some(nv) = crate::value::box_as_ireference(scope, value, inner) {
                                Ok(nv)
                            } else {
                                ffi_parse_pointer_arg(scope, value)
                            }
                        }
                        PointerPlan::TypeName => {
                            {
                                    // If the JS argument is a class constructor (instance=None,
                                    // struct_instance=None) synthesise a TypeName{Name,Kind=Metadata}.
                                    let synthesized: Option<*mut c_void> = 'synth: {
                                        if value.is_object() {
                                            let obj = value.to_object(scope).unwrap();
                                            if let Some(field) = obj.get_internal_field(scope, 0) {
                                                let ext = unsafe { field.cast::<v8::External>() };
                                                let dec_ffi = unsafe {
                                                    &*(ext.value() as *mut DeclarationFFI)
                                                };
                                                if dec_ffi.struct_instance.is_none()
                                                    && dec_ffi.instance.is_none()
                                                {
                                                    let class_name = dec_ffi
                                                        .inner
                                                        .read()
                                                        .full_name()
                                                        .to_string();
                                                    let hstring =
                                                        HSTRING::from(class_name.as_str());
                                                    // SAFETY: HSTRING is repr(transparent) over *mut u16.
                                                    // Transmute moves ownership out; the raw handle is
                                                    // stored in the leaked bytes and intentionally leaked
                                                    // alongside them (Navigate reads Name only during the call).
                                                    let raw: usize =
                                                        unsafe { std::mem::transmute(hstring) };
                                                    let mut bytes: Box<[u8; 16]> =
                                                        Box::new([0u8; 16]);
                                                    unsafe {
                                                        *(bytes.as_mut_ptr() as *mut usize) = raw;
                                                        *(bytes.as_mut_ptr().add(8) as *mut u32) =
                                                            1u32;
                                                    }
                                                    break 'synth Some(
                                                        Box::leak(bytes).as_ptr() as *mut c_void
                                                    );
                                                }
                                            }
                                        }
                                        None
                                    };
                                    if let Some(ptr) = synthesized {
                                        Ok(NativeValue { pointer: ptr })
                                    } else {
                                        // Already a TypeName struct instance — ffi_parse_pointer_arg
                                        // extracts struct_instance.buf.as_ptr() via try_get_external_handle.
                                        ffi_parse_pointer_arg(scope, value)
                                    }
                            }
                        }
                        PointerPlan::Struct(declaration) => {
                                {
                                    // Other structs: accept ArrayBuffer, struct instances, or plain JS objects.
                                    if value.is_array_buffer() || value.is_array_buffer_view() {
                                        ffi_parse_struct_arg(scope, value)
                                    } else if value.is_object() {
                                        let obj_v = value.to_object(scope).unwrap();
                                        let has_internal = obj_v
                                            .get_internal_field(scope, 0)
                                            .map(|f| {
                                                !unsafe { f.cast::<v8::External>() }
                                                    .value()
                                                    .is_null()
                                            })
                                            .unwrap_or(false);
                                        if has_internal {
                                            ffi_parse_pointer_arg(scope, value)
                                        } else {
                                            // Plain JS object {A:255, R:0, …} — build bytes honoring field
                                            // alignment, nested structs, and enum fields (see
                                            // property_call::append_struct_object_bytes).
                                            let mut sbuf: Vec<u8> = Vec::new();
                                            {
                                                let lock = declaration.read();
                                                if let Some(sd) = lock
                                                    .as_any()
                                                    .downcast_ref::<StructDeclaration>()
                                                {
                                                    crate::property_call::append_struct_object_bytes(&mut sbuf, scope, obj_v, sd);
                                                }
                                            }
                                            if sbuf.is_empty() {
                                                sbuf.push(0);
                                            }
                                            let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                            struct_scratch.push(sbuf);
                                            Ok(NativeValue { pointer: ptr })
                                        }
                                    } else {
                                        ffi_parse_pointer_arg(scope, value)
                                    }
                                }
                        }
                        PointerPlan::Delegate(guid, delegate_param_types) => {
                                {
                                    let handle_ptr = value.to_object(scope).and_then(|obj| {
                                        let key = v8::String::new(scope, "handle")?;
                                        let hv = obj.get(scope, key.into())?;
                                        v8::Local::<v8::External>::try_from(hv)
                                            .ok()
                                            .map(|e| e.value())
                                    });

                                    if let Some(ptr) = handle_ptr {
                                        Ok(NativeValue { pointer: ptr })
                                    } else if let Ok(func) =
                                        v8::Local::<v8::Function>::try_from(value)
                                    {
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
                        }
                        PointerPlan::Interface(iid) => {
                            match ffi_parse_query_interface_arg(scope, value, iid) {
                                Ok((pointer, Some(interface_guard))) => {
                                    queried_interfaces.push(interface_guard);
                                    Ok(pointer)
                                }
                                Ok((pointer, None)) => Ok(pointer),
                                Err(_) => ffi_parse_pointer_arg(scope, value),
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
                NativeType::Struct(_) => ffi_parse_struct_arg(scope, value),
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
        let composition_outer: *mut c_void = std::ptr::null_mut();
        let mut composition_inner: *mut c_void = std::ptr::null_mut();

        if is_initializer {
            if !is_sealed {
                // pOuter (baseInterface) must be a literal null pointer, not a pointer-to-null.
                // Passing &mut composition_outer (a stack address) causes the factory to
                // treat a non-null value as a valid IInspectable and crash on vtable access.
                self.argument_buf.push(NativeValue {
                    pointer: std::ptr::null_mut(),
                });
                self.argument_parse_types.push(None);
                // ppInner is an out-slot: pass the address of our local so the factory
                // can write the inner interface pointer through it.
                unsafe {
                    self.argument_buf.push(NativeValue {
                        pointer: &mut composition_inner as *mut _ as *mut c_void,
                    })
                };
                self.argument_parse_types.push(None);
            }
            unsafe {
                self.argument_buf.push(NativeValue {
                    pointer: &mut result as *mut _ as *mut c_void,
                })
            };
            self.argument_parse_types.push(None);
        } else if !is_void {
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
            Ok(value) => value,
            Err(_) => return (call_failure(), std::ptr::null_mut(), Vec::new()),
        };

        let func_to_call = self.func;

        let call_args =
            crate::ffi::build_call_args(&prep, &self.argument_buf, &self.si.parameter_types);

        // Guard against Rust panics crossing the FFI boundary and log common
        // COM HRESULTs (e.g. RPC_E_WRONG_THREAD) so callers can surface a
        // diagnostic to JS before a late destructor or release triggers a
        // process-level failure.
        let ret_i32_res = catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si
                .cif
                .call(CodePtr::from_ptr(func_to_call), &call_args)
        }));

        let ret = match ret_i32_res {
            Ok(code) => code,
            Err(_) => {
                let method_display = if self.si.method_name == ".ctor" {
                    "constructor"
                } else if self.si.method_name == ".cctor" {
                    "static constructor"
                } else {
                    self.si.method_name.as_str()
                };
                let msg = format!(
                    "WinRT call panicked during invocation of '{}': returning E_FAIL",
                    method_display
                );
                crate::store_last_js_error(msg);
                return (
                    HRESULT(0x8000_4005u32 as i32),
                    std::ptr::null_mut(),
                    Vec::new(),
                );
            }
        };

        // If the call returned RPC_E_WRONG_THREAD, format the canonical OS
        // message and surface it both in logs/last-error and as a V8 exception
        // so embedders/tests can catch it directly.
        let hr = HRESULT(ret);
        const RPC_E_WRONG_THREAD: u32 = 0x8001010E;
        if (hr.0 as u32) == RPC_E_WRONG_THREAD {
            let method_display = if self.si.method_name == ".ctor" {
                "constructor"
            } else if self.si.method_name == ".cctor" {
                "static constructor"
            } else {
                self.si.method_name.as_str()
            };
            let detail = crate::error::format_hresult_message(hr);
            let msg = format!("{} when invoking '{}'", detail, method_display);
            crate::store_last_js_error(msg.clone());
            if let Some(vmstr) = v8::String::new(scope, &msg) {
                let err = v8::Exception::error(scope, vmstr);
                scope.throw_exception(err);
            }
        }

        if is_initializer && !is_sealed && result.is_null() {
            if !composition_inner.is_null() {
                result = composition_inner;
            } else if !composition_outer.is_null() {
                result = composition_outer;
            }
        }

        if !is_initializer && !is_void && (is_value_type || is_scalar_return || is_string_return) {
            // Point result at the scratch buffer — all three categories write
            // into return_value_buf, so the pointer is stable for the caller.
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
            // Borrow the pre-cached signature string (no String clone needed).
            let sig = &self.si.param_sigs[param_idx];
            unsafe {
                let v = match parse_native_type {
                    NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                        // Storage holds a pointer-sized value; read the inner pointer.
                        let inner =
                            std::ptr::read_unaligned(storage_ptr as *const usize) as *mut c_void;
                        if inner.is_null() {
                            v8::null(scope).into()
                        } else if !sig.is_empty() && sig.contains('.') {
                            // If the original parameter signature names a WinRT type,
                            // construct the proper JS WinRT wrapper (structs or ns objects).
                            let mut lookup = sig.as_str();
                            if let Some(stripped) = lookup.strip_prefix("ByRef ") {
                                lookup = stripped;
                            }
                            let lookup = crate::helpers::strip_generic_suffix(lookup);
                            if let Some(declaration) = MetadataReader::find_by_name(lookup) {
                                if matches!(declaration.read().kind(), DeclarationKind::Struct) {
                                    crate::create_struct_object_from_raw(declaration, inner, scope)
                                        .into()
                                } else {
                                    // Attempt to inspect runtime identity via IInspectable
                                    let instance = unsafe { IUnknown::from_raw(inner) };
                                    match instance.clone().cast::<IInspectable>() {
                                        Ok(ins) => {
                                            let _ = ins.GetRuntimeClassName();
                                        }
                                        Err(e) => {
                                            let _ = e;
                                        }
                                    }
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

    /// Call an event add-method with a raw COM delegate pointer.
    /// Returns `(HRESULT, token)` where token is the EventRegistrationToken i64 value.
    pub fn call_with_raw_ptr(&mut self, ptr: *mut c_void) -> (HRESULT, i64) {
        if let Some(error) = self.init_error.as_deref() {
            crate::store_last_js_error(error.to_string());
            return (call_failure(), 0);
        }
        if self.func.is_null() {
            crate::store_last_js_error(
                "WinRT event add call has no ABI function pointer".to_string(),
            );
            return (call_failure(), 0);
        }

        let is_void = self.si.is_void;
        self.reset_args();
        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        self.argument_buf.push(NativeValue { pointer: ptr });
        let mut token: i64 = 0;
        if !is_void {
            self.argument_buf.push(NativeValue {
                pointer: &mut token as *mut _ as *mut c_void,
            });
        }
        let mut call_args: Vec<Arg> = Vec::with_capacity(self.argument_buf.len());
        for (i, v) in self.argument_buf.iter().enumerate() {
            let Some(native_type) = self.si.parameter_types.get(i) else {
                return (call_failure(), 0);
            };
            call_args.push(unsafe { v.as_arg(native_type) });
        }
        let ret: i32 = match catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si.cif.call(CodePtr::from_ptr(self.func), &call_args)
        })) {
            Ok(code) => code,
            Err(_) => {
                let msg = format!("WinRT event call panicked during invocation: returning E_FAIL");
                crate::store_last_js_error(msg);
                return (call_failure(), 0);
            }
        };
        let hr = HRESULT(ret);
        if hr.is_err() {
            crate::store_last_js_error(format!(
                "WinRT event add failed for '{}': {}",
                self.si.method_name,
                crate::error::format_hresult_message(hr)
            ));
            return (hr, 0);
        }
        (hr, token)
    }

    /// Call an event remove-method with an EventRegistrationToken value.
    /// The token is passed by value (i64) per the WinRT ABI for remove_* methods.
    pub fn call_with_event_token(&mut self, token: i64) -> HRESULT {
        if let Some(error) = self.init_error.as_deref() {
            crate::store_last_js_error(error.to_string());
            return call_failure();
        }
        if self.func.is_null() {
            crate::store_last_js_error(
                "WinRT event remove call has no ABI function pointer".to_string(),
            );
            return call_failure();
        }

        self.reset_args();
        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        // NativeType::Struct: as_arg dereferences the pointer field, so pass
        // &token_storage. Passing the token directly as i64 is read as an
        // address — remove silently no-ops and the old handler keeps firing.
        let mut token_storage: i64 = token;
        let token_arg = match self.si.parameter_types.get(1) {
            Some(NativeType::Struct(_)) => NativeValue {
                pointer: &mut token_storage as *mut i64 as *mut c_void,
            },
            _ => NativeValue { i64_value: token },
        };
        self.argument_buf.push(token_arg);
        let mut call_args: Vec<Arg> = Vec::with_capacity(self.argument_buf.len());
        for (i, v) in self.argument_buf.iter().enumerate() {
            let Some(native_type) = self.si.parameter_types.get(i) else {
                return call_failure();
            };
            call_args.push(unsafe { v.as_arg(native_type) });
        }
        let ret: i32 = match catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si.cif.call(CodePtr::from_ptr(self.func), &call_args)
        })) {
            Ok(code) => code,
            Err(_) => {
                let msg =
                    format!("WinRT event remove call panicked during invocation: returning E_FAIL");
                crate::store_last_js_error(msg);
                return call_failure();
            }
        };
        HRESULT(ret)
    }
}

/// Node-API entry point for invoking a WinRT method: runs the same marshaling pipeline,
/// argument-buffer reuse, out-slot handling, and error contract as `MethodCall::call`, but
/// takes JS values as napi handles instead of a `FunctionCallbackArguments`. Kept in this file
/// so it can share the private fields with `MethodCall::call`.
#[cfg(feature = "napi_engine")]
impl MethodCall {
    pub fn call_napi(
        &mut self,
        env: &napi::Env,
        args: &[napi::JsUnknown],
    ) -> (HRESULT, *mut c_void, Vec<napi::JsUnknown>) {
        use crate::napi_engine::delegate::make_napi_delegate;
        use crate::napi_engine::value as nv;
        use napi::{JsFunction, JsObject, JsUnknown, NapiRaw, NapiValue, ValueType};

        // Re-materialize the same napi handle as an owned JsUnknown (parsers take &JsUnknown;
        // handles are cheap value-copies of (env, napi_value)).
        #[inline]
        fn dup_arg(env: &napi::Env, v: &JsUnknown) -> JsUnknown {
            unsafe { JsUnknown::from_raw_unchecked(env.raw(), v.raw()) }
        }
        // v8's args.get(i) yields undefined past the end; mirror that.
        #[inline]
        fn arg_at(env: &napi::Env, args: &[JsUnknown], i: usize) -> Option<JsUnknown> {
            match args.get(i) {
                Some(v) => Some(dup_arg(env, v)),
                None => env
                    .get_undefined()
                    .ok()
                    .map(|u| unsafe { JsUnknown::from_raw_unchecked(env.raw(), u.raw()) }),
            }
        }
        #[inline]
        fn fail3() -> (HRESULT, *mut c_void, Vec<napi::JsUnknown>) {
            (call_failure(), std::ptr::null_mut(), Vec::new())
        }

        if self.init_error.is_some() {
            return (
                HRESULT(0x8000_4005u32 as i32),
                std::ptr::null_mut(),
                Vec::new(),
            );
        }

        let is_initializer = self.is_initializer;
        let is_sealed = self.is_sealed;
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
        // (argument_buf index, parse_native_type, param_index, out-wrapper object)
        let mut out_slots: Vec<(usize, NativeType, usize, Option<napi::JsObject>)> = Vec::new();

        self.argument_buf.push(NativeValue {
            pointer: self.interface.as_raw() as *mut c_void,
        });
        self.argument_parse_types.push(None);

        for (i, native_type) in self.si.parse_parameter_types.iter().enumerate() {
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

                let raw_init_val = match arg_at(env, args, i) {
                    Some(v) => v,
                    None => return fail3(),
                };
                let (wrapper_obj, init_val) = match nv::try_unwrap_out_param(env, &raw_init_val) {
                    Some((obj, value)) => (Some(obj), value),
                    None => (None, raw_init_val),
                };

                if args.len() > i {
                    let vt = init_val.get_type().unwrap_or(ValueType::Undefined);
                    if vt != ValueType::Undefined && vt != ValueType::Null {
                        match nv::write_js_value_to_ptr(env, &init_val, ptr, native_type) {
                            Ok(_) => {}
                            Err(_) => return fail3(),
                        }
                    }
                }

                out_slots.push((slot_index, native_type.clone(), i, wrapper_obj));
                continue;
            }

            let value = match arg_at(env, args, i) {
                Some(v) => v,
                None => return fail3(),
            };
            let vt = value.get_type().unwrap_or(ValueType::Undefined);

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
                    // All signature classification (IReference/struct/delegate/interface) was
                    // resolved once into the per-parameter plan when the static info was built.
                    match &self.si.param_plans[i] {
                        PointerPlan::Plain => nv::napi_parse_pointer(env, &value),
                        PointerPlan::IReference(inner) => {
                            // IReference<T>: box primitives with the correct typed Create* call.
                            if let Some(nvv) = nv::box_as_ireference(env, &value, inner) {
                                Ok(nvv)
                            } else {
                                nv::napi_parse_pointer(env, &value)
                            }
                        }
                        PointerPlan::TypeName => {
                            {
                                    // Class constructor passed for a TypeName parameter →
                                    // synthesize TypeName{Name, Kind=Metadata}. The wrapped
                                    // DeclarationFFI arrives via env.unwrap (rusty_v8 used
                                    // internal field 0).
                                    let synthesized: Option<*mut c_void> = 'synth: {
                                        if vt == ValueType::Object || vt == ValueType::Function {
                                            let obj: JsObject = unsafe { value.cast() };
                                            if let Ok(dec_ffi) =
                                                env.unwrap::<crate::DeclarationFFI>(&obj)
                                            {
                                                if dec_ffi.struct_instance.is_none()
                                                    && dec_ffi.instance.is_none()
                                                {
                                                    let class_name = dec_ffi
                                                        .inner
                                                        .read()
                                                        .full_name()
                                                        .to_string();
                                                    let hstring =
                                                        HSTRING::from(class_name.as_str());
                                                    // SAFETY: HSTRING is repr(transparent) over
                                                    // *mut u16; ownership moves into the leaked
                                                    // bytes (Navigate reads Name during the call).
                                                    let raw: usize =
                                                        unsafe { std::mem::transmute(hstring) };
                                                    let mut bytes: Box<[u8; 16]> =
                                                        Box::new([0u8; 16]);
                                                    unsafe {
                                                        *(bytes.as_mut_ptr() as *mut usize) = raw;
                                                        *(bytes.as_mut_ptr().add(8)
                                                            as *mut u32) = 1u32;
                                                    }
                                                    break 'synth Some(
                                                        Box::leak(bytes).as_ptr() as *mut c_void
                                                    );
                                                }
                                            }
                                        }
                                        None
                                    };
                                    if let Some(ptr) = synthesized {
                                        Ok(NativeValue { pointer: ptr })
                                    } else {
                                        nv::napi_parse_pointer(env, &value)
                                    }
                            }
                        }
                        PointerPlan::Struct(declaration) => {
                                {
                                    // Other structs: ArrayBuffer bytes, wrapped struct
                                    // instances, or plain JS objects serialized field-by-field.
                                    let is_buffer_like =
                                        nv::napi_parse_struct(env, &value).is_ok();
                                    if is_buffer_like {
                                        nv::napi_parse_struct(env, &value)
                                    } else if vt == ValueType::Object {
                                        let obj: JsObject = unsafe { value.cast() };
                                        let has_wrap =
                                            env.unwrap::<crate::DeclarationFFI>(&obj).is_ok();
                                        if has_wrap {
                                            nv::napi_parse_pointer(env, &value)
                                        } else {
                                            let mut sbuf: Vec<u8> = Vec::new();
                                            {
                                                let lock = declaration.read();
                                                if let Some(sd) = lock
                                                    .as_any()
                                                    .downcast_ref::<StructDeclaration>()
                                                {
                                                    crate::property_call::append_struct_object_bytes_napi(env, &mut sbuf, &obj, sd);
                                                }
                                            }
                                            if sbuf.is_empty() {
                                                sbuf.push(0);
                                            }
                                            let ptr = sbuf.as_mut_ptr() as *mut c_void;
                                            struct_scratch.push(sbuf);
                                            Ok(NativeValue { pointer: ptr })
                                        }
                                    } else {
                                        nv::napi_parse_pointer(env, &value)
                                    }
                                }
                        }
                        PointerPlan::Delegate(guid, delegate_param_types) => {
                                {
                                    // Pre-wrapped delegate ({handle: External}) → pass through.
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
                        }
                        PointerPlan::Interface(iid) => {
                            match nv::napi_parse_query_interface(env, &value, iid) {
                                Ok((pointer, Some(interface_guard))) => {
                                    queried_interfaces.push(interface_guard);
                                    Ok(pointer)
                                }
                                Ok((pointer, None)) => Ok(pointer),
                                Err(_) => nv::napi_parse_pointer(env, &value),
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
                NativeType::Struct(_) => nv::napi_parse_struct(env, &value),
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
        let composition_outer: *mut c_void = std::ptr::null_mut();
        let mut composition_inner: *mut c_void = std::ptr::null_mut();

        if is_initializer {
            if !is_sealed {
                // pOuter must be a literal null pointer (see the rusty_v8 original).
                self.argument_buf.push(NativeValue {
                    pointer: std::ptr::null_mut(),
                });
                self.argument_parse_types.push(None);
                self.argument_buf.push(NativeValue {
                    pointer: &mut composition_inner as *mut _ as *mut c_void,
                });
                self.argument_parse_types.push(None);
            }
            self.argument_buf.push(NativeValue {
                pointer: &mut result as *mut _ as *mut c_void,
            });
            self.argument_parse_types.push(None);
        } else if !is_void {
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
            Ok(value) => value,
            Err(_) => return fail3(),
        };

        let func_to_call = self.func;
        let call_args =
            crate::ffi::build_call_args(&prep, &self.argument_buf, &self.si.parameter_types);

        let ret_i32_res = catch_unwind(AssertUnwindSafe(|| unsafe {
            self.si
                .cif
                .call(CodePtr::from_ptr(func_to_call), &call_args)
        }));

        let ret = match ret_i32_res {
            Ok(code) => code,
            Err(_) => {
                let method_display = if self.si.method_name == ".ctor" {
                    "constructor"
                } else if self.si.method_name == ".cctor" {
                    "static constructor"
                } else {
                    self.si.method_name.as_str()
                };
                let msg = format!(
                    "WinRT call panicked during invocation of '{}': returning E_FAIL",
                    method_display
                );
                crate::store_last_js_error(msg);
                return (
                    HRESULT(0x8000_4005u32 as i32),
                    std::ptr::null_mut(),
                    Vec::new(),
                );
            }
        };

        let hr = HRESULT(ret);
        const RPC_E_WRONG_THREAD: u32 = 0x8001010E;
        if (hr.0 as u32) == RPC_E_WRONG_THREAD {
            let method_display = if self.si.method_name == ".ctor" {
                "constructor"
            } else if self.si.method_name == ".cctor" {
                "static constructor"
            } else {
                self.si.method_name.as_str()
            };
            let detail = crate::error::format_hresult_message(hr);
            let msg = format!("{} when invoking '{}'", detail, method_display);
            crate::store_last_js_error(msg.clone());
            nv::throw_js_error(env, &msg);
        }

        if is_initializer && !is_sealed && result.is_null() {
            if !composition_inner.is_null() {
                result = composition_inner;
            } else if !composition_outer.is_null() {
                result = composition_outer;
            }
        }

        if !is_initializer && !is_void && (is_value_type || is_scalar_return || is_string_return) {
            result = self.return_value_buf.as_mut_ptr() as *mut c_void;
        }

        // Marshal out-parameters back into JS values using the recorded slots.
        // TODO(ns_proxy port): WinRT-typed out params (structs / class instances) get typed
        // wrappers once create_struct_object_from_raw / create_ns_ctor_instance_object are
        // ported; until then they surface as externals via the fallback below.
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
                            // WinRT-typed out param: `inner` is the COM/struct pointer.
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
                            // Non-WinRT pointer sig: identical read path to the rusty_v8
                            // original (read_value_from_ptr on `inner`).
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
