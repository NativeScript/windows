//! Typed `Invoke` for WinRT delegates implemented in JS.
//!
//! The JS delegate COM objects (the classic engine's `JsDelegate`, the napi engine's
//! `NapiDelegate`) share one vtable whose Invoke takes up to three pointer-sized arguments and
//! returns nothing. That covers event handlers (`(sender, args)`) but not a delegate such as
//! `Int64 F(Single, Single, Single, Single)`: on x64 and ARM64 floating-point arguments
//! travel in vector registers, which an integer-typed Invoke never reads, and a return value comes
//! back through a trailing out-pointer, which it never writes.
//!
//! For those delegates a libffi closure with the Invoke's exact signature is installed as the
//! delegate's Invoke instead: libffi collects each argument from wherever the ABI put it, and the
//! engine writes the JS function's result into the out-pointer.

use std::ffi::c_void;

use libffi::low::{self, ffi_closure, CodePtr};
pub(crate) use libffi::low::ffi_cif;
use libffi::middle::{Cif, Type};

use crate::value::NativeType;

/// What a delegate's Invoke returns.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DelegateReturn {
    Void,
    /// A number or boolean, written from the JS function's result.
    Scalar(NativeType),
    /// An object, string or struct: not marshalled; the result slot is set to zero.
    Unsupported,
}

/// A delegate Invoke's in-parameters and return, from WinRT metadata.
#[derive(Clone, Debug)]
pub(crate) struct DelegateSignature {
    pub(crate) params: Vec<NativeType>,
    pub(crate) ret: DelegateReturn,
}

impl DelegateSignature {
    pub(crate) fn new(params: Vec<NativeType>, ret: DelegateReturn) -> Self {
        Self { params, ret }
    }

    /// Whether Invoke has to go through [`TypedInvoke`]: a floating-point parameter, more
    /// parameters than the shared vtable's Invoke reads, or a value to return. Parameters that
    /// span more than one ABI slot (arrays) or are passed by value (structs) keep the shared
    /// Invoke, as before.
    pub(crate) fn needs_typed_invoke(&self) -> bool {
        let single_slot = self.params.iter().all(|p| !matches!(p, NativeType::Buffer | NativeType::Struct(_)));
        single_slot
            && (self.params.len() > 3
                || self.params.iter().any(|p| matches!(p, NativeType::F32 | NativeType::F64))
                || matches!(self.ret, DelegateReturn::Scalar(_)))
    }

    fn has_result_slot(&self) -> bool {
        self.ret != DelegateReturn::Void
    }
}

/// Classifies an Invoke return signature. `resolve` maps a signature to its NativeType the way
/// delegate parameters are (named enums become `U32`).
pub(crate) fn delegate_return_for_signature(sig: &str, resolve: impl Fn(&str) -> NativeType) -> DelegateReturn {
    if sig.is_empty() || sig == "Void" {
        return DelegateReturn::Void;
    }
    match resolve(sig) {
        NativeType::Void => DelegateReturn::Void,
        NativeType::Pointer | NativeType::Buffer | NativeType::Function | NativeType::String | NativeType::Struct(_) => {
            DelegateReturn::Unsupported
        }
        scalar => DelegateReturn::Scalar(scalar),
    }
}

/// One Invoke argument as read from its ABI slot. Integers and pointers keep the raw word the
/// shared Invoke has always handed to the per-type conversions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DelegateArg {
    Word(usize),
    F32(f32),
    F64(f64),
}

/// The JS function's result, converted for the result slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ReturnValue {
    Int(i64),
    UInt(u64),
    Float(f64),
    Bool(bool),
}

/// The callback libffi runs for a typed Invoke. `result` is libffi's integer-wide slot for the
/// HRESULT; `args[0]` points at the delegate (`this`).
pub(crate) type TypedInvokeCallback = unsafe extern "C" fn(cif: &ffi_cif, result: &mut u64, args: *const *const c_void, userdata: &());

/// A libffi closure implementing one delegate's Invoke with its real signature. Lives as long as
/// the delegate: its code pointer sits in the delegate's vtable.
pub(crate) struct TypedInvoke {
    // The closure keeps a pointer to the CIF, so it must stay alive (and in place) with it.
    _cif: Box<Cif>,
    closure: *mut ffi_closure,
    code: CodePtr,
}

impl TypedInvoke {
    /// Builds `HRESULT Invoke(this, params..., [result*])` around `callback`.
    pub(crate) fn new(signature: &DelegateSignature, callback: TypedInvokeCallback) -> Option<Self> {
        let mut args = Vec::with_capacity(signature.params.len() + 2);
        args.push(Type::pointer());
        for param in &signature.params {
            args.push(Type::try_from(param.clone()).ok()?);
        }
        if signature.has_result_slot() {
            args.push(Type::pointer());
        }
        let cif = Box::new(Cif::new(args, Type::i32()));
        let (closure, code) = low::closure_alloc();
        if closure.is_null() {
            return None;
        }
        let prepared = unsafe { low::prep_closure(closure, cif.as_raw_ptr(), callback, std::ptr::null::<()>(), code) };
        if prepared.is_err() {
            unsafe { low::closure_free(closure) };
            return None;
        }
        Some(Self { _cif: cif, closure, code })
    }

