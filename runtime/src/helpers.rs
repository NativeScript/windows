use crate::value::NativeType;
use metadata::declarations::struct_declaration::StructDeclaration;
use metadata::meta_data_reader::MetadataReader;
use metadata::signature::Signature;
#[cfg(feature = "classic")]
use regex::Regex;
#[cfg(feature = "classic")]
use std::sync::OnceLock;

#[cfg(feature = "classic")]
static RE_GENERIC_COUNT: OnceLock<Regex> = OnceLock::new();
#[cfg(feature = "classic")]
static RE_GENERIC_PARAMS: OnceLock<Regex> = OnceLock::new();

/// Returns the open-generic type name without the closed `<T, U, …>` arguments,
/// preserving the backtick+arity suffix required by WinRT metadata lookups.
///
/// Example: `"Windows.Foundation.IAsyncOperationWithProgress`2<T,P>"` → `"Windows.Foundation.IAsyncOperationWithProgress`2"`
#[inline]
pub fn strip_generic_suffix(name: &str) -> &str {
    if let Some(angle) = name.find('<') {
        return &name[..angle];
    }
    name
}

#[cfg(feature = "classic")]
pub struct GenericReturnTypes<'s> {
    names: Vec<&'s str>,
    types: usize,
}

#[cfg(feature = "classic")]
impl GenericReturnTypes<'_> {
    pub fn names(&self) -> &[&str] {
        self.names.as_slice()
    }

    pub fn types(&self) -> usize {
        self.types
    }
}

#[cfg(feature = "classic")]
pub fn get_generic_return_types(name: &str) -> GenericReturnTypes<'_> {
    let re_count = RE_GENERIC_COUNT.get_or_init(|| Regex::new(r"`(\d+)").unwrap());
    let re_params = RE_GENERIC_PARAMS.get_or_init(|| Regex::new(r"<(.*?)>").unwrap());

    let types = re_count
        .captures(name)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<usize>().ok())
        .unwrap_or(0);

    let names = re_params
        .captures(name)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().split(", ").collect::<Vec<_>>())
        .unwrap_or_default();

    GenericReturnTypes { names, types }
}

/// Returns `Some(NativeType::Struct([field_types...]))` when `signature` names a WinRT struct
/// that must be passed by value in the FFI call frame, or `None` for classes/interfaces/enums.
///
/// WinRT structs are passed by value (not by pointer) as IN parameters. Using `NativeType::Pointer`
/// for them causes libffi to pass the heap address instead of the struct bytes, yielding garbage.
pub(crate) fn struct_native_type_for_sig(signature: &str) -> Option<NativeType> {
    let sig = signature.trim();
    // Out (ByRef) parameters are always passed as a caller-allocated pointer.
    if sig.starts_with("ByRef ") {
        return None;
    }
    // Only fully-qualified dotted names can be struct types.
    if !sig.contains('.') {
        return None;
    }
    // Strip any closed generic arguments ("IFoo`1<T>" → "IFoo`1").
    let lookup = strip_generic_suffix(sig);
    let declaration = MetadataReader::find_by_name(lookup)?;
    let lock = declaration.read();
    let struct_dec = lock.as_any().downcast_ref::<StructDeclaration>()?;
    let mut field_types: Vec<NativeType> = Vec::new();
    for field in struct_dec.fields() {
        let metadata = field.base().metadata()?;
        let field_type_str = Signature::to_string(metadata, &field.type_());
        let native_type = NativeType::try_from(field_type_str.as_str()).ok()?;
        field_types.push(native_type);
    }
    if field_types.is_empty() {
        return None;
    }
    Some(NativeType::Struct(field_types.into_boxed_slice()))
}

/// If `sig` is `Windows.Foundation.IReference`1<T>` (optionally prefixed with `ByRef `),
/// returns the inner type name `T` as a `&str` slice into `sig`.
/// Used to select the correct `PropertyValue::Create*` overload when boxing primitives.
pub fn ireference_inner_type(sig: &str) -> Option<&str> {
    let inner = sig.trim().strip_prefix("ByRef ").unwrap_or(sig.trim());
    let rest = inner.strip_prefix("Windows.Foundation.IReference`1<")?;
    rest.strip_suffix('>')
}

