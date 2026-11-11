//! Prepared Win32 calls shared by the classic and napi engines.
//!
//! Up to `MAX_ARGS` arguments go straight to the argument registers; longer signatures use
//! libffi.

use ahash::AHashMap;
use libffi::middle::{Arg, Cif, Type};
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::mem::MaybeUninit;

/// How a value crosses the call boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Void,
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Pointer,
    /// NUL terminated UTF-16; JS strings are copied for the call.
    WStr,
    /// NUL terminated UTF-8.
    Str,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "void" => Kind::Void,
            "bool" => Kind::Bool,
            "i8" => Kind::I8,
            "u8" => Kind::U8,
            "i16" => Kind::I16,
            "u16" => Kind::U16,
            "i32" => Kind::I32,
            "u32" => Kind::U32,
            "i64" => Kind::I64,
            "u64" => Kind::U64,
            "f32" => Kind::F32,
            "f64" => Kind::F64,
            "pointer" => Kind::Pointer,
            "wstr" => Kind::WStr,
            "str" => Kind::Str,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Void => "void",
            Kind::Bool => "bool",
            Kind::I8 => "i8",
            Kind::U8 => "u8",
            Kind::I16 => "i16",
            Kind::U16 => "u16",
            Kind::I32 => "i32",
            Kind::U32 => "u32",
            Kind::I64 => "i64",
            Kind::U64 => "u64",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Pointer => "pointer",
            Kind::WStr => "wstr",
            Kind::Str => "str",
        }
    }

    #[inline]
    pub fn is_float(self) -> bool {
        matches!(self, Kind::F32 | Kind::F64)
    }

    #[inline]
    pub fn is_text(self) -> bool {
        matches!(self, Kind::WStr | Kind::Str)
    }

    #[inline]
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Kind::I8
                | Kind::U8
                | Kind::I16
                | Kind::U16
                | Kind::I32
                | Kind::U32
                | Kind::I64
                | Kind::U64
        )
    }

    /// Sign or zero extended to 64 bits: clang callees may rely on it.
    #[inline]
    pub fn extend(self, v: i64) -> i64 {
        match self {
            Kind::Bool => (v != 0) as i64,
            Kind::I8 => v as i8 as i64,
            Kind::U8 => v as u8 as i64,
            Kind::I16 => v as i16 as i64,
            Kind::U16 => v as u16 as i64,
            Kind::I32 => v as i32 as i64,
            Kind::U32 => v as u32 as i64,
            _ => v,
        }
    }

    /// Argument word for a JS number.
    #[inline]
    pub fn word_from_f64(self, n: f64) -> u64 {
        match self {
            Kind::F32 => (n as f32).to_bits() as u64,
            Kind::F64 => n.to_bits(),
            Kind::Bool => (n != 0.0 && !n.is_nan()) as u64,
            // Saturating: -1 is INVALID_HANDLE_VALUE, not 0.
            _ => self.extend(n as i64) as u64,
        }
    }

    /// Argument word for a 64-bit integer (a BigInt or an address).
    #[inline]
    pub fn word_from_i64(self, v: i64) -> u64 {
        match self {
            Kind::F32 => (v as f32).to_bits() as u64,
            Kind::F64 => (v as f64).to_bits(),
            _ => self.extend(v) as u64,
        }
    }

    pub fn ffi_type(self) -> Type {
        match self {
            Kind::Void => Type::void(),
            Kind::Bool => Type::i32(),
            Kind::I8 => Type::i8(),
            Kind::U8 => Type::u8(),
            Kind::I16 => Type::i16(),
            Kind::U16 => Type::u16(),
            Kind::I32 => Type::i32(),
            Kind::U32 => Type::u32(),
            Kind::I64 => Type::i64(),
            Kind::U64 => Type::u64(),
            Kind::F32 => Type::f32(),
            Kind::F64 => Type::f64(),
            Kind::Pointer | Kind::WStr | Kind::Str => Type::pointer(),
        }
    }
}

/// Arguments the register path takes; longer signatures go through libffi.
pub const MAX_ARGS: usize = 8;

/// Argument words in call order; an f32 sits in the low half.
pub type Words = smallvec::SmallVec<[u64; MAX_ARGS]>;

pub enum Win32Result {
    Null,
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
}

const ARENA: usize = 2048;

/// NUL terminated string copies that outlive the call; stack first, heap fallback.
pub struct TextArena {
    buf: [MaybeUninit<u16>; ARENA / 2],
    used: usize,
    heap: Vec<Box<[u16]>>,
}

