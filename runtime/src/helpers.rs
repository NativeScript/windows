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
    match signature.trim().strip_suffix("[]") {
        None => true,
        Some(element) => ArrayElement::from_name(element).is_some(),
    }
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
        return match ArrayElement::from_name(element) {
            Some(_) => NativeType::Buffer,
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
        assert!(!parameter_layout_supported("Windows.Foundation.Uri[]"));
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
    fn views_must_match_the_element() {
        assert!(ArrayElement::I32.accepts_view(TypedArrayKind::I32));
        assert!(!ArrayElement::I32.accepts_view(TypedArrayKind::F64));
        assert!(ArrayElement::U8.accepts_view(TypedArrayKind::I8));
        assert!(ArrayElement::Char16.accepts_view(TypedArrayKind::U16));
        assert!(!ArrayElement::U64.accepts_view(TypedArrayKind::I64));
    }
}