/// Whether the call layout can carry a parameter of this signature. The ABI passes an array as
/// `(count, pointer)`, or pointers to both for a received (`ByRef`) array. Only byte arrays get
/// that pair here, and any other array would shift every later argument.
pub(crate) fn parameter_layout_supported(signature: &str) -> bool {
    let signature = signature.trim();
    if let Some(received) = signature.strip_prefix("ByRef ").and_then(|s| s.strip_suffix("[]")) {
        return crate::classify_array(received).is_some();
    }
    match signature.strip_suffix("[]") {
        None => true,
        Some(element) => {
            ArrayElement::from_name(element).is_some()
                || ReferenceElement::of_signature(signature).is_some()
        }
    }
}

/// The element of a `String[]` or object-array parameter the caller passes or the callee fills:
/// how a filled element converts back, and the interface each passed object is queried for.
pub(crate) struct ReferenceElement {
    pub(crate) kind: crate::ReturnKind,
    pub(crate) type_name: std::sync::Arc<str>,
    /// `None` for strings.
    pub(crate) iid: Option<windows::core::GUID>,
}

thread_local! {
    static REFERENCE_ELEMENTS: std::cell::RefCell<
        ahash::AHashMap<String, Option<std::rc::Rc<ReferenceElement>>>,
    > = std::cell::RefCell::new(ahash::AHashMap::new());
}

impl ReferenceElement {
    /// The element of a pass or fill `T[]` signature whose `T` is a string or an object type.
    pub(crate) fn of_signature(signature: &str) -> Option<std::rc::Rc<Self>> {
        let signature = signature.trim();
        if signature.starts_with("ByRef ") || !signature.ends_with("[]") {
            return None;
        }
        if let Some(cached) = REFERENCE_ELEMENTS.with(|c| c.borrow().get(signature).cloned()) {
            return cached;
        }
        let element = Self::resolve(&signature[..signature.len() - 2]).map(std::rc::Rc::new);
        REFERENCE_ELEMENTS.with(|c| c.borrow_mut().insert(signature.to_string(), element.clone()));
        element
    }

    fn resolve(element: &str) -> Option<Self> {
        use crate::ReturnKind;
        use metadata::declarations::class_declaration::ClassDeclaration;
        use metadata::declarations::declaration::DeclarationKind;
        use metadata::declarations::interface_declaration::generic_interface_instance_declaration::GenericInterfaceInstanceDeclaration;
        use metadata::declarations::interface_declaration::InterfaceDeclaration;
        use windows::core::Interface;
        let kind = crate::classify_return(element, false);
        let iid = match &kind {
            ReturnKind::Primitive(NativeType::String) => None,
            ReturnKind::DynamicObject => Some(windows::core::IInspectable::IID),
            ReturnKind::Object { decl, .. } | ReturnKind::InterfaceObject { decl, .. } => {
                let lock = decl.read();
                let iid = match lock.kind() {
                    DeclarationKind::Class => lock
                        .as_any()
                        .downcast_ref::<ClassDeclaration>()
                        .and_then(|class| class.default_interface())
                        .map(|iface| iface.id()),
                    DeclarationKind::Interface => {
                        lock.as_any().downcast_ref::<InterfaceDeclaration>().map(|i| i.id())
                    }
                    DeclarationKind::GenericInterfaceInstance => lock
                        .as_any()
                        .downcast_ref::<GenericInterfaceInstanceDeclaration>()
                        .map(|i| i.id()),
                    _ => None,
                };
                Some(iid?)
            }
            _ => return None,
        };
        Some(Self { kind, type_name: std::sync::Arc::from(element), iid })
    }
}