impl Default for TextArena {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArena {
    #[inline(always)]
    pub fn new() -> Self {
        TextArena {
            buf: [MaybeUninit::uninit(); ARENA / 2],
            used: 0,
            heap: Vec::new(),
        }
    }

    /// Room for `units` u16 values; the caller fills them.
    #[inline]
    fn reserve(&mut self, units: usize) -> *mut u16 {
        if self.used + units <= ARENA / 2 {
            let p = unsafe { self.buf.as_mut_ptr().add(self.used) as *mut u16 };
            self.used += units;
            return p;
        }
        let mut block = vec![0u16; units].into_boxed_slice();
        let p = block.as_mut_ptr();
        self.heap.push(block);
        p
    }

    /// Room for `len` units plus the terminator; returns the buffer and its address.
    #[inline]
    pub fn wide(&mut self, len: usize) -> (&mut [u16], u64) {
        let p = self.reserve(len + 1);
        unsafe {
            *p.add(len) = 0;
            (std::slice::from_raw_parts_mut(p, len), p as u64)
        }
    }

    /// Room for a UTF-8 string of `len` bytes; the terminator is in place.
    #[inline]
    pub fn narrow(&mut self, len: usize) -> (&mut [u8], u64) {
        let p = self.reserve(len / 2 + 1) as *mut u8;
        unsafe {
            *p.add(len) = 0;
            (std::slice::from_raw_parts_mut(p, len), p as u64)
        }
    }

    pub fn push_wide(&mut self, s: &str) -> u64 {
        let len = s.encode_utf16().count();
        let (out, address) = self.wide(len);
        for (slot, unit) in out.iter_mut().zip(s.encode_utf16()) {
            *slot = unit;
        }
        address
    }

    pub fn push_narrow(&mut self, s: &str) -> u64 {
        let (out, address) = self.narrow(s.len());
        out.copy_from_slice(s.as_bytes());
        address
    }

    /// A string argument of `kind` from Rust text.
    pub fn push(&mut self, kind: Kind, s: &str) -> u64 {
        if kind == Kind::Str {
            self.push_narrow(s)
        } else {
            self.push_wide(s)
        }
    }
}

/// A Win32 function with a prepared call interface.
pub struct Prepared {
    pub dll: Box<str>,
    pub name: Box<str>,
    pub address: usize,
    pub args: Box<[Kind]>,
    pub ret: Kind,
    cif: Cif,
}

impl Prepared {
    #[inline]
    pub fn arity(&self) -> usize {
        self.args.len()
    }

    /// Returns the raw return register (RAX or XMM0 bits, X0 or D0).
    ///
    /// # Safety
    /// `words` holds one argument word per `self.args`, laid out per `Words`.
    #[inline]
    pub unsafe fn call_raw(&self, words: &[u64]) -> u64 {
        debug_assert_eq!(words.len(), self.args.len());
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        if words.len() <= MAX_ARGS {
            return unsafe { call_registers(self.address, &self.args, words, self.ret.is_float()) };
        }
        unsafe { self.call_ffi(words) }
    }

    /// Calls the function and converts the result.
    ///
    /// # Safety
    /// As [`Self::call_raw`].
    #[inline]
    pub unsafe fn call(&self, words: &[u64]) -> Win32Result {
        self.result(unsafe { self.call_raw(words) })
    }

