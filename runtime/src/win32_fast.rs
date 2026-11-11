//! V8 fast API overloads for bound Win32 functions (`__nsWin32BindFast`).
//!
//! Typed arrays, coercion and errors convert under a callback scope: handles created outside
//! one pile up in the caller's scope and slow every later GC. Mixed signatures take values,
//! not typed doubles, since Windows assigns registers by position.

use crate::win32_call::{prepare, Kind, Prepared, TextArena, Win32Result, MAX_ARGS};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use v8::fast_api::{
    CFunction, CFunctionInfo, CTypeInfo, FastApiCallbackOptions, Int64Representation, Type as CType,
};

/// How V8 hands an argument of this kind to the trampoline.
fn arg_ctype(kind: Kind, all_float: bool) -> CType {
    match kind {
        Kind::F32 | Kind::F64 if all_float => CType::Float64,
        Kind::I8 | Kind::I16 | Kind::I32 => CType::Int32,
        Kind::U8 | Kind::U16 | Kind::U32 => CType::Uint32,
        _ => CType::V8Value,
    }
}

/// 64-bit integers and pointers return as doubles, like the other paths.
fn ret_ctype(kind: Kind) -> CType {
    match kind {
        Kind::Void => CType::Void,
        Kind::Bool => CType::Bool,
        Kind::I8 | Kind::I16 | Kind::I32 => CType::Int32,
        Kind::U8 | Kind::U16 | Kind::U32 => CType::Uint32,
        _ => CType::Float64,
    }
}

struct Info(&'static CFunctionInfo);
// written once, then only read by V8
unsafe impl Send for Info {}
unsafe impl Sync for Info {}

/// One leaked `CFunctionInfo` per distinct signature.
fn info(args: &[CType], ret: CType) -> &'static CFunctionInfo {
    static INFOS: OnceLock<Mutex<HashMap<Vec<u8>, Info>>> = OnceLock::new();
    let mut key: Vec<u8> = args.iter().map(|t| *t as u8).collect();
    key.push(ret as u8);
    let mut infos = INFOS.get_or_init(Default::default).lock().unwrap();
    infos
        .entry(key)
        .or_insert_with(|| {
            // receiver, args, options
            let mut types = vec![CType::V8Value.as_info()];
            types.extend(args.iter().map(|t| t.as_info()));
            types.push(CType::CallbackOptions.as_info());
            let types: &'static [CTypeInfo] = Box::leak(types.into_boxed_slice());
            Info(Box::leak(Box::new(CFunctionInfo::new(
                ret.as_info(),
                types,
                Int64Representation::Number,
            ))))
        })
        .0
}

#[inline(always)]
unsafe fn prepared_of(data: v8::Local<v8::Value>) -> &'static Prepared {
    let ext = unsafe { v8::Local::<v8::External>::cast_unchecked(data) };
    unsafe { &*(ext.value() as *const Prepared) }
}

#[inline(always)]
unsafe fn local_from_raw(raw: u64) -> v8::Local<'static, v8::Value> {
    // A Local is one pointer into the frame's handle storage.
    unsafe { std::mem::transmute::<u64, v8::Local<'static, v8::Value>>(raw) }
}

