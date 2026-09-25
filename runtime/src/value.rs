use std::ffi::c_void;
use std::mem::ManuallyDrop;

#[cfg(feature = "classic")]
use crate::dotnet::call_dotnet;
use crate::error::*;
#[cfg(feature = "classic")]
use crate::DeclarationFFI;
use libffi::low::*;
use libffi::middle::Arg;
use windows::core::HSTRING;
#[cfg(feature = "classic")]
use windows::core::{IUnknown, Interface, GUID};
#[cfg(feature = "classic")]
use windows::Foundation::PropertyValue;

pub(crate) const MAX_SAFE_INTEGER: isize = 9007199254740991;
pub(crate) const MIN_SAFE_INTEGER: isize = -9007199254740991;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeType {
    Void,
    Bool,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    USize,
    ISize,
    F32,
    F64,
    Pointer,
    Buffer,
    Function,
    Struct(Box<[NativeType]>),
    String,
}

impl NativeType {
    pub fn size(&self) -> usize {
        unsafe {
            match self {
                NativeType::Void => types::void.size,
                NativeType::Bool | NativeType::U8 => types::uint8.size,
                NativeType::I8 => types::sint8.size,
                NativeType::U16 => types::uint16.size,
                NativeType::I16 => types::sint16.size,
                NativeType::U32 => types::uint32.size,
                NativeType::I32 => types::sint32.size,
                NativeType::U64 => types::uint64.size,
                NativeType::I64 => types::sint64.size,
                NativeType::USize | NativeType::ISize => {
                    // Both are pointer-sized integers; use compile-time constant instead of
                    // constructing a libffi Type object on every size() call.
                    std::mem::size_of::<usize>()
                }
                NativeType::F32 => types::float.size,
                NativeType::F64 => types::double.size,
                NativeType::Pointer | NativeType::String => types::pointer.size,
                NativeType::Buffer => types::pointer.size,
                NativeType::Function => types::pointer.size,
                NativeType::Struct(ref value) => {
                    // Prefer libffi's computed struct size (handles alignment/padding).
                    // Fall back to naive sum of field sizes if Type construction fails.
                    let try_size = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut fields_vec: Vec<libffi::middle::Type> = Vec::new();
                        for f in value.iter() {
                            match std::convert::TryFrom::try_from(f.clone()) {
                                Ok(t) => fields_vec.push(t),
                                Err(_) => return None,
                            }
                        }
                        let s = libffi::middle::Type::structure(fields_vec);
                        let raw = s.as_raw_ptr();
                        Some(unsafe { (*raw).size })
                    }));

                    if let Ok(Some(s)) = try_size {
                        s
                    } else {
                        let mut size = 0_usize;
                        for native_type in value.iter() {
                            size = size + native_type.size();
                        }
                        size
                    }
                }
            }
        }
    }
}

impl TryFrom<NativeType> for libffi::middle::Type {
    type Error = AnyError;

    fn try_from(native_type: NativeType) -> std::result::Result<Self, Self::Error> {
        Ok(match native_type {
            NativeType::Void => libffi::middle::Type::void(),
            NativeType::U8 | NativeType::Bool => libffi::middle::Type::u8(),
            NativeType::I8 => libffi::middle::Type::i8(),
            NativeType::U16 => libffi::middle::Type::u16(),
            NativeType::I16 => libffi::middle::Type::i16(),
            NativeType::U32 => libffi::middle::Type::u32(),
            NativeType::I32 => libffi::middle::Type::i32(),
            NativeType::U64 => libffi::middle::Type::u64(),
            NativeType::I64 => libffi::middle::Type::i64(),
            NativeType::USize => libffi::middle::Type::usize(),
            NativeType::ISize => libffi::middle::Type::isize(),
            NativeType::F32 => libffi::middle::Type::f32(),
            NativeType::F64 => libffi::middle::Type::f64(),
            NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                libffi::middle::Type::pointer()
            }
            NativeType::String => {
                // HSTRING is an opaque pointer-sized handle at the ABI layer.
                // Use libffi's pointer type so the callee sees a `void*`.
                libffi::middle::Type::pointer()
            }
            NativeType::Struct(fields) => libffi::middle::Type::structure(match fields.len() > 0 {
                true => fields
                    .iter()
                    .map(|field| field.clone().try_into())
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                false => {
                    return Err(type_error("Struct must have at least one field"));
                }
            }),
        })
    }
}

impl TryFrom<&str> for NativeType {
    type Error = AnyError;

    fn try_from(native_type: &str) -> std::result::Result<Self, Self::Error> {
        let signature = native_type.trim();
        let by_ref_inner = signature.strip_prefix("ByRef ").unwrap_or(signature);

        // Array signatures are represented as "T[]" by metadata::Signature.
        // Byte arrays are best treated as buffer sources in JS; all other arrays
        // are opaque pointers in this bridge.
        if let Some(element) = by_ref_inner.strip_suffix("[]") {
            return Ok(match element {
                "UInt8" | "Uint8" | "Int8" | "Byte" | "SByte" => NativeType::Buffer,
                _ => NativeType::Pointer,
            });
        }

        // Generic vars and non-primitive named WinRT types are pointer-marshalled.
        if by_ref_inner.starts_with("Var!")
            || by_ref_inner.contains('.')
            || by_ref_inner == "Object"
            || by_ref_inner == "Guid"
        {
            return Ok(NativeType::Pointer);
        }

        Ok(match by_ref_inner {
            "Void" => NativeType::Void,
            "UInt8" | "Uint8" | "Byte" => NativeType::U8,
            "Boolean" => NativeType::Bool,
            "Int8" | "SByte" => NativeType::I8,
            "UInt16" => NativeType::U16,
            "Int16" => NativeType::I16,
            "UInt32" => NativeType::U32,
            "Int32" | "IntI32" => NativeType::I32,
            "UInt64" => NativeType::U64,
            "Int64" => NativeType::I64,
            "USize" => NativeType::USize,
            "ISize" => NativeType::ISize,
            "Single" => NativeType::F32,
            "Double" => NativeType::F64,
            "String" => NativeType::String,
            "Char16" => NativeType::U16,
            _ => {
                return Err(type_error("Unsupported type"));
            }
        })
    }
}

#[repr(C)]
pub union NativeValue {
    pub void_value: (),
    pub bool_value: bool,
    pub u8_value: u8,
    pub i8_value: i8,
    pub u16_value: u16,
    pub i16_value: i16,
    pub u32_value: u32,
    pub i32_value: i32,
    pub u64_value: u64,
    pub i64_value: i64,
    pub usize_value: usize,
    pub isize_value: isize,
    pub f32_value: f32,
    pub f64_value: f64,
    pub pointer: *mut c_void,
    pub string: ManuallyDrop<HSTRING>,
}