/// The native buffer behind a `String[]` or object-array argument: the strings or (queried)
/// objects passed in, or zeroed slots the callee fills. Dropping it releases what it holds.
pub(crate) enum ReferenceArray {
    Strings(Vec<windows::core::HSTRING>),
    Objects(Vec<Option<windows::core::IUnknown>>),
    Fill(Vec<usize>),
}

impl ReferenceArray {
    pub(crate) fn count(&self) -> u32 {
        (match self {
            Self::Strings(v) => v.len(),
            Self::Objects(v) => v.len(),
            Self::Fill(v) => v.len(),
        }) as u32
    }

    /// The element buffer: `HSTRING` and `Option<IUnknown>` are pointer-sized, so each `Vec` is
    /// the `T*` the ABI expects.
    pub(crate) fn pointer(&mut self) -> *mut std::ffi::c_void {
        match self {
            Self::Strings(v) => v.as_mut_ptr() as *mut std::ffi::c_void,
            Self::Objects(v) => v.as_mut_ptr() as *mut std::ffi::c_void,
            Self::Fill(v) => v.as_mut_ptr() as *mut std::ffi::c_void,
        }
    }
}

/// The array a boxed `IReferenceArray<T>` holds: its conversion, length and WinRT-allocated
/// data, which the caller converts and frees. `None` for a scalar box.
pub(crate) fn boxed_array(
    value: &windows::Foundation::IPropertyValue,
    kind: windows::Foundation::PropertyType,
) -> Option<(crate::ReturnKind, u32, *mut std::ffi::c_void)> {
    use windows::core::Interface;
    use windows::Foundation::PropertyType as P;
    type GetArray = unsafe extern "system" fn(
        *mut std::ffi::c_void,
        *mut u32,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    let v = value.vtable();
    let (element, get) = match kind {
        P::UInt8Array => ("UInt8", v.GetUInt8Array as usize),
        P::Int16Array => ("Int16", v.GetInt16Array as usize),
        P::UInt16Array => ("UInt16", v.GetUInt16Array as usize),
        P::Int32Array => ("Int32", v.GetInt32Array as usize),
        P::UInt32Array => ("UInt32", v.GetUInt32Array as usize),
        P::Int64Array => ("Int64", v.GetInt64Array as usize),
        P::UInt64Array => ("UInt64", v.GetUInt64Array as usize),
        P::SingleArray => ("Single", v.GetSingleArray as usize),
        P::DoubleArray => ("Double", v.GetDoubleArray as usize),
        P::Char16Array => ("Char16", v.GetChar16Array as usize),
        P::BooleanArray => ("Boolean", v.GetBooleanArray as usize),
        P::StringArray => ("String", v.GetStringArray as usize),
        P::InspectableArray => ("Object", v.GetInspectableArray as usize),
        P::GuidArray => ("Guid", v.GetGuidArray as usize),
        P::DateTimeArray => ("Windows.Foundation.DateTime", v.GetDateTimeArray as usize),
        P::TimeSpanArray => ("Windows.Foundation.TimeSpan", v.GetTimeSpanArray as usize),
        P::PointArray => ("Windows.Foundation.Point", v.GetPointArray as usize),
        P::SizeArray => ("Windows.Foundation.Size", v.GetSizeArray as usize),
        P::RectArray => ("Windows.Foundation.Rect", v.GetRectArray as usize),
        _ => return None,
    };
    let kind = crate::classify_array(element)?;
    // Every Get*Array slot has the same (length, data) out-parameter ABI.
    let get = unsafe { std::mem::transmute::<usize, GetArray>(get) };
    let mut length = 0u32;
    let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { get(value.as_raw(), &mut length, &mut data) }.ok().ok()?;
    Some((kind, length, data))
}

/// The element of a received array parameter (`ByRef T[]`), which takes two ABI slots.
pub(crate) fn received_array_element(signature: &str) -> Option<&str> {
    signature.trim().strip_prefix("ByRef ")?.strip_suffix("[]")
}

/// Element type of a WinRT array parameter (`Int32[]`) the call layout carries as
/// `(count, pointer)` over JS-owned memory: a typed array of the matching kind, an
/// ArrayBuffer, a DataView or a plain array (copied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArrayElement {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Bool,
    Char16,
}