/// Argument word of `v` as `kind` without allocating, running JS or creating handles.
/// `None`: needs the full conversion.
#[inline]
pub(crate) fn simple_arg(
    isolate: &v8::Isolate,
    kind: Kind,
    v: v8::Local<v8::Value>,
    arena: &mut TextArena,
) -> Option<u64> {
    if v.is_number() {
        let n = unsafe { v8::Local::<v8::Number>::cast_unchecked(v) }.value();
        return Some(kind.word_from_f64(n));
    }
    if v.is_null_or_undefined() {
        return Some(match kind {
            Kind::F64 if v.is_undefined() => f64::NAN.to_bits(),
            Kind::F32 if v.is_undefined() => f32::NAN.to_bits() as u64,
            _ => 0,
        });
    }
    if v.is_boolean() {
        return Some(kind.word_from_i64(v.is_true() as i64));
    }
    if v.is_string() {
        if !kind.is_text() {
            return None;
        }
        let s = unsafe { v8::Local::<v8::String>::cast_unchecked(v) };
        // A cons string is flattened first (handles in the enclosing scope); literals and
        // most strings are already flat.
        return Some(if kind == Kind::WStr {
            let (out, address) = arena.wide(s.length());
            s.write_v2(isolate, 0, out, v8::WriteFlags::empty());
            address
        } else {
            let (out, address) = arena.narrow(s.length() * 3);
            let n = s.write_utf8_v2(isolate, out, v8::WriteFlags::kReplaceInvalidUtf8, None);
            out[n.min(out.len())] = 0;
            address
        });
    }
    if v.is_big_int() {
        let b = unsafe { v8::Local::<v8::BigInt>::cast_unchecked(v) };
        return Some(if kind == Kind::U64 || kind == Kind::Pointer {
            kind.word_from_i64(b.u64_value().0 as i64)
        } else {
            kind.word_from_i64(b.i64_value().0)
        });
    }
    if !matches!(kind, Kind::Pointer | Kind::WStr | Kind::Str) {
        return None;
    }
    if v.is_array_buffer() {
        let buffer = unsafe { v8::Local::<v8::ArrayBuffer>::cast_unchecked(v) };
        return Some(buffer.data().map_or(0, |p| p.as_ptr() as u64));
    }
    if v.is_external() {
        let ext = unsafe { v8::Local::<v8::External>::cast_unchecked(v) };
        return Some(ext.value() as u64);
    }
    // Typed arrays and DataViews need the scoped path: reading their address goes through
    // `Buffer()`, which creates a handle in the current scope.
    None
}

/// The full conversion: coercion through `ToNumber` (which may run JS) and errors.
pub(crate) fn marshal_arg(
    scope: &mut v8::PinScope<'_, '_>,
    kind: Kind,
    v: v8::Local<v8::Value>,
    arena: &mut TextArena,
) -> Result<u64, String> {
    if let Some(w) = simple_arg(scope, kind, v, arena) {
        return Ok(w);
    }
    if kind.is_text() || kind == Kind::Pointer {
        if v.is_array_buffer_view() {
            let view = unsafe { v8::Local::<v8::ArrayBufferView>::cast_unchecked(v) };
            return Ok(view.data() as u64);
        }
        return Err(format!("cannot pass this value as '{}'", kind.name()));
    }
    let n = v.number_value(scope).unwrap_or(0.0);
    Ok(kind.word_from_f64(n))
}

/// Double bits for kinds V8 boxes from a double, the integer otherwise.
#[inline(always)]
fn fast_return(kind: Kind, raw: u64) -> u64 {
    match Prepared::convert(kind, raw) {
        Win32Result::Null => 0,
        Win32Result::Bool(b) => b as u64,
        Win32Result::I64(v) if matches!(ret_ctype(kind), CType::Int32) => v as i32 as u32 as u64,
        Win32Result::U64(v) if matches!(ret_ctype(kind), CType::Uint32) => v as u32 as u64,
        Win32Result::I64(v) => (v as f64).to_bits(),
        Win32Result::U64(v) => (v as f64).to_bits(),
        Win32Result::F64(v) => v.to_bits(),
    }
}

/// Fast entry for signatures taking values: `raw` holds a `Local` or a 32-bit integer per
/// argument, as its kind's C type says.
#[inline(always)]
unsafe fn invoke_values(opts: *mut FastApiCallbackOptions, raw: &[u64]) -> u64 {
    let opts = unsafe { &*opts };
    let p = unsafe { prepared_of(opts.data) };
    let isolate = unsafe { opts.isolate_unchecked() };
    let mut arena = TextArena::new();
    let mut words = [0u64; MAX_ARGS];
    for (i, kind) in p.args.iter().enumerate() {
        words[i] = match kind {
            Kind::I8 | Kind::I16 | Kind::I32 => kind.extend(raw[i] as u32 as i32 as i64) as u64,
            Kind::U8 | Kind::U16 | Kind::U32 => kind.extend(raw[i] as u32 as i64) as u64,
            _ => match simple_arg(
                isolate,
                *kind,
                unsafe { local_from_raw(raw[i]) },
                &mut arena,
            ) {
                Some(w) => w,
                None => return unsafe { invoke_full(opts, p, raw) },
            },
        };
    }
    fast_return(p.ret, unsafe { p.call_raw(&words[..p.arity()]) })
}

