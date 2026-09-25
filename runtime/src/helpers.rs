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

/// Shared mapping from WinRT signature string to FFI `NativeType`.
/// Used by `MethodCall` and `PropertyCall` during construction.
#[inline]
pub(crate) fn ffi_native_type_from_signature(signature: &str) -> NativeType {
    let signature = signature.trim();
    let by_ref_inner = signature.strip_prefix("ByRef ").unwrap_or(signature);

    if let Some(element) = by_ref_inner.strip_suffix("[]") {
        return match element {
            "UInt8" | "Uint8" | "Int8" | "Byte" | "SByte" => NativeType::Buffer,
            _ => NativeType::Pointer,
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