impl NativeValue {
    pub unsafe fn as_arg(&self, native_type: &NativeType) -> Arg<'_> {
        match native_type {
            // Void should never be marshalled as an argument, but return a stable
            // placeholder to avoid process aborts in malformed metadata scenarios.
            NativeType::Void => Arg::new(&self.u8_value),
            NativeType::Bool => Arg::new(&self.bool_value),
            NativeType::U8 => Arg::new(&self.u8_value),
            NativeType::I8 => Arg::new(&self.i8_value),
            NativeType::U16 => Arg::new(&self.u16_value),
            NativeType::I16 => Arg::new(&self.i16_value),
            NativeType::U32 => Arg::new(&self.u32_value),
            NativeType::I32 => Arg::new(&self.i32_value),
            NativeType::U64 => Arg::new(&self.u64_value),
            NativeType::I64 => Arg::new(&self.i64_value),
            NativeType::USize => Arg::new(&self.usize_value),
            NativeType::ISize => Arg::new(&self.isize_value),
            NativeType::F32 => Arg::new(&self.f32_value),
            NativeType::F64 => Arg::new(&self.f64_value),
            NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                Arg::new(&self.pointer)
            }
            NativeType::Struct(_) => Arg::new(&*self.pointer),
            NativeType::String => {
                // HSTRING must be passed by value (handle-sized) on WinRT x64.
                // The union stores the HSTRING in the same memory as `usize_value`,
                // so take the address of `usize_value` to provide a stable
                // handle-sized reference for libffi without creating temporaries.
                Arg::new(&self.usize_value)
            }
        }
    }

    // SAFETY: native_type must correspond to the type of value represented by the union field
    #[cfg(feature = "classic")]
    #[inline]
    pub unsafe fn to_v8<'a>(
        &'a self,
        scope: &mut v8::PinScope<'a, '_>,
        native_type: NativeType,
    ) -> v8::Local<'a, v8::Value> {
        let value = match native_type {
            NativeType::Void => {
                let local_value: v8::Local<v8::Value> = v8::undefined(scope).into();
                local_value
            }
            NativeType::Bool => {
                let local_value: v8::Local<v8::Value> =
                    v8::Boolean::new(scope, self.bool_value).into();
                local_value
            }
            NativeType::U8 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new_from_unsigned(scope, self.u8_value as u32).into();
                local_value
            }
            NativeType::I8 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new(scope, self.i8_value as i32).into();
                local_value
            }
            NativeType::U16 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new_from_unsigned(scope, self.u16_value as u32).into();
                local_value
            }
            NativeType::I16 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new(scope, self.i16_value as i32).into();
                local_value
            }
            NativeType::U32 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new_from_unsigned(scope, self.u32_value).into();
                local_value
            }
            NativeType::I32 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Integer::new(scope, self.i32_value).into();
                local_value
            }
            NativeType::U64 => {
                let value = self.u64_value;
                let local_value: v8::Local<v8::Value> = if value > MAX_SAFE_INTEGER as u64 {
                    v8::BigInt::new_from_u64(scope, value).into()
                } else {
                    v8::Number::new(scope, value as f64).into()
                };
                local_value
            }
            NativeType::I64 => {
                let value = self.i64_value;
                let local_value: v8::Local<v8::Value> =
                    if value > MAX_SAFE_INTEGER as i64 || value < MIN_SAFE_INTEGER as i64 {
                        v8::BigInt::new_from_i64(scope, self.i64_value).into()
                    } else {
                        v8::Number::new(scope, value as f64).into()
                    };
                local_value
            }
            NativeType::USize => {
                let value = self.usize_value;
                let local_value: v8::Local<v8::Value> = if value > MAX_SAFE_INTEGER as usize {
                    v8::BigInt::new_from_u64(scope, value as u64).into()
                } else {
                    v8::Number::new(scope, value as f64).into()
                };
                local_value
            }
            NativeType::ISize => {
                let value = self.isize_value;
                let local_value: v8::Local<v8::Value> =
                    if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
                        v8::BigInt::new_from_i64(scope, self.isize_value as i64).into()
                    } else {
                        v8::Number::new(scope, value as f64).into()
                    };
                local_value
            }
            NativeType::F32 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Number::new(scope, self.f32_value as f64).into();
                local_value
            }
            NativeType::F64 => {
                let local_value: v8::Local<v8::Value> =
                    v8::Number::new(scope, self.f64_value).into();
                local_value
            }
            NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
                let local_value: v8::Local<v8::Value> = if self.pointer.is_null() {
                    v8::null(scope).into()
                } else {
                    v8::External::new(scope, self.pointer).into()
                };
                local_value
            }
            NativeType::Struct(_) => {
                let local_value: v8::Local<v8::Value> = v8::null(scope).into();
                local_value
            }
            NativeType::String => {
                let local_value: v8::Local<v8::Value> =
                    v8::String::new_from_two_byte(scope, &*self.string, v8::NewStringType::Normal)
                        .unwrap()
                        .into();
                local_value
            }
        };

        value
    }
}

// SAFETY: NativeValue is only used on the V8 thread; raw pointer fields are not
// accessed concurrently.
unsafe impl Send for NativeValue {}

/// Marshal a V8 string to an HSTRING by copying UTF-16 code units straight across, skipping the
/// UTF-16→UTF-8→UTF-16 double transcode (and the intermediate Rust String allocation) that
/// `to_rust_string_lossy()` + `HSTRING::from()` incur. V8 strings and HSTRING are both UTF-16, so
/// `write_v2` fills a u16 buffer and `HSTRING::from_wide` copies it in — one transcode-free copy each
/// way.
#[cfg(feature = "classic")]
#[inline]
fn hstring_from_v8_string(scope: &mut v8::PinScope<'_, '_>, s: v8::Local<v8::String>) -> HSTRING {
    let len = s.length();
    if len == 0 {
        return HSTRING::new();
    }
    let mut buf: Vec<u16> = vec![0u16; len];
    s.write_v2(scope, 0, &mut buf, v8::WriteFlags::empty());
    HSTRING::from_wide(&buf)
}