impl ArrayElement {
    pub(crate) fn from_name(element: &str) -> Option<Self> {
        Some(match element {
            "UInt8" | "Uint8" | "Byte" => Self::U8,
            "Int8" | "SByte" => Self::I8,
            "UInt16" => Self::U16,
            "Int16" => Self::I16,
            "UInt32" => Self::U32,
            "Int32" => Self::I32,
            "UInt64" => Self::U64,
            "Int64" => Self::I64,
            "Single" => Self::F32,
            "Double" => Self::F64,
            "Boolean" => Self::Bool,
            "Char16" => Self::Char16,
            _ => return None,
        })
    }

    pub(crate) fn from_native(native_type: &NativeType) -> Option<Self> {
        Some(match native_type {
            NativeType::U8 => Self::U8,
            NativeType::I8 => Self::I8,
            NativeType::U16 => Self::U16,
            NativeType::I16 => Self::I16,
            NativeType::U32 => Self::U32,
            NativeType::I32 => Self::I32,
            NativeType::U64 => Self::U64,
            NativeType::I64 => Self::I64,
            NativeType::F32 => Self::F32,
            NativeType::F64 => Self::F64,
            NativeType::Bool => Self::Bool,
            _ => return None,
        })
    }

    /// The element type of a pass or fill array signature; `None` for anything else,
    /// including received (`ByRef`) arrays.
    pub(crate) fn of_signature(signature: &str) -> Option<Self> {
        signature.trim().strip_suffix("[]").and_then(Self::from_name)
    }

    pub(crate) fn size(self) -> usize {
        match self {
            Self::U8 | Self::I8 | Self::Bool => 1,
            Self::U16 | Self::I16 | Self::Char16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    pub(crate) fn winrt_name(self) -> &'static str {
        match self {
            Self::U8 => "UInt8",
            Self::I8 => "Int8",
            Self::U16 => "UInt16",
            Self::I16 => "Int16",
            Self::U32 => "UInt32",
            Self::I32 => "Int32",
            Self::U64 => "UInt64",
            Self::I64 => "Int64",
            Self::F32 => "Single",
            Self::F64 => "Double",
            Self::Bool => "Boolean",
            Self::Char16 => "Char16",
        }
    }

    /// The error for a value that can't back an array of this element.
    pub(crate) fn mismatch(self) -> crate::error::AnyError {
        crate::error::type_error(format!(
            "Expected {}, an ArrayBuffer, a plain array or null for parameter type {}[]",
            self.typed_array_with_article(),
            self.winrt_name()
        ))
    }

    /// The error for a plain array passed where the callee fills the array.
    pub(crate) fn fill_mismatch(self) -> crate::error::AnyError {
        crate::error::type_error(format!(
            "The {}[] parameter is filled by the callee: pass {} (interop.Array.create('{}', n))",
            self.winrt_name(),
            self.typed_array_with_article(),
            self.winrt_name()
        ))
    }

    fn typed_array_with_article(self) -> String {
        let name = self.typed_array_name();
        let article = if name.starts_with('I') { "an" } else { "a" };
        format!("{article} {name}")
    }

    /// The typed array JS uses for this element.
    pub(crate) fn typed_array_name(self) -> &'static str {
        match self {
            Self::U8 | Self::Bool => "Uint8Array",
            Self::I8 => "Int8Array",
            Self::U16 | Self::Char16 => "Uint16Array",
            Self::I16 => "Int16Array",
            Self::U32 => "Uint32Array",
            Self::I32 => "Int32Array",
            Self::U64 => "BigUint64Array",
            Self::I64 => "BigInt64Array",
            Self::F32 => "Float32Array",
            Self::F64 => "Float64Array",
        }
    }