    unsafe fn call_ffi(&self, words: &[u64]) -> u64 {
        let args: smallvec::SmallVec<[Arg<'_>; 16]> = words.iter().map(Arg::new).collect();
        let code = libffi::middle::CodePtr(self.address as *mut _);
        unsafe {
            match self.ret {
                Kind::Void => {
                    self.cif.call::<()>(code, &args);
                    0
                }
                Kind::F64 => self.cif.call::<f64>(code, &args).to_bits(),
                Kind::F32 => self.cif.call::<f32>(code, &args).to_bits() as u64,
                // libffi widens small integer results to a full word.
                _ => self.cif.call::<u64>(code, &args),
            }
        }
    }

    /// The call's result from the raw return register.
    #[inline]
    pub fn result(&self, raw: u64) -> Win32Result {
        Self::convert(self.ret, raw)
    }

    /// A `kind` result from the raw return register.
    #[inline]
    pub fn convert(kind: Kind, raw: u64) -> Win32Result {
        match kind {
            Kind::Void => Win32Result::Null,
            // BOOL is a 32-bit int; any nonzero value is true.
            Kind::Bool => Win32Result::Bool(raw as u32 != 0),
            Kind::I8 => Win32Result::I64(raw as i8 as i64),
            Kind::I16 => Win32Result::I64(raw as i16 as i64),
            Kind::I32 => Win32Result::I64(raw as i32 as i64),
            Kind::I64 => Win32Result::I64(raw as i64),
            Kind::U8 => Win32Result::U64(raw as u8 as u64),
            Kind::U16 => Win32Result::U64(raw as u16 as u64),
            Kind::U32 => Win32Result::U64(raw as u32 as u64),
            Kind::U64 | Kind::Pointer | Kind::WStr | Kind::Str => Win32Result::U64(raw),
            Kind::F32 => Win32Result::F64(f32::from_bits(raw as u32) as f64),
            Kind::F64 => Win32Result::F64(f64::from_bits(raw)),
        }
    }

    fn matches(&self, dll: &str, name: &str, ret: Kind, args: &[Kind]) -> bool {
        self.ret == ret
            && &*self.args == args
            && &*self.name == name
            && self.dll.eq_ignore_ascii_case(dll)
    }
}

// x64: a variadic call puts floats at positions 1 to 3 in both the integer and XMM register,
// so one type fits any callee; position 0 is fixed and needs two variants.
#[cfg(target_arch = "x86_64")]
unsafe fn call_registers(address: usize, args: &[Kind], words: &[u64], float_ret: bool) -> u64 {
    type IntInt = unsafe extern "C" fn(i64, ...) -> i64;
    type IntFloat = unsafe extern "C" fn(i64, ...) -> f64;
    type FloatInt = unsafe extern "C" fn(f64, ...) -> i64;
    type FloatFloat = unsafe extern "C" fn(f64, ...) -> f64;

    let mut w = [0u64; MAX_ARGS];
    w[..words.len()].copy_from_slice(words);
    let [a, b, c, d, e, f, g, h] = w;
    let (b, c, d) = (f64::from_bits(b), f64::from_bits(c), f64::from_bits(d));
    let (e, f, g, h) = (e as i64, f as i64, g as i64, h as i64);
    let first_float = args.first().is_some_and(|k| k.is_float());
    unsafe {
        match (first_float, float_ret) {
            (false, false) => {
                let fun: IntInt = std::mem::transmute(address);
                fun(a as i64, b, c, d, e, f, g, h) as u64
            }
            (false, true) => {
                let fun: IntFloat = std::mem::transmute(address);
                fun(a as i64, b, c, d, e, f, g, h).to_bits()
            }
            (true, false) => {
                let fun: FloatInt = std::mem::transmute(address);
                fun(f64::from_bits(a), b, c, d, e, f, g, h) as u64
            }
            (true, true) => {
                let fun: FloatFloat = std::mem::transmute(address);
                fun(f64::from_bits(a), b, c, d, e, f, g, h).to_bits()
            }
        }
    }
}

// arm64: X0-X7 and D0-D7 are separate, so one type taking both covers every signature.
#[cfg(target_arch = "aarch64")]
unsafe fn call_registers(address: usize, args: &[Kind], words: &[u64], float_ret: bool) -> u64 {
    type IntFn = unsafe extern "C" fn(
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
    ) -> i64;
    type FloatFn = unsafe extern "C" fn(
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
    ) -> f64;

    let mut ints = [0i64; MAX_ARGS];
    let mut floats = [0f64; MAX_ARGS];
    let (mut ni, mut nf) = (0, 0);
    for (kind, word) in args.iter().zip(words) {
        if kind.is_float() {
            floats[nf] = f64::from_bits(*word);
            nf += 1;
        } else {
            ints[ni] = *word as i64;
            ni += 1;
        }
    }
    let [a, b, c, d, e, f, g, h] = ints;
    let [p, q, r, s, t, u, v, w] = floats;
    unsafe {
        if float_ret {
            let fun: FloatFn = std::mem::transmute(address);
            fun(a, b, c, d, e, f, g, h, p, q, r, s, t, u, v, w).to_bits()
        } else {
            let fun: IntFn = std::mem::transmute(address);
            fun(a, b, c, d, e, f, g, h, p, q, r, s, t, u, v, w) as u64
        }
    }
}

thread_local! {
    // Descriptors are leaked (bounded by distinct signatures) and only used on this thread:
    // the cif is not Send.
    static PREPARED: RefCell<AHashMap<u64, Vec<&'static Prepared>>> = RefCell::new(AHashMap::new());
}

fn signature_hash(dll: &str, name: &str, ret: Kind, args: &[Kind]) -> u64 {
    let mut h = ahash::AHasher::default();
    for b in dll.bytes() {
        h.write_u8(b.to_ascii_lowercase());
    }
    h.write_u8(0);
    name.hash(&mut h);
    ret.hash(&mut h);
    args.hash(&mut h);
    h.finish()
}

/// Cached per (dll, name, signature).
pub fn prepare(
    dll: &str,
    name: &str,
    ret: Kind,
    args: &[Kind],
) -> Result<&'static Prepared, String> {
    let hash = signature_hash(dll, name, ret, args);
    let hit = PREPARED.with(|cache| {
        cache.borrow().get(&hash).and_then(|list| {
            list.iter()
                .copied()
                .find(|p| p.matches(dll, name, ret, args))
        })
    });
    if let Some(p) = hit {
        return Ok(p);
    }
    let address = crate::win32::resolve_fn(dll, name)? as usize;
    let cif = Cif::new(args.iter().map(|k| k.ffi_type()), ret.ffi_type());
    let prepared: &'static Prepared = Box::leak(Box::new(Prepared {
        dll: dll.into(),
        name: name.into(),
        address,
        args: args.into(),
        ret,
        cif,
    }));
    PREPARED.with(|cache| cache.borrow_mut().entry(hash).or_default().push(prepared));
    Ok(prepared)
}