/// As `hstring_from_v8_string`, but coerces a non-string JS value via ToString first (matching
/// `Value::to_rust_string_lossy`'s behavior). Empty HSTRING if coercion fails.
#[cfg(feature = "classic")]
#[inline]
fn hstring_from_v8_value(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> HSTRING {
    match value.to_string(scope) {
        Some(s) => hstring_from_v8_string(scope, s),
        None => HSTRING::new(),
    }
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // If the JS value carries an `__hstring_ptr` External (set by
    // `create_hstring_backed_js_value`), reuse it to avoid reallocation.
    if arg.is_object() {
        if let Some(obj) = arg.to_object(scope) {
            if let Some(key) = v8::String::new(scope, "__hstring_ptr") {
                if let Some(val) = obj.get(scope, key.into()) {
                    if let Ok(ext) = v8::Local::<v8::External>::try_from(val) {
                        let raw = ext.value() as *const HSTRING;
                        if !raw.is_null() {
                            let hclone = unsafe { (*raw).clone() };
                            return Ok(NativeValue {
                                string: ManuallyDrop::new(hclone),
                            });
                        }
                    }
                }
            }
        }
    }

    let string_value = v8::Local::<v8::String>::try_from(arg)
        .map_err(|_| type_error("Invalid FFI String type, expected String"))?;

    Ok(NativeValue {
        string: ManuallyDrop::new(hstring_from_v8_string(scope, string_value)),
    })
}

/// Create a JS object that carries an `HSTRING` pointer in `__hstring_ptr`.
/// This is a convenience helper; callers should avoid creating these unless
/// they expect the JS value to be passed back to native code later.
#[cfg(feature = "classic")]
pub fn create_hstring_backed_js_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    h: HSTRING,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    // Box the HSTRING so we have an address to store in the External.
    let boxed = Box::new(h);
    let ptr = Box::into_raw(boxed) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, ptr);
    let key = v8::String::new(scope, "__hstring_ptr").unwrap();
    let _ = obj.set(scope, key.into(), ext.into());
    // Also expose the string content under `value` for convenience.
    let s = unsafe { (&*(ptr as *mut HSTRING)).to_string_lossy() };
    let _ = obj.set(
        scope,
        v8::String::new(scope, "value").unwrap().into(),
        v8::String::new(scope, &s).unwrap().into(),
    );
    obj
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_bool_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    let bool_value = v8::Local::<v8::Boolean>::try_from(arg)
        .map_err(|_| type_error("Invalid FFI u8 type, expected boolean"))?
        .is_true();
    Ok(NativeValue { bool_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_u8_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    if let Ok(v) = v8::Local::<v8::Uint32>::try_from(arg) {
        return Ok(NativeValue {
            u8_value: v.value() as u8,
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= 0.0 && f <= u8::MAX as f64 {
            return Ok(NativeValue { u8_value: f as u8 });
        }
    }
    Err(type_error("Invalid FFI u8 type, expected unsigned integer"))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_i8_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    if let Ok(v) = v8::Local::<v8::Int32>::try_from(arg) {
        return Ok(NativeValue {
            i8_value: v.value() as i8,
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= i8::MIN as f64 && f <= i8::MAX as f64 {
            return Ok(NativeValue { i8_value: f as i8 });
        }
    }
    Err(type_error("Invalid FFI i8 type, expected integer"))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_u16_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    if let Ok(v) = v8::Local::<v8::Uint32>::try_from(arg) {
        return Ok(NativeValue {
            u16_value: v.value() as u16,
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= 0.0 && f <= u16::MAX as f64 {
            return Ok(NativeValue {
                u16_value: f as u16,
            });
        }
    }
    Err(type_error(
        "Invalid FFI u16 type, expected unsigned integer",
    ))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_i16_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    if let Ok(v) = v8::Local::<v8::Int32>::try_from(arg) {
        return Ok(NativeValue {
            i16_value: v.value() as i16,
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= i16::MIN as f64 && f <= i16::MAX as f64 {
            return Ok(NativeValue {
                i16_value: f as i16,
            });
        }
    }
    Err(type_error("Invalid FFI i16 type, expected integer"))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_u32_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    if let Ok(v) = v8::Local::<v8::Uint32>::try_from(arg) {
        return Ok(NativeValue {
            u32_value: v.value(),
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= 0.0 && f <= u32::MAX as f64 {
            return Ok(NativeValue {
                u32_value: f as u32,
            });
        }
    }
    Err(type_error(
        "Invalid FFI u32 type, expected unsigned integer",
    ))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_i32_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    // Accept both Smi (Int32) and HeapNumber (Number) — WinRT enum values are
    // cached as v8::Integer in the interceptor but may arrive as v8::Number when
    // stored in JS variables or passed through expressions.
    if let Ok(v) = v8::Local::<v8::Int32>::try_from(arg) {
        return Ok(NativeValue {
            i32_value: v.value(),
        });
    }
    if let Ok(v) = v8::Local::<v8::Number>::try_from(arg) {
        let f = v.value();
        if f.fract() == 0.0 && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
            return Ok(NativeValue {
                i32_value: f as i32,
            });
        }
    }
    Err(type_error("Invalid FFI i32 type, expected integer"))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_u64_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // Order of checking:
    // 1. BigInt: Uncommon and not supported by Fast API, so optimise slow call for this case.
    // 2. Number: Common, supported by Fast API, so let that be the optimal case.
    let u64_value: u64 = if let Ok(value) = v8::Local::<v8::BigInt>::try_from(arg) {
        value.u64_value().0
    } else if let Ok(value) = v8::Local::<v8::Number>::try_from(arg) {
        value.integer_value(scope).unwrap() as u64
    } else {
        return Err(type_error(
            "Invalid FFI u64 type, expected unsigned integer",
        ));
    };
    Ok(NativeValue { u64_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_i64_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // Order of checking:
    // 1. BigInt: Uncommon and not supported by Fast API, so optimise slow call for this case.
    // 2. Number: Common, supported by Fast API, so let that be the optimal case.
    let i64_value: i64 = if let Ok(value) = v8::Local::<v8::BigInt>::try_from(arg) {
        value.i64_value().0
    } else if let Ok(value) = v8::Local::<v8::Number>::try_from(arg) {
        value.integer_value(scope).unwrap()
    } else {
        return Err(type_error("Invalid FFI i64 type, expected integer"));
    };
    Ok(NativeValue { i64_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_usize_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // Order of checking:
    // 1. BigInt: Uncommon and not supported by Fast API, so optimise slow call for this case.
    // 2. Number: Common, supported by Fast API, so let that be the optimal case.
    let usize_value: usize = if let Ok(value) = v8::Local::<v8::BigInt>::try_from(arg) {
        value.u64_value().0 as usize
    } else if let Ok(value) = v8::Local::<v8::Number>::try_from(arg) {
        value.integer_value(scope).unwrap() as usize
    } else {
        return Err(type_error("Invalid FFI usize type, expected integer"));
    };
    Ok(NativeValue { usize_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_isize_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // Order of checking:
    // 1. BigInt: Uncommon and not supported by Fast API, so optimise slow call for this case.
    // 2. Number: Common, supported by Fast API, so let that be the optimal case.
    let isize_value: isize = if let Ok(value) = v8::Local::<v8::BigInt>::try_from(arg) {
        value.i64_value().0 as isize
    } else if let Ok(value) = v8::Local::<v8::Number>::try_from(arg) {
        value.integer_value(scope).unwrap() as isize
    } else {
        return Err(type_error("Invalid FFI isize type, expected integer"));
    };
    Ok(NativeValue { isize_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_f32_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    let f32_value = v8::Local::<v8::Number>::try_from(arg)
        .map_err(|_| type_error("Invalid FFI f32 type, expected number"))?
        .value() as f32;
    Ok(NativeValue { f32_value })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_f64_arg(arg: v8::Local<v8::Value>) -> std::result::Result<NativeValue, AnyError> {
    let f64_value = v8::Local::<v8::Number>::try_from(arg)
        .map_err(|_| type_error("Invalid FFI f64 type, expected number"))?
        .value();
    Ok(NativeValue { f64_value })
}

#[cfg(feature = "classic")]
#[inline]
fn try_get_external_handle(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Object>,
) -> Option<*mut c_void> {
    if let Some(handle_key) = v8::String::new(scope, "handle") {
        if let Some(handle) = arg.get(scope, handle_key.into()) {
            // If `handle` is a function (common for managed wrappers exposing
            // a getter), call it and attempt to parse its return value as a
            // native handle.  Calling a small accessor here is acceptable
            // because it's the explicit bridge contract for retrieving native
            // identity from wrapped managed objects.
            if handle.is_function() {
                if let Ok(func) = v8::Local::<v8::Function>::try_from(handle) {
                    // Call with `this` = the object so typical getters work.
                    if let Some(ret) = func.call(scope, arg.into(), &[]) {
                        if let Ok(ext) = v8::Local::<v8::External>::try_from(ret) {
                            return Some(ext.value());
                        }
                        if ret.is_null() {
                            return Some(std::ptr::null_mut());
                        }
                        if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(ret) {
                            let u = bi.u64_value().0;
                            return Some(u as *mut c_void);
                        }
                        if let Ok(num) = v8::Local::<v8::Number>::try_from(ret) {
                            if let Some(iv) = num.integer_value(scope) {
                                return Some(iv as isize as *mut c_void);
                            } else {
                                let v = num.value();
                                return Some(v as usize as *mut c_void);
                            }
                        }
                    }
                }
            }
            if let Ok(value) = v8::Local::<v8::External>::try_from(handle) {
                return Some(value.value());
            }

            if handle.is_null() {
                return Some(std::ptr::null_mut());
            }
            // Fallback: some managed bridges may expose the native pointer as a
            // numeric value (BigInt or Number). Accept those too.
            if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(handle) {
                let u = bi.u64_value().0;
                let ptr = u as *mut c_void;
                return Some(ptr);
            }
            if let Ok(num) = v8::Local::<v8::Number>::try_from(handle) {
                if let Some(iv) = num.integer_value(scope) {
                    let ptr = iv as isize as *mut c_void;
                    return Some(ptr);
                } else {
                    let v = num.value();
                    let ptr = v as usize as *mut c_void;
                    return Some(ptr);
                }
            }
        }
    }

    if let Some(dec) = arg.get_internal_field(scope, 0) {
        let dec = unsafe { dec.cast::<v8::External>() };
        let dec = dec.value() as *mut DeclarationFFI;
        let dec = unsafe { &*dec };

        if let Some(ref instance) = dec.instance {
            return Some(instance.as_raw() as *mut c_void);
        }

        // Struct objects have no COM instance; return a pointer to the raw
        // byte buffer so WinRT setters receive the struct data by reference.
        if let Some((ref buf, _)) = dec.struct_instance {
            return Some(buf.as_ptr() as *mut c_void);
        }

        return Some(std::ptr::null_mut());
    }

    // Bridge may provide a canonical native pointer directly on the JS object
    // via a `__native_ptr` property (written as a string or numeric value by
    // the managed bridge). Accept hex strings, decimal strings, BigInt, and
    // Number values here so managed-returned wrappers can expose their
    // canonical IUnknown/IInspectable pointer identity.
    if let Some(native_key) = v8::String::new(scope, "__native_ptr") {
        if let Some(val) = arg.get(scope, native_key.into()) {
            if val.is_string() {
                if let Ok(sv) = v8::Local::<v8::String>::try_from(val) {
                    let s = sv.to_rust_string_lossy(scope);
                    let s_trim = s.trim_start();
                    let parsed = if s_trim.starts_with("0x") || s_trim.starts_with("0X") {
                        usize::from_str_radix(&s_trim[2..], 16).ok()
                    } else {
                        s_trim.parse::<usize>().ok()
                    };
                    if let Some(u) = parsed {
                        return Some(u as *mut c_void);
                    }
                }
            }
            if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(val) {
                let u = bi.u64_value().0;
                return Some(u as *mut c_void);
            }
            if let Ok(num) = v8::Local::<v8::Number>::try_from(val) {
                if let Some(iv) = num.integer_value(scope) {
                    return Some(iv as isize as *mut c_void);
                } else {
                    let v = num.value();
                    return Some(v as usize as *mut c_void);
                }
            }
            if let Ok(ext) = v8::Local::<v8::External>::try_from(val) {
                return Some(ext.value());
            }
        }
    }

    // If the object exposes a managed handle id, ask the managed bridge for
    // a canonical native pointer for that handle. This is a fallback for
    // managed-created wrappers that did not carry an External or __native_ptr.
    if let Some(handle_key) = v8::String::new(scope, "__handle") {
        if let Some(val) = arg.get(scope, handle_key.into()) {
            // extract integer handle id from various JS numeric types
            let mut handle_id: Option<i32> = None;
            if let Ok(v) = v8::Local::<v8::Int32>::try_from(val) {
                handle_id = Some(v.value());
            } else if let Ok(n) = v8::Local::<v8::Number>::try_from(val) {
                if let Some(iv) = n.integer_value(scope) {
                    handle_id = Some(iv as i32);
                } else {
                    handle_id = Some(n.value() as i32);
                }
            } else if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(val) {
                handle_id = Some(bi.u64_value().0 as i32);
            } else if val.is_object() {
                // Some bridges nest the handle in an inner __handle property.
                if let Ok(obj) = v8::Local::<v8::Object>::try_from(val) {
                    if let Some(inner) = obj.get(scope, handle_key.into()) {
                        if let Ok(v) = v8::Local::<v8::Int32>::try_from(inner) {
                            handle_id = Some(v.value());
                        } else if let Ok(n) = v8::Local::<v8::Number>::try_from(inner) {
                            if let Some(iv) = n.integer_value(scope) {
                                handle_id = Some(iv as i32);
                            } else {
                                handle_id = Some(n.value() as i32);
                            }
                        } else if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(inner) {
                            handle_id = Some(bi.u64_value().0 as i32);
                        }
                    }
                }
            }

            if let Some(id) = handle_id {
                // Compose a JSON call to the managed bridge: call the static
                // Bridge.GetNativePtrForHandle(handleId) method and parse the
                // returned numeric pointer (0 means absent).
                let req = format!(
                    "{{\"assembly\":null,\"typeName\":\"NativeScriptBridge.Bridge\",\"method\":\"GetNativePtrForHandle\",\"args\":[{}]}}",
                    id
                );
                // Verbose tracing — only emit when `NS_DEBUG` is set.
                if std::env::var("NS_DEBUG").is_ok() {
                    crate::debug_output(&format!(
                        "[RUNTIME] calling bridge for native ptr of handle {}\n",
                        id
                    ));
                }
                if let Ok(resp) = call_dotnet(&req) {
                    if std::env::var("NS_DEBUG").is_ok() {
                        crate::debug_output(&format!(
                            "[RUNTIME] bridge resp for handle {}: {}\n",
                            id, resp
                        ));
                    }
                    let trimmed = resp.trim();
                    if !trimmed.is_empty() && trimmed != "null" {
                        // Try parse as integer JSON (e.g. 12345)
                        if let Ok(n) = trimmed.parse::<i64>() {
                            if n != 0 {
                                if std::env::var("NS_DEBUG").is_ok() {
                                    crate::debug_output(&format!(
                                        "[RUNTIME] parsed native ptr {} for handle {}\n",
                                        n, id
                                    ));
                                }
                                return Some(n as usize as *mut c_void);
                            }
                        } else {
                            // Maybe the bridge returned a quoted hex string
                            let s = trimmed.trim_matches('"');
                            let s_trim = s.trim_start();
                            if s_trim.starts_with("0x") || s_trim.starts_with("0X") {
                                if let Ok(u) = usize::from_str_radix(&s_trim[2..], 16) {
                                    if std::env::var("NS_DEBUG").is_ok() {
                                        crate::debug_output(&format!("[RUNTIME] parsed hex native ptr 0x{:x} for handle {}\n", u, id));
                                    }
                                    return Some(u as *mut c_void);
                                }
                            } else if let Ok(u) = s_trim.parse::<usize>() {
                                if u != 0 {
                                    if std::env::var("NS_DEBUG").is_ok() {
                                        crate::debug_output(&format!(
                                            "[RUNTIME] parsed native ptr {} for handle {}\n",
                                            u, id
                                        ));
                                    }
                                    return Some(u as *mut c_void);
                                }
                            }
                        }
                    }
                } else {
                    // Only emit the failure message when verbose debug is requested.
                    if std::env::var("NS_DEBUG").is_ok() {
                        crate::debug_output(&format!("[RUNTIME] call_dotnet failed while requesting native ptr for handle {}\n", id));
                    }
                }
            }
        }
    }

    None
}

/// Box a JS value as a concrete WinRT `IPropertyValue` for the given WinRT type name.
///
/// `PropertyValue::Create*` produces an `IInspectable` that implements **both**
/// `IPropertyValue` (concrete typed value) and `IReference<T>` (nullable wrapper),
/// so this function serves two caller intents:
///   • Overload disambiguation / typed passing to `Object` parameters.
///   • Explicit `IReference<T>` boxing for nullable XAML parameters.
///
/// `type_name` is the WinRT primitive name: `"Double"`, `"Single"`, `"Int32"`,
/// `"Char16"`, `"TimeSpan"` (accepts ms number), `"DateTime"` (accepts ms-since-epoch),
/// `"Guid"` (accepts "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" string), etc.
/// Returns `None` when the value/type combination cannot be boxed.
/// Read a JS value as i64, accepting Number or BigInt. i64 struct fields (DateTime.UniversalTime)
/// exceed 2^53, so callers pass a BigInt; `number_value()` returns 0 on a BigInt.
#[cfg(feature = "classic")]
fn v8_to_i64(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> i64 {
    if let Ok(bi) = v8::Local::<v8::BigInt>::try_from(value) {
        return bi.i64_value().0;
    }
    value.number_value(scope).unwrap_or(0.0) as i64
}

/// Read an i64 field (Number or BigInt) off a JS object; 0 if absent.
#[cfg(feature = "classic")]
fn read_i64_field(scope: &mut v8::PinScope<'_, '_>, obj_val: v8::Local<v8::Value>, field: &str) -> i64 {
    if let Some(obj) = obj_val.to_object(scope) {
        if let Some(k) = v8::String::new(scope, field) {
            if let Some(v) = obj.get(scope, k.into()) {
                return v8_to_i64(scope, v);
            }
        }
    }
    0
}

#[cfg(feature = "classic")]
pub fn box_as_typed_value(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
    type_name: &str,
) -> Option<NativeValue> {
    use windows::Foundation::PropertyValue;

    // Already-boxed value (a `PropertyValue.Create*` result or any wrapped COM value) → pass its
    // handle straight through; re-reading fields off the wrapper would yield 0 (double-boxing).
    // Plain struct objects like { Duration: 123 } have no `handle`, so this skips them.
    if arg.is_object() {
        if let Some(obj) = arg.to_object(scope) {
            if let Some(ptr) = try_get_external_handle(scope, obj) {
                if !ptr.is_null() {
                    return Some(NativeValue { pointer: ptr });
                }
            }
        }
    }

    macro_rules! box_insp {
        ($expr:expr) => {{
            let v = $expr.ok()?;
            let ptr = v.as_raw() as *mut std::ffi::c_void;
            std::mem::forget(v);
            Some(NativeValue { pointer: ptr })
        }};
    }
    macro_rules! box_num {
        ($create:expr) => {{
            let n = arg.number_value(scope)?;
            box_insp!($create(n))
        }};
    }
    // Accept both short names ("TimeSpan") and the fully-qualified forms that
    // callers like NativeScript core pass ("Windows.Foundation.TimeSpan").
    let type_name = type_name.trim();
    let type_name = type_name
        .strip_prefix("Windows.Foundation.")
        .unwrap_or(type_name);
    match type_name {
        "Double" => box_num!(|n: f64| PropertyValue::CreateDouble(n)),
        "Single" => box_num!(|n: f64| PropertyValue::CreateSingle(n as f32)),
        "Int32" | "IntI32" => box_num!(|n: f64| PropertyValue::CreateInt32(n as i32)),
        "UInt32" => box_num!(|n: f64| PropertyValue::CreateUInt32(n as u32)),
        // Int64/UInt64 may exceed 2^53 — read via BigInt-aware helper, not number_value.
        "Int64" => box_insp!(PropertyValue::CreateInt64(v8_to_i64(scope, arg))),
        "UInt64" => box_insp!(PropertyValue::CreateUInt64(v8_to_i64(scope, arg) as u64)),
        "Int16" => box_num!(|n: f64| PropertyValue::CreateInt16(n as i16)),
        "UInt16" => box_num!(|n: f64| PropertyValue::CreateUInt16(n as u16)),
        "UInt8" | "Uint8" | "Byte" => box_num!(|n: f64| PropertyValue::CreateUInt8(n as u8)),
        // Char16: accept a JS string (takes first UTF-16 code unit) or a number.
        "Char16" | "Char" => {
            let ch: u16 = if arg.is_string() {
                arg.to_rust_string_lossy(scope)
                    .encode_utf16()
                    .next()
                    .unwrap_or(0)
            } else {
                arg.number_value(scope)? as u16
            };
            box_insp!(PropertyValue::CreateChar16(ch))
        }
        "Boolean" => {
            let b = arg.boolean_value(scope);
            box_insp!(PropertyValue::CreateBoolean(b))
        }
        "String" => {
            let hs = hstring_from_v8_value(scope, arg);
            box_insp!(PropertyValue::CreateString(&hs))
        }
        // TimeSpan: accept a number of milliseconds; struct { Duration } (100ns ticks) is also
        // handled — pass the raw ticks integer directly.
        "TimeSpan" => {
            let ticks = if arg.is_object() {
                // struct { Duration: <100ns ticks> } — Number or BigInt
                read_i64_field(scope, arg, "Duration")
            } else {
                // treat as milliseconds → convert to 100ns ticks
                (arg.number_value(scope)? * 10_000.0) as i64
            };
            let ts = windows::Foundation::TimeSpan { Duration: ticks };
            box_insp!(PropertyValue::CreateTimeSpan(ts))
        }
        // DateTime: accept JS milliseconds since Unix epoch; struct { UniversalTime } also works.
        "DateTime" => {
            let universal_time = if arg.is_object() {
                // struct { UniversalTime: <100ns ticks since 1601> } — Number or BigInt
                read_i64_field(scope, arg, "UniversalTime")
            } else {
                // JS ms since 1970-01-01 → WinRT 100ns ticks since 1601-01-01
                const EPOCH_DIFF_TICKS: i64 = 11_644_473_600_000 * 10_000;
                let ms = arg.number_value(scope)? as i64;
                ms * 10_000 + EPOCH_DIFF_TICKS
            };
            let dt = windows::Foundation::DateTime {
                UniversalTime: universal_time,
            };
            box_insp!(PropertyValue::CreateDateTime(dt))
        }
        // Guid: accept "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" string.
        "Guid" => {
            let s = arg.to_rust_string_lossy(scope);
            let guid = parse_guid_str(s.trim())?;
            box_insp!(PropertyValue::CreateGuid(guid))
        }
        _ => None,
    }
}

/// Keep the old name as an alias — used by method_call / property_call for IReference<T> params.
#[cfg(feature = "classic")]
#[inline]
pub fn box_as_ireference(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
    inner_type: &str,
) -> Option<NativeValue> {
    box_as_typed_value(scope, arg, inner_type)
}

/// Parse "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" into a GUID.
pub(crate) fn parse_guid_str(s: &str) -> Option<windows::core::GUID> {
    let s = s.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let data1 = u32::from_str_radix(parts[0], 16).ok()?;
    let data2 = u16::from_str_radix(parts[1], 16).ok()?;
    let data3 = u16::from_str_radix(parts[2], 16).ok()?;
    let b34 = u16::from_str_radix(parts[3], 16).ok()?;
    let b5 = u64::from_str_radix(parts[4], 16).ok()?;
    let data4 = [
        (b34 >> 8) as u8,
        b34 as u8,
        (b5 >> 40) as u8,
        (b5 >> 32) as u8,
        (b5 >> 24) as u8,
        (b5 >> 16) as u8,
        (b5 >> 8) as u8,
        b5 as u8,
    ];
    Some(windows::core::GUID {
        data1,
        data2,
        data3,
        data4,
    })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_pointer_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    if arg.is_object() {
        let arg = arg.to_object(scope).unwrap();
        if let Some(pointer) = try_get_external_handle(scope, arg) {
            return Ok(NativeValue { pointer });
        }
    }

    // Box primitive JS values as WinRT IPropertyValue (IInspectable) so they can be
    // passed to Object-typed parameters like Header, Content, or IVector<Object>.Append.
    if arg.is_string() {
        let hstring = hstring_from_v8_value(scope, arg);
        if let Ok(inspectable) = PropertyValue::CreateString(&hstring) {
            let ptr = inspectable.as_raw() as *mut c_void;
            // WinRT callees AddRef when storing; our reference is intentionally leaked
            // here so the raw pointer stays valid through the FFI call.
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    if arg.is_number() {
        let n = arg.number_value(scope).unwrap_or(0.0);
        // For untyped Object parameters, use Int32 for whole numbers and Double otherwise.
        // Callers that need a specific IReference<T> type call box_as_ireference() first.
        if n == n.trunc() && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
            if let Ok(inspectable) = PropertyValue::CreateInt32(n as i32) {
                let ptr = inspectable.as_raw() as *mut c_void;
                std::mem::forget(inspectable);
                return Ok(NativeValue { pointer: ptr });
            }
        } else if let Ok(inspectable) = PropertyValue::CreateDouble(n) {
            let ptr = inspectable.as_raw() as *mut c_void;
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    if arg.is_boolean() {
        let b = arg.boolean_value(scope);
        if let Ok(inspectable) = PropertyValue::CreateBoolean(b) {
            let ptr = inspectable.as_raw() as *mut c_void;
            std::mem::forget(inspectable);
            return Ok(NativeValue { pointer: ptr });
        }
    }

    let pointer = if let Ok(value) = v8::Local::<v8::External>::try_from(arg) {
        value.value()
    } else if arg.is_null_or_undefined() {
        std::ptr::null_mut()
    } else {
        return Err(type_error(
            "Invalid FFI pointer type, expected null, External, or { handle: External|null }",
        ));
    };
    Ok(NativeValue { pointer })
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_query_interface_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
    iid: &GUID,
) -> std::result::Result<(NativeValue, Option<IUnknown>), AnyError> {
    if arg.is_null_or_undefined() {
        return Ok((
            NativeValue {
                pointer: std::ptr::null_mut(),
            },
            None,
        ));
    }

    if arg.is_object() {
        let arg = arg.to_object(scope).unwrap();
        if let Some(pointer) = try_get_external_handle(scope, arg) {
            if pointer.is_null() {
                return Ok((
                    NativeValue {
                        pointer: std::ptr::null_mut(),
                    },
                    None,
                ));
            }

            let unknown = ManuallyDrop::new(unsafe { IUnknown::from_raw(pointer) });
            let vtable = unknown.vtable();
            let mut queried: *mut c_void = std::ptr::null_mut();

            let result = unsafe {
                ((*vtable).QueryInterface)(
                    unknown.as_raw(),
                    iid,
                    &mut queried as *mut _ as *mut *mut c_void,
                )
            };

            if result.is_ok() && !queried.is_null() {
                let queried = unsafe { IUnknown::from_raw(queried) };
                let pointer = queried.as_raw() as *mut c_void;
                return Ok((NativeValue { pointer }, Some(queried)));
            }

            return Err(type_error(
                "Invalid FFI interface argument for expected WinRT type",
            ));
        }
    }

    Ok((ffi_parse_pointer_arg(scope, arg)?, None))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_buffer_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    let (value, _) = ffi_parse_buffer_arg_with_length(scope, arg)?;
    Ok(value)
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_buffer_arg_with_length(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<(NativeValue, u32), AnyError> {
    // Order of checking:
    // 1. ArrayBuffer: Fairly common and not supported by Fast API, optimise this case.
    // 2. ArrayBufferView: Common and supported by Fast API
    // 5. Null: Very uncommon / can be represented by a 0.

    let (pointer, byte_length) = if let Ok(value) = v8::Local::<v8::ArrayBuffer>::try_from(arg) {
        let len = value.byte_length() as u32;
        if let Some(non_null) = value.data() {
            (non_null.as_ptr(), len)
        } else {
            (std::ptr::null_mut(), len)
        }
    } else if let Ok(value) = v8::Local::<v8::ArrayBufferView>::try_from(arg) {
        let byte_offset = value.byte_offset();
        let len = value.byte_length() as u32;
        let pointer = value
            .buffer(scope)
            .ok_or_else(|| type_error("Invalid FFI ArrayBufferView, expected data in the buffer"))?
            .data();
        if let Some(non_null) = pointer {
            // SAFETY: Pointer is non-null, and V8 guarantees that the byte_offset
            // is within the buffer backing store.
            (unsafe { non_null.as_ptr().add(byte_offset) }, len)
        } else {
            (std::ptr::null_mut(), len)
        }
    } else if arg.is_null() {
        (std::ptr::null_mut(), 0)
    } else {
        return Err(type_error(
            "Invalid FFI buffer type, expected null, ArrayBuffer, or ArrayBufferView",
        ));
    };
    Ok((NativeValue { pointer }, byte_length))
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_struct_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    // Order of checking:
    // 1. ArrayBuffer: Fairly common and not supported by Fast API, optimise this case.
    // 2. ArrayBufferView: Common and supported by Fast API

    let pointer = if let Ok(value) = v8::Local::<v8::ArrayBuffer>::try_from(arg) {
        if let Some(non_null) = value.data() {
            non_null.as_ptr()
        } else {
            return Err(type_error(
                "Invalid FFI ArrayBuffer, expected data in buffer",
            ));
        }
    } else if let Ok(value) = v8::Local::<v8::ArrayBufferView>::try_from(arg) {
        let byte_offset = value.byte_offset();
        let pointer = value
            .buffer(scope)
            .ok_or_else(|| type_error("Invalid FFI ArrayBufferView, expected data in the buffer"))?
            .data();
        if let Some(non_null) = pointer {
            // SAFETY: Pointer is non-null, and V8 guarantees that the byte_offset
            // is within the buffer backing store.
            unsafe { non_null.as_ptr().add(byte_offset) }
        } else {
            return Err(type_error(
                "Invalid FFI ArrayBufferView, expected data in buffer",
            ));
        }
    } else {
        return Err(type_error(
            "Invalid FFI struct type, expected ArrayBuffer, or ArrayBufferView",
        ));
    };
    Ok(NativeValue { pointer })
}

/// Write a single struct field value (from a V8 value) into a byte buffer in little-endian order.
/// Used when converting a plain JS object like `{A:255, R:0, G:0, B:0}` to WinRT struct bytes.
#[cfg(feature = "classic")]
pub(crate) fn append_struct_field_bytes(
    buf: &mut Vec<u8>,
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    native_type: &NativeType,
) {
    let num = value.number_value(scope).unwrap_or(0.0);
    match native_type {
        NativeType::F64 => buf.extend_from_slice(&num.to_le_bytes()),
        NativeType::F32 => buf.extend_from_slice(&(num as f32).to_le_bytes()),
        NativeType::I32 => buf.extend_from_slice(&(num as i32).to_le_bytes()),
        NativeType::U32 => buf.extend_from_slice(&(num as u32).to_le_bytes()),
        // i64/u64 fields can exceed 2^53 — read via BigInt-aware helper, not f64.
        NativeType::I64 => buf.extend_from_slice(&v8_to_i64(scope, value).to_le_bytes()),
        NativeType::U64 => buf.extend_from_slice(&(v8_to_i64(scope, value) as u64).to_le_bytes()),
        NativeType::I16 => buf.extend_from_slice(&(num as i16).to_le_bytes()),
        NativeType::U16 => buf.extend_from_slice(&(num as u16).to_le_bytes()),
        NativeType::I8 => buf.extend_from_slice(&(num as i8).to_le_bytes()),
        NativeType::U8 => buf.push(num as u8),
        NativeType::Bool => buf.push(if value.boolean_value(scope) { 1u8 } else { 0u8 }),
        _ => buf.extend(std::iter::repeat(0u8).take(native_type.size())),
    }
}

#[cfg(feature = "classic")]
#[inline]
pub fn ffi_parse_function_arg(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
) -> std::result::Result<NativeValue, AnyError> {
    if arg.is_object() {
        let arg = arg.to_object(scope).unwrap();
        if let Some(pointer) = try_get_external_handle(scope, arg) {
            return Ok(NativeValue { pointer });
        }
    }

    let pointer = if let Ok(value) = v8::Local::<v8::External>::try_from(arg) {
        value.value()
    } else if arg.is_null_or_undefined() {
        std::ptr::null_mut()
    } else {
        return Err(type_error(
            "Invalid FFI function type, expected null, External, or { handle: External|null }",
        ));
    };

    Ok(NativeValue { pointer })
}

#[cfg(feature = "classic")]
#[inline]
fn external_or_null<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    value: *mut c_void,
) -> v8::Local<'a, v8::Value> {
    if value.is_null() {
        v8::null(scope).into()
    } else {
        v8::External::new(scope, value).into()
    }
}

#[cfg(feature = "classic")]
#[inline]
pub unsafe fn set_ret_val(
    value: *mut c_void,
    scope: &mut v8::PinScope<'_, '_>,
    mut rv: v8::ReturnValue,
    native_type: NativeType,
) {
    match native_type {
        NativeType::Void => {
            rv.set_undefined();
        }
        NativeType::Bool => {
            let b = unsafe { std::ptr::read_unaligned(value as *const u8) } != 0u8;
            rv.set_bool(b);
        }
        NativeType::U8 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const u8) };
            rv.set_uint32(v as u32);
        }
        NativeType::I8 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const i8) };
            rv.set_int32(v as i32);
        }
        NativeType::U16 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const u16) };
            rv.set_uint32(v as u32);
        }
        NativeType::I16 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const i16) };
            rv.set_int32(v as i32);
        }
        NativeType::U32 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const u32) };
            rv.set_uint32(v);
        }
        NativeType::I32 => {
            let v = unsafe { std::ptr::read_unaligned(value as *const i32) };
            rv.set_int32(v);
        }
        NativeType::U64 => {
            let ret = unsafe { std::ptr::read_unaligned(value as *const u64) };
            let local_value: v8::Local<v8::Value> = if ret > MAX_SAFE_INTEGER as u64 {
                v8::BigInt::new_from_u64(scope, ret).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            };
            rv.set(local_value);
        }
        NativeType::I64 => {
            let ret = unsafe { std::ptr::read_unaligned(value as *const i64) };
            let local_value: v8::Local<v8::Value> =
                if ret > MAX_SAFE_INTEGER as i64 || ret < MIN_SAFE_INTEGER as i64 {
                    v8::BigInt::new_from_i64(scope, ret).into()
                } else {
                    v8::Number::new(scope, ret as f64).into()
                };
            rv.set(local_value);
        }
        NativeType::USize => {
            let ret = unsafe { std::ptr::read_unaligned(value as *const usize) };
            let local_value: v8::Local<v8::Value> = if ret > MAX_SAFE_INTEGER as usize {
                v8::BigInt::new_from_u64(scope, ret as u64).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            };
            rv.set(local_value);
        }
        NativeType::ISize => {
            let ret = unsafe { std::ptr::read_unaligned(value as *const isize) };
            let local_value: v8::Local<v8::Value> =
                if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&ret) {
                    v8::BigInt::new_from_i64(scope, ret as i64).into()
                } else {
                    v8::Number::new(scope, ret as f64).into()
                };
            rv.set(local_value);
        }
        NativeType::F32 => {
            let bits = unsafe { std::ptr::read_unaligned(value as *const u32) };
            let ret = f32::from_bits(bits);
            rv.set(v8::Number::new(scope, ret as f64).into());
        }
        NativeType::F64 => {
            let bits = unsafe { std::ptr::read_unaligned(value as *const u64) };
            let ret = f64::from_bits(bits);
            rv.set_double(ret);
        }
        NativeType::Pointer => {
            // A WinRT reference-type return is a COM pointer; resolve its concrete type so the JS side
            // gets a full typed proxy (with property/event interceptors) rather than a bare, non-extensible
            // External. Falls back to the plain wrapper when the type can't be resolved.
            match crate::ns_proxy::try_wrap_inspectable_pointer(value, scope) {
                Some(v) => rv.set(v),
                None => rv.set(external_or_null(scope, value)),
            }
        }
        NativeType::Buffer => {
            rv.set(external_or_null(scope, value));
        }
        NativeType::Function => {
            rv.set(external_or_null(scope, value));
        }
        NativeType::Struct(_) => {
            rv.set(external_or_null(scope, value));
        }
        NativeType::String => {
            if value.is_null() {
                rv.set_undefined();
            } else {
                // `value` points to the return_value_buf scratch area where
                // WinRT wrote the HSTRING handle.  Read it out, take
                // ownership via transmute (we are the callee-allocated owner),
                // convert to a Rust string, then drop to release the WinRT
                // string buffer.
                let raw_usize = unsafe { std::ptr::read_unaligned(value as *const usize) };
                let hstring: HSTRING = unsafe { std::mem::transmute(raw_usize) };
                let s = hstring.to_string_lossy();
                drop(hstring);
                let v = v8::String::new(scope, &s).unwrap_or_else(|| v8::String::empty(scope));
                rv.set(v.into());
            }
        }
    }
}

/// Read a native value from a raw pointer and convert it to a V8 value.
/// `ptr` must point to storage containing the native representation (e.g. u32, HSTRING, pointer, struct bytes).
#[cfg(feature = "classic")]
pub unsafe fn read_value_from_ptr<'a>(
    ptr: *const c_void,
    scope: &mut v8::PinScope<'a, '_>,
    native_type: NativeType,
) -> v8::Local<'a, v8::Value> {
    match native_type {
        NativeType::Void => v8::undefined(scope).into(),
        NativeType::Bool => {
            let b = std::ptr::read_unaligned(ptr as *const u8) != 0u8;
            v8::Boolean::new(scope, b).into()
        }
        NativeType::U8 => {
            let v = std::ptr::read_unaligned(ptr as *const u8);
            v8::Integer::new_from_unsigned(scope, v as u32).into()
        }
        NativeType::I8 => {
            let v = std::ptr::read_unaligned(ptr as *const i8);
            v8::Integer::new(scope, v as i32).into()
        }
        NativeType::U16 => {
            let v = std::ptr::read_unaligned(ptr as *const u16);
            v8::Integer::new_from_unsigned(scope, v as u32).into()
        }
        NativeType::I16 => {
            let v = std::ptr::read_unaligned(ptr as *const i16);
            v8::Integer::new(scope, v as i32).into()
        }
        NativeType::U32 => {
            let v = std::ptr::read_unaligned(ptr as *const u32);
            v8::Integer::new_from_unsigned(scope, v).into()
        }
        NativeType::I32 => {
            let v = std::ptr::read_unaligned(ptr as *const i32);
            v8::Integer::new(scope, v).into()
        }
        NativeType::U64 => {
            let ret = std::ptr::read_unaligned(ptr as *const u64);
            if ret > MAX_SAFE_INTEGER as u64 {
                v8::BigInt::new_from_u64(scope, ret).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            }
        }
        NativeType::I64 => {
            let ret = std::ptr::read_unaligned(ptr as *const i64);
            if ret > MAX_SAFE_INTEGER as i64 || ret < MIN_SAFE_INTEGER as i64 {
                v8::BigInt::new_from_i64(scope, ret).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            }
        }
        NativeType::USize => {
            let ret = std::ptr::read_unaligned(ptr as *const usize);
            if ret > MAX_SAFE_INTEGER as usize {
                v8::BigInt::new_from_u64(scope, ret as u64).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            }
        }
        NativeType::ISize => {
            let ret = std::ptr::read_unaligned(ptr as *const isize);
            if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&ret) {
                v8::BigInt::new_from_i64(scope, ret as i64).into()
            } else {
                v8::Number::new(scope, ret as f64).into()
            }
        }
        NativeType::F32 => {
            let bits = std::ptr::read_unaligned(ptr as *const u32);
            let ret = f32::from_bits(bits);
            v8::Number::new(scope, ret as f64).into()
        }
        NativeType::F64 => {
            let bits = std::ptr::read_unaligned(ptr as *const u64);
            let ret = f64::from_bits(bits);
            v8::Number::new(scope, ret).into()
        }
        NativeType::Pointer => {
            let p = std::ptr::read_unaligned(ptr as *const *mut c_void);
            if p.is_null() {
                v8::null(scope).into()
            } else {
                v8::External::new(scope, p).into()
            }
        }
        NativeType::Buffer => {
            let p = std::ptr::read_unaligned(ptr as *const *mut c_void);
            if p.is_null() {
                v8::null(scope).into()
            } else {
                v8::External::new(scope, p).into()
            }
        }
        NativeType::Function => {
            let p = std::ptr::read_unaligned(ptr as *const *mut c_void);
            if p.is_null() {
                v8::null(scope).into()
            } else {
                v8::External::new(scope, p).into()
            }
        }
        NativeType::Struct(_) => {
            // Expose as External pointing to the struct bytes
            if ptr.is_null() {
                v8::null(scope).into()
            } else {
                v8::External::new(scope, ptr as *mut c_void).into()
            }
        }
        NativeType::String => {
            if ptr.is_null() {
                v8::undefined(scope).into()
            } else {
                let raw_usize = std::ptr::read_unaligned(ptr as *const usize);
                let hstring: HSTRING = std::mem::transmute(raw_usize);
                let s = hstring.to_string_lossy();
                drop(hstring);
                v8::String::new(scope, &s)
                    .unwrap_or_else(|| v8::String::empty(scope))
                    .into()
            }
        }
    }
}

/// Initialize caller-allocated out-slot storage from a V8 value.
/// Writes the native representation for `native_type` into `dst`.
/// Returns Ok(Some(parse_type)) when the slot should be treated as having that parse type
/// for later string-cloning logic (e.g. NativeType::String), Ok(None) when no parse-type
/// should be set, or Err(...) on parse errors.
#[cfg(feature = "classic")]
pub fn write_v8_value_to_ptr(
    scope: &mut v8::PinScope<'_, '_>,
    arg: v8::Local<v8::Value>,
    dst: *mut c_void,
    native_type: &NativeType,
) -> std::result::Result<Option<NativeType>, AnyError> {
    use std::ptr::{copy_nonoverlapping, write_unaligned};
    match native_type {
        NativeType::Bool => {
            let nv = ffi_parse_bool_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut u8, nv.bool_value as u8);
            }
            Ok(Some(NativeType::Bool))
        }
        NativeType::U8 => {
            let nv = ffi_parse_u8_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut u8, nv.u8_value);
            }
            Ok(Some(NativeType::U8))
        }
        NativeType::I8 => {
            let nv = ffi_parse_i8_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut i8, nv.i8_value);
            }
            Ok(Some(NativeType::I8))
        }
        NativeType::U16 => {
            let nv = ffi_parse_u16_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut u16, nv.u16_value);
            }
            Ok(Some(NativeType::U16))
        }
        NativeType::I16 => {
            let nv = ffi_parse_i16_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut i16, nv.i16_value);
            }
            Ok(Some(NativeType::I16))
        }
        NativeType::U32 => {
            let nv = ffi_parse_u32_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut u32, nv.u32_value);
            }
            Ok(Some(NativeType::U32))
        }
        NativeType::I32 => {
            let nv = ffi_parse_i32_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut i32, nv.i32_value);
            }
            Ok(Some(NativeType::I32))
        }
        NativeType::U64 => {
            let nv = ffi_parse_u64_arg(scope, arg)?;
            unsafe {
                write_unaligned(dst as *mut u64, nv.u64_value);
            }
            Ok(Some(NativeType::U64))
        }
        NativeType::I64 => {
            let nv = ffi_parse_i64_arg(scope, arg)?;
            unsafe {
                write_unaligned(dst as *mut i64, nv.i64_value);
            }
            Ok(Some(NativeType::I64))
        }
        NativeType::USize => {
            let nv = ffi_parse_usize_arg(scope, arg)?;
            unsafe {
                write_unaligned(dst as *mut usize, nv.usize_value);
            }
            Ok(Some(NativeType::USize))
        }
        NativeType::ISize => {
            let nv = ffi_parse_isize_arg(scope, arg)?;
            unsafe {
                write_unaligned(dst as *mut isize, nv.isize_value);
            }
            Ok(Some(NativeType::ISize))
        }
        NativeType::F32 => {
            let nv = ffi_parse_f32_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut f32, nv.f32_value);
            }
            Ok(Some(NativeType::F32))
        }
        NativeType::F64 => {
            let nv = ffi_parse_f64_arg(arg)?;
            unsafe {
                write_unaligned(dst as *mut f64, nv.f64_value);
            }
            Ok(Some(NativeType::F64))
        }
        NativeType::Pointer | NativeType::Buffer | NativeType::Function => {
            let nv = ffi_parse_pointer_arg(scope, arg)?;
            unsafe {
                write_unaligned(dst as *mut *mut c_void, nv.pointer);
            }
            Ok(Some(NativeType::Pointer))
        }
        NativeType::Struct(_) => {
            // Copy struct bytes from an ArrayBuffer/ArrayBufferView or other struct source
            let nv = ffi_parse_struct_arg(scope, arg)?;
            let src = unsafe { nv.pointer as *const u8 };
            let size = native_type.size();
            unsafe {
                copy_nonoverlapping(src, dst as *mut u8, size);
            }
            Ok(Some(native_type.clone()))
        }
        NativeType::String => {
            // If the JS object carries an __hstring_ptr External, clone it.
            if arg.is_object() {
                if let Some(obj) = arg.to_object(scope) {
                    if let Some(key) = v8::String::new(scope, "__hstring_ptr") {
                        if let Some(val) = obj.get(scope, key.into()) {
                            if let Ok(ext) = v8::Local::<v8::External>::try_from(val) {
                                let raw = ext.value() as *const HSTRING;
                                if !raw.is_null() {
                                    let hclone = unsafe { (*raw).clone() };
                                    let raw_usize: usize = unsafe { std::mem::transmute(hclone) };
                                    unsafe {
                                        write_unaligned(dst as *mut usize, raw_usize);
                                    }
                                    return Ok(Some(NativeType::String));
                                }
                            }
                        }
                    }
                }
            }

            // Fallback: convert JS string -> HSTRING and store raw handle (transfer ownership)
            let s = v8::Local::<v8::String>::try_from(arg)
                .map_err(|_| type_error("Invalid FFI String type, expected String"))?;
            let h: HSTRING = hstring_from_v8_string(scope, s);
            let raw_usize: usize = unsafe { std::mem::transmute(h) };
            unsafe {
                write_unaligned(dst as *mut usize, raw_usize);
            }
            Ok(Some(NativeType::String))
        }
        NativeType::Void => Ok(None),
    }
}