    /// Whether a typed array of `kind` may back an array of this element: the matching kind,
    /// or any byte-sized view for a byte-sized element.
    pub(crate) fn accepts_view(self, kind: TypedArrayKind) -> bool {
        use TypedArrayKind as K;
        match self {
            Self::U8 | Self::I8 | Self::Bool => matches!(kind, K::U8 | K::U8Clamped | K::I8),
            Self::U16 | Self::Char16 => kind == K::U16,
            Self::I16 => kind == K::I16,
            Self::U32 => kind == K::U32,
            Self::I32 => kind == K::I32,
            Self::U64 => kind == K::U64,
            Self::I64 => kind == K::I64,
            Self::F32 => kind == K::F32,
            Self::F64 => kind == K::F64,
        }
    }

    /// Appends one element converted from a JS number, as the matching typed array would
    /// store it.
    pub(crate) fn push_number(self, out: &mut Vec<u8>, number: f64) {
        let integer = |bits: u32| -> u64 {
            if !number.is_finite() {
                return 0;
            }
            let modulus = 2f64.powi(bits as i32);
            let wrapped = number.trunc() % modulus;
            let wrapped = if wrapped < 0.0 { wrapped + modulus } else { wrapped };
            wrapped as u64
        };
        match self {
            Self::U8 | Self::I8 => out.push(integer(8) as u8),
            Self::Bool => out.push((number != 0.0 && !number.is_nan()) as u8),
            Self::U16 | Self::I16 | Self::Char16 => {
                out.extend_from_slice(&(integer(16) as u16).to_le_bytes())
            }
            Self::U32 | Self::I32 => out.extend_from_slice(&(integer(32) as u32).to_le_bytes()),
            Self::U64 | Self::I64 => out.extend_from_slice(&(number as i64).to_le_bytes()),
            Self::F32 => out.extend_from_slice(&(number as f32).to_le_bytes()),
            Self::F64 => out.extend_from_slice(&number.to_le_bytes()),
        }
    }

    /// Appends one 64-bit element given as a BigInt.
    pub(crate) fn push_bigint(self, out: &mut Vec<u8>, value: u64) {
        match self {
            Self::U64 | Self::I64 => out.extend_from_slice(&value.to_le_bytes()),
            _ => self.push_number(out, value as i64 as f64),
        }
    }
}

/// A JS value marshaled for a `T[]` parameter: JS-owned memory the callee reads or fills in
/// place, or a copy of a plain array.
pub(crate) struct ArrayArg {
    pub(crate) pointer: *mut std::ffi::c_void,
    pub(crate) count: u32,
    pub(crate) byte_length: u32,
    pub(crate) copy: Option<Vec<u8>>,
}

impl ArrayArg {
    /// Borrowed memory of `byte_length` bytes, which must hold whole elements.
    pub(crate) fn borrowed(
        pointer: *mut std::ffi::c_void,
        byte_length: u32,
        element: ArrayElement,
    ) -> Result<Self, crate::error::AnyError> {
        if byte_length as usize % element.size() != 0 {
            return Err(element.mismatch());
        }
        Ok(Self {
            pointer,
            count: byte_length / element.size() as u32,
            byte_length,
            copy: None,
        })
    }

    pub(crate) fn copied(bytes: Vec<u8>, element: ArrayElement) -> Self {
        Self {
            pointer: bytes.as_ptr() as *mut std::ffi::c_void,
            count: (bytes.len() / element.size()) as u32,
            byte_length: bytes.len() as u32,
            copy: Some(bytes),
        }
    }
}

/// The values of a JS array or typed array passed for an `Object` parameter.
pub(crate) enum ArrayValues {
    Typed(TypedArrayKind, Vec<u8>),
    Strings(Vec<windows::core::HSTRING>),
    Numbers(Vec<f64>),
    Booleans(Vec<bool>),
    Objects(Vec<Option<windows::core::IInspectable>>),
}