    /// The Invoke entry point to put in the delegate's vtable.
    pub(crate) fn code_ptr(&self) -> *const c_void {
        self.code.as_ptr()
    }
}

impl Drop for TypedInvoke {
    fn drop(&mut self) {
        unsafe { low::closure_free(self.closure) };
    }
}

/// Reads a typed Invoke's `this`, arguments and result slot from libffi's argument array.
///
/// # Safety
/// `args` must be the argument array libffi passed to a [`TypedInvoke`] callback built from
/// `signature`.
pub(crate) unsafe fn read_invoke_args(args: *const *const c_void, signature: &DelegateSignature) -> (*mut c_void, Vec<DelegateArg>, *mut c_void) {
    let this = *(*args as *const *mut c_void);
    let mut values = Vec::with_capacity(signature.params.len());
    for (i, ty) in signature.params.iter().enumerate() {
        let slot = *args.add(i + 1);
        values.push(match ty {
            NativeType::F32 => DelegateArg::F32(*(slot as *const f32)),
            NativeType::F64 => DelegateArg::F64(*(slot as *const f64)),
            NativeType::Bool | NativeType::U8 => DelegateArg::Word(*(slot as *const u8) as usize),
            NativeType::I8 => DelegateArg::Word(*(slot as *const i8) as isize as usize),
            NativeType::U16 => DelegateArg::Word(*(slot as *const u16) as usize),
            NativeType::I16 => DelegateArg::Word(*(slot as *const i16) as isize as usize),
            NativeType::U32 => DelegateArg::Word(*(slot as *const u32) as usize),
            NativeType::I32 => DelegateArg::Word(*(slot as *const i32) as isize as usize),
            // 64-bit integers, pointers and handles fill the word.
            _ => DelegateArg::Word(*(slot as *const usize)),
        });
    }
    let result = if signature.has_result_slot() {
        *(*args.add(signature.params.len() + 1) as *const *mut c_void)
    } else {
        std::ptr::null_mut()
    };
    (this, values, result)
}

/// Writes the JS result into an Invoke's result slot; `None` (no result, or the function threw)
/// writes zero.
///
/// # Safety
/// `slot` must be null or the result pointer of an Invoke whose return is `ret`.
pub(crate) unsafe fn write_return(slot: *mut c_void, ret: &DelegateReturn, value: Option<ReturnValue>) {
    if slot.is_null() {
        return;
    }
    let ty = match ret {
        DelegateReturn::Void => return,
        DelegateReturn::Unsupported => {
            *(slot as *mut usize) = 0;
            return;
        }
        DelegateReturn::Scalar(ty) => ty,
    };
    let (int, float, boolean) = match value {
        Some(ReturnValue::Int(v)) => (v, v as f64, v != 0),
        Some(ReturnValue::UInt(v)) => (v as i64, v as f64, v != 0),
        Some(ReturnValue::Float(v)) => (float_to_i64(v), v, v != 0.0 && !v.is_nan()),
        Some(ReturnValue::Bool(v)) => (v as i64, v as u8 as f64, v),
        None => (0, 0.0, false),
    };
    match ty {
        NativeType::F32 => *(slot as *mut f32) = float as f32,
        NativeType::F64 => *(slot as *mut f64) = float,
        NativeType::Bool => *(slot as *mut u8) = boolean as u8,
        NativeType::U8 | NativeType::I8 => *(slot as *mut u8) = int as u8,
        NativeType::U16 | NativeType::I16 => *(slot as *mut u16) = int as u16,
        NativeType::U32 | NativeType::I32 => *(slot as *mut u32) = int as u32,
        NativeType::U64 => *(slot as *mut u64) = match value {
            Some(ReturnValue::UInt(v)) => v,
            _ => int as u64,
        },
        _ => *(slot as *mut i64) = int,
    }
}

// JS numbers convert to integers the way `ToInt64`-style casts do: truncate toward zero, NaN is 0.
fn float_to_i64(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else {
        v.trunc() as i64
    }
}