pub(crate) const OUT_PARAM_MARKER: &str = "__nswinrt_out_param__";

/// Detects an `NSWinRT.interop.out(...)` wrapper and returns its object plus
/// the wrapped `value` that should initialize the native byref slot.
#[cfg(feature = "classic")]
pub fn try_unwrap_out_param<'scope, 'value>(
    scope: &mut v8::PinScope<'scope, '_>,
    arg: v8::Local<'value, v8::Value>,
) -> Option<(v8::Local<'scope, v8::Object>, v8::Local<'scope, v8::Value>)> {
    if !arg.is_object() {
        return None;
    }

    let object = arg.to_object(scope)?;
    let marker_key = v8::String::new(scope, OUT_PARAM_MARKER)?;
    let marker = object.get(scope, marker_key.into())?;
    if !marker.boolean_value(scope) {
        return None;
    }

    let value_key = v8::String::new(scope, "value")?;
    let value = object
        .get(scope, value_key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    Some((object, value))
}

#[cfg(feature = "classic")]
pub fn set_out_param_value<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    wrapper: v8::Local<'a, v8::Object>,
    value: v8::Local<'a, v8::Value>,
) -> bool {
    let Some(value_key) = v8::String::new(scope, "value") else {
        return false;
    };
    wrapper.set(scope, value_key.into(), value).unwrap_or(false)
}