/// `values` boxed as the `IReferenceArray<T>` that `PropertyValue` creates, the inverse of
/// unboxing a returned array. `None` for Int8 and Float16 views, which it has no array for.
pub(crate) fn box_array(values: ArrayValues) -> Option<windows::core::IInspectable> {
    use windows::Foundation::PropertyValue as P;
    use TypedArrayKind as K;
    fn read<T, const N: usize>(bytes: &[u8], from: fn([u8; N]) -> T) -> Vec<T> {
        bytes.chunks_exact(N).map(|c| from(c.try_into().unwrap())).collect()
    }
    let boxed = match values {
        ArrayValues::Typed(kind, bytes) => match kind {
            K::U8 | K::U8Clamped => P::CreateUInt8Array(&bytes),
            K::I16 => P::CreateInt16Array(&read(&bytes, i16::from_le_bytes)),
            K::U16 => P::CreateUInt16Array(&read(&bytes, u16::from_le_bytes)),
            K::I32 => P::CreateInt32Array(&read(&bytes, i32::from_le_bytes)),
            K::U32 => P::CreateUInt32Array(&read(&bytes, u32::from_le_bytes)),
            K::I64 => P::CreateInt64Array(&read(&bytes, i64::from_le_bytes)),
            K::U64 => P::CreateUInt64Array(&read(&bytes, u64::from_le_bytes)),
            K::F32 => P::CreateSingleArray(&read(&bytes, f32::from_le_bytes)),
            K::F64 => P::CreateDoubleArray(&read(&bytes, f64::from_le_bytes)),
            K::I8 | K::Other => return None,
        },
        ArrayValues::Strings(strings) => P::CreateStringArray(&strings),
        ArrayValues::Numbers(numbers) => {
            let int32 =
                |n: &f64| n.trunc() == *n && (i32::MIN as f64..=i32::MAX as f64).contains(n);
            if numbers.iter().all(int32) {
                P::CreateInt32Array(&numbers.iter().map(|&n| n as i32).collect::<Vec<_>>())
            } else {
                P::CreateDoubleArray(&numbers)
            }
        }
        ArrayValues::Booleans(flags) => P::CreateBooleanArray(&flags),
        ArrayValues::Objects(objects) => P::CreateInspectableArray(&objects),
    };
    boxed.ok()
}

/// Typed array kinds, shared by both engines' checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypedArrayKind {
    I8,
    U8,
    U8Clamped,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
    I64,
    U64,
    Other,
}

/// Size of the scratch buffer a call writes its value-type return into.
pub(crate) const RETURN_BUFFER_SIZE: usize = 128;

/// Whether the call layout can carry a return of this type: an array return takes two out slots,
/// and a value type must fit the return buffer.
pub(crate) fn return_layout_supported(return_type: &str) -> bool {
    let return_type = return_type.trim();
    if return_type.ends_with("[]") {
        return false;
    }
    let size = struct_native_type_for_sig(return_type)
        .map_or(0, |t| t.size())
        .max(crate::property_call::sig_size_align_pub(return_type).0);
    size <= RETURN_BUFFER_SIZE
}