/// Fast entry for all-float signatures.
#[inline(always)]
unsafe fn invoke_floats(opts: *mut FastApiCallbackOptions, floats: &[f64]) -> u64 {
    let p = unsafe { prepared_of((*opts).data) };
    let mut words = [0u64; MAX_ARGS];
    for (i, kind) in p.args.iter().enumerate() {
        words[i] = kind.word_from_f64(floats[i]);
    }
    fast_return(p.ret, unsafe { p.call_raw(&words[..p.arity()]) })
}

/// Conversion inside the fast call that needs a scope: typed arrays, coercion and errors.
#[inline(never)]
unsafe fn invoke_full(opts: &FastApiCallbackOptions, p: &Prepared, raw: &[u64]) -> u64 {
    v8::callback_scope!(unsafe scope, opts);
    let mut arena = TextArena::new();
    let mut words = [0u64; MAX_ARGS];
    for (i, kind) in p.args.iter().enumerate() {
        words[i] = match kind {
            Kind::I8 | Kind::I16 | Kind::I32 => kind.extend(raw[i] as u32 as i32 as i64) as u64,
            Kind::U8 | Kind::U16 | Kind::U32 => kind.extend(raw[i] as u32 as i64) as u64,
            _ => match marshal_arg(scope, *kind, unsafe { local_from_raw(raw[i]) }, &mut arena) {
                Ok(w) => w,
                Err(e) => {
                    throw_type_error(scope, &format!("{}() argument {}: {e}", p.name, i + 1));
                    return 0;
                }
            },
        };
    }
    fast_return(p.ret, unsafe { p.call_raw(&words[..p.arity()]) })
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(msg) = v8::String::new(scope, message) {
        let err = v8::Exception::type_error(scope, msg);
        scope.throw_exception(err);
    }
}

// Receiver, the arguments, options. One pair per arity: the integer-returning variant for
// Void/Bool/Int32/Uint32 results, the double-returning one for Float64.
macro_rules! value_trampolines {
    ($($int:ident, $float:ident ($($a:ident),*);)*) => {
        $(
            unsafe extern "C" fn $int(
                _recv: v8::Local<v8::Value>,
                $($a: u64,)*
                opts: *mut FastApiCallbackOptions,
            ) -> u64 {
                let raw: &[u64] = &[$($a),*];
                unsafe { invoke_values(opts, raw) }
            }
            unsafe extern "C" fn $float(
                _recv: v8::Local<v8::Value>,
                $($a: u64,)*
                opts: *mut FastApiCallbackOptions,
            ) -> f64 {
                let raw: &[u64] = &[$($a),*];
                f64::from_bits(unsafe { invoke_values(opts, raw) })
            }
        )*
        const VALUE_INT: [*const c_void; MAX_ARGS + 1] = [$($int as *const c_void),*];
        const VALUE_FLOAT: [*const c_void; MAX_ARGS + 1] = [$($float as *const c_void),*];
    };
}

macro_rules! float_trampolines {
    ($($int:ident, $float:ident ($($a:ident),*);)*) => {
        $(
            unsafe extern "C" fn $int(
                _recv: v8::Local<v8::Value>,
                $($a: f64,)*
                opts: *mut FastApiCallbackOptions,
            ) -> u64 {
                let floats: &[f64] = &[$($a),*];
                unsafe { invoke_floats(opts, floats) }
            }
            unsafe extern "C" fn $float(
                _recv: v8::Local<v8::Value>,
                $($a: f64,)*
                opts: *mut FastApiCallbackOptions,
            ) -> f64 {
                let floats: &[f64] = &[$($a),*];
                f64::from_bits(unsafe { invoke_floats(opts, floats) })
            }
        )*
        const FLOAT_INT: [*const c_void; MAX_ARGS + 1] = [$($int as *const c_void),*];
        const FLOAT_FLOAT: [*const c_void; MAX_ARGS + 1] = [$($float as *const c_void),*];
    };
}