/// Parses a signature given as type names.
pub fn parse_signature(
    ret: &str,
    args: &[impl AsRef<str>],
) -> Result<(Kind, smallvec::SmallVec<[Kind; MAX_ARGS]>), String> {
    let ret = Kind::parse(ret).ok_or_else(|| format!("Unsupported return type: {ret}"))?;
    let mut kinds = smallvec::SmallVec::new();
    for a in args {
        let a = a.as_ref();
        let kind = Kind::parse(a).ok_or_else(|| format!("Unsupported arg type: {a}"))?;
        if kind == Kind::Void {
            return Err("void is not an argument type".into());
        }
        kinds.push(kind);
    }
    Ok((ret, kinds))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signatures that exercise every register and stack position on both architectures.
    extern "C" fn mixed(a: i32, b: f64, c: f32, d: i64, e: f64, f: u8, g: f32, h: i16) -> f64 {
        a as f64
            + b * 10.0
            + c as f64 * 100.0
            + d as f64 * 1000.0
            + e * 10000.0
            + f as f64 * 100000.0
            + g as f64 * 1000000.0
            + h as f64 * 10000000.0
    }
    extern "C" fn first_float(a: f32, b: i32, c: f64, d: u16) -> i64 {
        (a * 2.0) as i64 + b as i64 * 100 + (c * 4.0) as i64 * 10000 + d as i64 * 1000000
    }
    extern "C" fn ints(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64) -> i64 {
        a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h
    }
    extern "C" fn narrow(p: *const u8) -> u8 {
        unsafe { *p.add(1) }
    }
    extern "C" fn wide(p: *const u16) -> u16 {
        unsafe { *p.add(2) }
    }
    extern "C" fn f32_ret(a: f32) -> f32 {
        a * 1.5
    }
    extern "C" fn bool_ret(a: i32) -> i32 {
        if a > 0 {
            0x100
        } else {
            0
        }
    }

    fn prepared(address: usize, ret: Kind, args: &[Kind]) -> Prepared {
        Prepared {
            dll: "test".into(),
            name: "fn".into(),
            address,
            args: args.into(),
            ret,
            cif: Cif::new(args.iter().map(|k| k.ffi_type()), ret.ffi_type()),
        }
    }

    #[test]
    fn mixed_positions_through_registers() {
        let kinds = [
            Kind::I32,
            Kind::F64,
            Kind::F32,
            Kind::I64,
            Kind::F64,
            Kind::U8,
            Kind::F32,
            Kind::I16,
        ];
        let p = prepared(mixed as usize, Kind::F64, &kinds);
        let values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, -8.0];
        let words: Vec<u64> = kinds
            .iter()
            .zip(values)
            .map(|(k, v)| k.word_from_f64(v))
            .collect();
        let expected = mixed(1, 2.0, 3.0, 4, 5.0, 6, 7.0, -8);
        match unsafe { p.call(&words) } {
            Win32Result::F64(v) => assert_eq!(v, expected),
            _ => panic!("not a float result"),
        }
        // the libffi path agrees
        assert_eq!(f64::from_bits(unsafe { p.call_ffi(&words) }), expected);
    }

    #[test]
    fn float_in_position_zero() {
        let kinds = [Kind::F32, Kind::I32, Kind::F64, Kind::U16];
        let p = prepared(first_float as usize, Kind::I64, &kinds);
        let words = [
            Kind::F32.word_from_f64(1.5),
            Kind::I32.word_from_f64(-3.0),
            Kind::F64.word_from_f64(2.25),
            Kind::U16.word_from_f64(7.0),
        ];
        match unsafe { p.call(&words) } {
            Win32Result::I64(v) => assert_eq!(v, first_float(1.5, -3, 2.25, 7)),
            _ => panic!("not an integer result"),
        }
    }

    #[test]
    fn eight_integers() {
        let kinds = [Kind::I64; 8];
        let p = prepared(ints as usize, Kind::I64, &kinds);
        let words: Vec<u64> = (1..=8).map(|i| Kind::I64.word_from_i64(i)).collect();
        match unsafe { p.call(&words) } {
            Win32Result::I64(v) => assert_eq!(v, ints(1, 2, 3, 4, 5, 6, 7, 8)),
            _ => panic!("not an integer result"),
        }
    }

    #[test]
    fn strings_from_the_arena() {
        let mut arena = TextArena::new();
        let n = prepared(narrow as usize, Kind::U8, &[Kind::Str]);
        let w = prepared(wide as usize, Kind::U16, &[Kind::WStr]);
        let a = arena.push(Kind::Str, "abc");
        let b = arena.push(Kind::WStr, "xyz");
        let big = "q".repeat(5000);
        let c = arena.push(Kind::WStr, &big);
        match unsafe { n.call(&[a]) } {
            Win32Result::U64(v) => assert_eq!(v, b'b' as u64),
            _ => panic!(),
        }
        match unsafe { w.call(&[b]) } {
            Win32Result::U64(v) => assert_eq!(v, 'z' as u64),
            _ => panic!(),
        }
        match unsafe { w.call(&[c]) } {
            Win32Result::U64(v) => assert_eq!(v, 'q' as u64),
            _ => panic!(),
        }
    }

    #[test]
    fn small_returns() {
        let f = prepared(f32_ret as usize, Kind::F32, &[Kind::F32]);
        match unsafe { f.call(&[Kind::F32.word_from_f64(2.0)]) } {
            Win32Result::F64(v) => assert_eq!(v, 3.0),
            _ => panic!(),
        }
        let b = prepared(bool_ret as usize, Kind::Bool, &[Kind::I32]);
        match unsafe { b.call(&[Kind::I32.word_from_f64(1.0)]) } {
            Win32Result::Bool(v) => assert!(v),
            _ => panic!(),
        }
        match unsafe { b.call(&[Kind::I32.word_from_f64(-1.0)]) } {
            Win32Result::Bool(v) => assert!(!v),
            _ => panic!(),
        }
    }

    #[test]
    fn system_functions() {
        let p = prepare("kernel32.dll", "GetCurrentProcessId", Kind::U32, &[]).unwrap();
        match unsafe { p.call(&[]) } {
            Win32Result::U64(v) => assert_eq!(v as u32, std::process::id()),
            _ => panic!(),
        }
        let same = prepare("KERNEL32.DLL", "GetCurrentProcessId", Kind::U32, &[]).unwrap();
        assert!(std::ptr::eq(p, same));
        let other = prepare("kernel32.dll", "GetCurrentProcessId", Kind::I32, &[]).unwrap();
        assert!(!std::ptr::eq(p, other));

        let len = prepare("kernel32.dll", "lstrlenW", Kind::I32, &[Kind::WStr]).unwrap();
        let mut arena = TextArena::new();
        let s = arena.push(Kind::WStr, "hello");
        match unsafe { len.call(&[s]) } {
            Win32Result::I64(v) => assert_eq!(v, 5),
            _ => panic!(),
        }
        assert!(prepare("kernel32.dll", "NoSuchExport", Kind::I32, &[]).is_err());
    }

    #[test]
    fn pointer_words() {
        assert_eq!(Kind::Pointer.word_from_f64(-1.0), u64::MAX);
        assert_eq!(Kind::I8.word_from_f64(-1.0), u64::MAX);
        assert_eq!(Kind::U8.word_from_f64(-1.0), 0xff);
        assert_eq!(Kind::Bool.word_from_f64(f64::NAN), 0);
        assert_eq!(Kind::U32.word_from_i64(-1), 0xffff_ffff);
    }
}