/// Shared mapping from WinRT signature string to FFI `NativeType`.
/// Used by `MethodCall` and `PropertyCall` during construction.
#[inline]
pub(crate) fn ffi_native_type_from_signature(signature: &str) -> NativeType {
    let signature = signature.trim();
    let by_ref_inner = signature.strip_prefix("ByRef ").unwrap_or(signature);

    if let Some(element) = by_ref_inner.strip_suffix("[]") {
        let pass_or_fill = by_ref_inner.len() == signature.len();
        return match ArrayElement::from_name(element) {
            Some(_) => NativeType::Buffer,
            None if pass_or_fill && ReferenceElement::of_signature(signature).is_some() => {
                NativeType::Buffer
            }
            None => NativeType::Pointer,
        };
    }

    // EventRegistrationToken passed by value in remove_* methods — treat as i64.
    // "ByRef Windows.Foundation.EventRegistrationToken" (add out-param) stays as Pointer below.
    if signature == "Windows.Foundation.EventRegistrationToken" {
        return NativeType::I64;
    }

    if by_ref_inner.starts_with("Var!")
        || by_ref_inner.contains('.')
        || by_ref_inner == "Object"
        || by_ref_inner == "Guid"
    {
        return NativeType::Pointer;
    }

    match by_ref_inner {
        "Void" => NativeType::Void,
        "String" => NativeType::Pointer,
        "Char16" => NativeType::U16,
        "Boolean" => NativeType::Bool,
        "UInt8" | "Uint8" | "Byte" => NativeType::U8,
        "Int8" | "SByte" => NativeType::I8,
        "UInt16" => NativeType::U16,
        "UInt32" => NativeType::U32,
        "UInt64" => NativeType::U64,
        "Int16" => NativeType::I16,
        "Int32" => NativeType::I32,
        "Int64" => NativeType::I64,
        "Single" => NativeType::F32,
        "Double" => NativeType::F64,
        _ => NativeType::Pointer,
    }
}

#[cfg(test)]
mod array_element_tests {
    use super::{parameter_layout_supported, ArrayArg, ArrayElement, TypedArrayKind};

    fn bytes(element: ArrayElement, numbers: &[f64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &n in numbers {
            element.push_number(&mut out, n);
        }
        out
    }

    #[test]
    fn signatures() {
        assert_eq!(ArrayElement::of_signature("Int32[]"), Some(ArrayElement::I32));
        assert_eq!(ArrayElement::of_signature("Char16[]"), Some(ArrayElement::Char16));
        assert_eq!(ArrayElement::of_signature("ByRef Int32[]"), None);
        assert_eq!(ArrayElement::of_signature("String[]"), None);
        assert!(parameter_layout_supported("Double[]"));
        assert!(!parameter_layout_supported("Var!0[]"));
    }

    #[test]
    fn numbers_convert_like_typed_arrays() {
        assert_eq!(bytes(ArrayElement::U8, &[257.0, -1.0, 3.9, f64::NAN]), [1, 255, 3, 0]);
        assert_eq!(bytes(ArrayElement::I32, &[-1.0]), (-1i32).to_le_bytes());
        assert_eq!(bytes(ArrayElement::U32, &[4294967297.0]), 1u32.to_le_bytes());
        assert_eq!(bytes(ArrayElement::I16, &[-2.0]), (-2i16).to_le_bytes());
        assert_eq!(bytes(ArrayElement::Bool, &[2.0, 0.0]), [1, 0]);
        assert_eq!(bytes(ArrayElement::F32, &[0.5]), 0.5f32.to_le_bytes());
        assert_eq!(bytes(ArrayElement::I64, &[-5.0]), (-5i64).to_le_bytes());
        let mut out = Vec::new();
        ArrayElement::I64.push_bigint(&mut out, (-7i64) as u64);
        assert_eq!(out, (-7i64).to_le_bytes());
    }

    #[test]
    fn counts_are_elements_not_bytes() {
        let doubles = bytes(ArrayElement::F64, &[1.0, 2.0, 3.0]);
        let copied = ArrayArg::copied(doubles, ArrayElement::F64);
        assert_eq!((copied.count, copied.byte_length), (3, 24));
        let borrowed = ArrayArg::borrowed(std::ptr::null_mut(), 12, ArrayElement::I32);
        let borrowed = borrowed.ok().unwrap();
        assert_eq!(borrowed.count, 3);
        assert!(ArrayArg::borrowed(std::ptr::null_mut(), 6, ArrayElement::I32).is_err());
    }

    #[test]
    fn received_arrays_classify_by_element() {
        use crate::ReturnKind;
        let size = |element: &str| match crate::classify_array(element) {
            Some(ReturnKind::Array { size, .. }) => Some(size),
            _ => None,
        };
        assert_eq!(size("Int32"), Some(4));
        assert_eq!(size("Boolean"), Some(1));
        assert_eq!(size("String"), Some(std::mem::size_of::<usize>()));
        assert_eq!(size("Object"), Some(std::mem::size_of::<usize>()));
        assert_eq!(size("Windows.Foundation.Uri"), Some(std::mem::size_of::<usize>()));
        assert_eq!(size("Windows.Foundation.Point"), Some(8));
        assert_eq!(size("Guid"), Some(16));
        assert_eq!(size("Var!0"), None);
        assert!(parameter_layout_supported("ByRef Int32[]"));
        assert!(parameter_layout_supported("ByRef Windows.Foundation.Uri[]"));
        assert!(!parameter_layout_supported("ByRef Var!0[]"));
    }

    #[test]
    fn boxed_arrays_round_trip() {
        use super::{box_array, boxed_array, ArrayValues};
        use windows::core::{Interface, HSTRING};
        use windows::Foundation::IPropertyValue;
        let strings = vec![HSTRING::from("a"), HSTRING::from("bc")];
        let boxed = box_array(ArrayValues::Strings(strings)).unwrap();
        let value: IPropertyValue = boxed.cast().unwrap();
        let kind = unsafe { value.Type() }.unwrap();
        let (_, length, data) = boxed_array(&value, kind).unwrap();
        assert_eq!(length, 2);
        let items = unsafe { std::slice::from_raw_parts(data as *const usize, 2) };
        let read: Vec<String> = items
            .iter()
            .map(|&raw| unsafe { std::mem::transmute::<usize, HSTRING>(raw) }.to_string_lossy())
            .collect();
        assert_eq!(read, ["a", "bc"]);
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(data as *const _)) };