/// Stores `hr` in libffi's result slot for an `i32` return (integer returns narrower than a word
/// are written word-wide, sign-extended).
pub(crate) fn set_hresult(result: &mut u64, hr: i32) {
    *result = hr as i64 as u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure_signature() -> DelegateSignature {
        DelegateSignature::new(vec![NativeType::F32; 4], DelegateReturn::Scalar(NativeType::I64))
    }

    #[test]
    fn typed_invoke_is_only_for_signatures_the_shared_invoke_cannot_serve() {
        assert!(!DelegateSignature::new(vec![NativeType::Pointer, NativeType::Pointer], DelegateReturn::Void).needs_typed_invoke());
        assert!(!DelegateSignature::new(vec![NativeType::Pointer], DelegateReturn::Unsupported).needs_typed_invoke());
        assert!(measure_signature().needs_typed_invoke());
        assert!(DelegateSignature::new(vec![NativeType::F64], DelegateReturn::Void).needs_typed_invoke());
        assert!(DelegateSignature::new(vec![NativeType::Pointer; 4], DelegateReturn::Void).needs_typed_invoke());
        assert!(DelegateSignature::new(vec![], DelegateReturn::Scalar(NativeType::Bool)).needs_typed_invoke());
        // Arrays and by-value structs keep the shared Invoke.
        assert!(!DelegateSignature::new(vec![NativeType::Buffer, NativeType::F32], DelegateReturn::Void).needs_typed_invoke());
    }

    #[test]
    fn return_signatures_classify() {
        let resolve = crate::helpers::ffi_native_type_from_signature;
        assert_eq!(delegate_return_for_signature("Void", resolve), DelegateReturn::Void);
        assert_eq!(delegate_return_for_signature("Int64", resolve), DelegateReturn::Scalar(NativeType::I64));
        assert_eq!(delegate_return_for_signature("Single", resolve), DelegateReturn::Scalar(NativeType::F32));
        assert_eq!(delegate_return_for_signature("Boolean", resolve), DelegateReturn::Scalar(NativeType::Bool));
        assert_eq!(delegate_return_for_signature("Object", resolve), DelegateReturn::Unsupported);
        assert_eq!(delegate_return_for_signature("String", resolve), DelegateReturn::Unsupported);
    }

    struct Seen {
        this: usize,
        args: Vec<DelegateArg>,
    }

    thread_local! {
        static SEEN: std::cell::RefCell<Option<Seen>> = const { std::cell::RefCell::new(None) };
    }

    // Stands in for an engine: records what it read and returns the packed width/height.
    unsafe extern "C" fn record(_cif: &ffi_cif, result: &mut u64, args: *const *const c_void, _userdata: &()) {
        let signature = measure_signature();
        let (this, values, slot) = read_invoke_args(args, &signature);
        let (w, h) = match (values[0], values[1]) {
            (DelegateArg::F32(w), DelegateArg::F32(h)) => (w, h),
            _ => (0.0, 0.0),
        };
        let packed = ((w.to_bits() as u64) << 32) | h.to_bits() as u64;
        write_return(slot, &signature.ret, Some(ReturnValue::UInt(packed)));
        SEEN.with(|s| *s.borrow_mut() = Some(Seen { this: this as usize, args: values }));
        set_hresult(result, 0);
    }

    #[test]
    fn typed_invoke_reads_float_arguments_and_writes_the_result() {
        let invoke = TypedInvoke::new(&measure_signature(), record).expect("closure");
        // What a C++/WinRT caller of `Int64 F(Single, Single, Single, Single)` calls.
        type MeasureInvoke = unsafe extern "system" fn(*mut c_void, f32, f32, f32, f32, *mut i64) -> i32;
        let call: MeasureInvoke = unsafe { std::mem::transmute(invoke.code_ptr()) };
        let this = 0x1234usize as *mut c_void;
        let mut out: i64 = 0;
        let hr = unsafe { call(this, 120.5, f32::NAN, 300.0, -2.0, &mut out) };
        assert_eq!(hr, 0);
        let seen = SEEN.with(|s| s.borrow_mut().take()).expect("callback ran");
        assert_eq!(seen.this, 0x1234);
        assert!(matches!(seen.args[0], DelegateArg::F32(v) if v == 120.5));
        assert!(matches!(seen.args[1], DelegateArg::F32(v) if v.is_nan()));
        assert_eq!(seen.args[2], DelegateArg::F32(300.0));
        assert_eq!(seen.args[3], DelegateArg::F32(-2.0));
        assert_eq!((out as u64 >> 32) as u32, 120.5f32.to_bits());
        assert_eq!(out as u64 as u32, f32::NAN.to_bits());
    }

    #[test]
    fn results_convert_to_the_declared_type() {
        let mut f: f32 = 0.0;
        unsafe { write_return(&mut f as *mut f32 as *mut c_void, &DelegateReturn::Scalar(NativeType::F32), Some(ReturnValue::Float(1.5))) };
        assert_eq!(f, 1.5);
        let mut b: u8 = 7;
        unsafe { write_return(&mut b as *mut u8 as *mut c_void, &DelegateReturn::Scalar(NativeType::Bool), Some(ReturnValue::Bool(true))) };
        assert_eq!(b, 1);
        let mut i: i32 = 7;
        unsafe { write_return(&mut i as *mut i32 as *mut c_void, &DelegateReturn::Scalar(NativeType::I32), Some(ReturnValue::Float(-3.9))) };
        assert_eq!(i, -3);
        let mut p: usize = 99;
        unsafe { write_return(&mut p as *mut usize as *mut c_void, &DelegateReturn::Unsupported, None) };
        assert_eq!(p, 0);
        let mut z: i64 = 9;
        unsafe { write_return(&mut z as *mut i64 as *mut c_void, &DelegateReturn::Scalar(NativeType::I64), None) };
        assert_eq!(z, 0);
    }
}