value_trampolines! {
    values_int0, values_float0 ();
    values_int1, values_float1 (a);
    values_int2, values_float2 (a, b);
    values_int3, values_float3 (a, b, c);
    values_int4, values_float4 (a, b, c, d);
    values_int5, values_float5 (a, b, c, d, e);
    values_int6, values_float6 (a, b, c, d, e, f);
    values_int7, values_float7 (a, b, c, d, e, f, g);
    values_int8, values_float8 (a, b, c, d, e, f, g, h);
}

float_trampolines! {
    floats_int0, floats_float0 ();
    floats_int1, floats_float1 (a);
    floats_int2, floats_float2 (a, b);
    floats_int3, floats_float3 (a, b, c);
    floats_int4, floats_float4 (a, b, c, d);
    floats_int5, floats_float5 (a, b, c, d, e);
    floats_int6, floats_float6 (a, b, c, d, e, f);
    floats_int7, floats_float7 (a, b, c, d, e, f, g);
    floats_int8, floats_float8 (a, b, c, d, e, f, g, h);
}

/// The API callback: unoptimized frames and the fast call's deopt path.
fn slow_call(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let p = unsafe { prepared_of(args.data()) };
    let mut arena = TextArena::new();
    let mut words = [0u64; MAX_ARGS];
    for (i, kind) in p.args.iter().enumerate() {
        words[i] = match marshal_arg(scope, *kind, args.get(i as i32), &mut arena) {
            Ok(w) => w,
            Err(e) => {
                throw_type_error(scope, &format!("{}() argument {}: {e}", p.name, i + 1));
                return;
            }
        };
    }
    // Boxed as V8 boxes the fast overload's result.
    match unsafe { p.call(&words[..p.arity()]) } {
        Win32Result::Null => retval.set_undefined(),
        Win32Result::Bool(b) => retval.set_bool(b),
        Win32Result::I64(v) if matches!(ret_ctype(p.ret), CType::Int32) => {
            retval.set_int32(v as i32)
        }
        Win32Result::U64(v) if matches!(ret_ctype(p.ret), CType::Uint32) => {
            retval.set_uint32(v as u32)
        }
        Win32Result::I64(v) => retval.set_double(v as f64),
        Win32Result::U64(v) => retval.set_double(v as f64),
        Win32Result::F64(v) => retval.set_double(v),
    }
}

/// `None` for unknown types, unresolved functions or more than `MAX_ARGS` arguments.
pub(crate) fn make_fast_bound_function<'a>(
    scope: &mut v8::PinScope<'a, '_>,
    dll: &str,
    fn_name: &str,
    ret_type: &str,
    arg_types: &[String],
) -> Option<v8::Local<'a, v8::Function>> {
    if arg_types.len() > MAX_ARGS {
        return None;
    }
    let (ret, kinds) = crate::win32_call::parse_signature(ret_type, arg_types).ok()?;
    let p = prepare(dll, fn_name, ret, &kinds).ok()?;

    let all_float = !kinds.is_empty() && kinds.iter().all(|k| k.is_float());
    let ctypes: Vec<CType> = kinds.iter().map(|k| arg_ctype(*k, all_float)).collect();
    let ret_ctype = ret_ctype(ret);
    let arity = kinds.len();
    let address = match (all_float, matches!(ret_ctype, CType::Float64)) {
        (false, false) => VALUE_INT[arity],
        (false, true) => VALUE_FLOAT[arity],
        (true, false) => FLOAT_INT[arity],
        (true, true) => FLOAT_FLOAT[arity],
    };
    let cfn = CFunction::new(address, info(&ctypes, ret_ctype));

    let ext = v8::External::new(scope, p as *const Prepared as *mut c_void);
    let tmpl = v8::FunctionTemplate::builder(slow_call)
        .data(ext.into())
        .build_fast(scope, &[cfn]);
    tmpl.get_function(scope)
}