        let numbers = box_array(ArrayValues::Numbers(vec![1.0, 2.5])).unwrap();
        let value: IPropertyValue = numbers.cast().unwrap();
        let kind = unsafe { value.Type() }.unwrap();
        assert_eq!(kind, windows::Foundation::PropertyType::DoubleArray);
        let ints = box_array(ArrayValues::Numbers(vec![1.0, -2.0])).unwrap();
        let value: IPropertyValue = ints.cast().unwrap();
        assert_eq!(unsafe { value.Type() }.unwrap(), windows::Foundation::PropertyType::Int32Array);
        assert!(box_array(ArrayValues::Typed(TypedArrayKind::I8, vec![1])).is_none());
    }

    #[test]
    fn string_and_object_arrays_are_reference_elements() {
        use super::{ffi_native_type_from_signature, ReferenceElement};
        use crate::value::NativeType;
        use windows::core::Interface;
        let strings = ReferenceElement::of_signature("String[]").unwrap();
        assert!(strings.iid.is_none());
        let objects = ReferenceElement::of_signature("Object[]").unwrap();
        assert_eq!(objects.iid, Some(windows::core::IInspectable::IID));
        let uris = ReferenceElement::of_signature("Windows.Foundation.Uri[]").unwrap();
        assert_eq!(uris.iid, Some(windows::Foundation::Uri::IID));
        assert!(ReferenceElement::of_signature("Int32[]").is_none());
        assert!(ReferenceElement::of_signature("ByRef String[]").is_none());
        assert!(matches!(ffi_native_type_from_signature("String[]"), NativeType::Buffer));
        assert!(matches!(ffi_native_type_from_signature("ByRef String[]"), NativeType::Pointer));
        assert!(parameter_layout_supported("Windows.Data.Json.IJsonValue[]"));
    }

    #[test]
    fn views_must_match_the_element() {
        assert!(ArrayElement::I32.accepts_view(TypedArrayKind::I32));
        assert!(!ArrayElement::I32.accepts_view(TypedArrayKind::F64));
        assert!(ArrayElement::U8.accepts_view(TypedArrayKind::I8));
        assert!(ArrayElement::Char16.accepts_view(TypedArrayKind::U16));
        assert!(!ArrayElement::U64.accepts_view(TypedArrayKind::I64));
    }
}
