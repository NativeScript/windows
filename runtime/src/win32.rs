// The napi engines only use the typed path.
#![cfg_attr(not(feature = "classic"), allow(dead_code))]

//! Per-call Win32 entry points (`__nsWin32CallRaw`, JSON `__nsWin32Call`) over `win32_call`.
//!
//! Types: void bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 pointer wstr str

pub(crate) use crate::win32_call::Win32Result;
use crate::win32_call::{parse_signature, prepare, Kind, TextArena, Words, MAX_ARGS};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LoadLibraryW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
};

static DLL_CACHE: std::sync::OnceLock<Mutex<HashMap<String, usize>>> = std::sync::OnceLock::new();

fn dll_cache() -> &'static Mutex<HashMap<String, usize>> {
    DLL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolves `KNOWN_FNS` up front; entries whose DLL is absent are skipped.
#[cfg(feature = "classic")]
pub(crate) fn prewarm_known_fns() {
    use crate::win32_known_fns::KNOWN_FNS;
    for f in KNOWN_FNS {
        if let Ok((ret, args)) = parse_signature(f.ret, f.params) {
            let _ = prepare(f.dll, f.name, ret, &args);
        }
    }
}

fn load_dll(name: &str) -> Result<usize, String> {
    let mut cache = dll_cache().lock();
    if let Some(&h) = cache.get(name) {
        return Ok(h);
    }
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // A bare name searches only the app directory and System32: the current directory and PATH
    // can hold a planted DLL of the same name. A path loads exactly that file.
    let hmod = unsafe {
        if name.contains(['\\', '/']) {
            LoadLibraryW(PCWSTR(wide.as_ptr()))
        } else {
            LoadLibraryExW(
                PCWSTR(wide.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|e| format!("LoadLibrary({name}): {e}"))?
    };
    let handle = hmod.0 as usize;
    cache.insert(name.to_string(), handle);
    Ok(handle)
}

fn resolve_proc(dll_handle: usize, func_name: &str) -> Result<*mut c_void, String> {
    use windows::core::PCSTR;
    use windows::Win32::Foundation::HMODULE;
    let cname = format!("{func_name}\0");
    let hmod = HMODULE(dll_handle as *mut c_void);
    let ptr = unsafe { GetProcAddress(hmod, PCSTR(cname.as_ptr())) }
        .ok_or_else(|| format!("GetProcAddress({func_name}) failed"))?;
    Ok(ptr as *mut c_void)
}

/// Load `dll` (cached) and resolve `func_name` to its raw address.
pub(crate) fn resolve_fn(dll: &str, func_name: &str) -> Result<*mut c_void, String> {
    let handle = load_dll(dll)?;
    resolve_proc(handle, func_name)
}

pub(crate) fn ffi_type_for(ty: &str) -> Result<libffi::middle::Type, String> {
    Kind::parse(ty)
        .map(Kind::ffi_type)
        .ok_or_else(|| format!("Unknown FFI type: {ty}"))
}

/// An engine value before type tagging.
pub(crate) enum RawArgValue {
    /// Numbers, pointers and bools.
    Number(f64),
    /// wstr and str arguments.
    Str(String),
    /// A BigInt or an address.
    Word(u64),
}

impl RawArgValue {
    fn word(&self, kind: Kind, arena: &mut TextArena) -> u64 {
        match self {
            RawArgValue::Number(n) => kind.word_from_f64(*n),
            RawArgValue::Word(w) => kind.word_from_i64(*w as i64),
            RawArgValue::Str(s) if kind.is_text() => arena.push(kind, s),
            RawArgValue::Str(_) => 0,
        }
    }
}

/// Dispatch a Win32 call from pre-parsed values.
pub(crate) fn call_win32_direct(
    dll: &str,
    fn_name: &str,
    ret_type: &str,
    arg_pairs: &[(&str, RawArgValue)],
) -> Result<Win32Result, String> {
    let types: smallvec::SmallVec<[&str; MAX_ARGS]> = arg_pairs.iter().map(|(t, _)| *t).collect();
    let (ret, kinds) = parse_signature(ret_type, &types)?;
    let prepared = prepare(dll, fn_name, ret, &kinds)?;
    let mut arena = TextArena::new();
    let words: Words = kinds
        .iter()
        .zip(arg_pairs)
        .map(|(kind, (_, value))| value.word(*kind, &mut arena))
        .collect();
    Ok(unsafe { prepared.call(&words) })
}

/// Returns every named export from `dll` by parsing its PE export directory.
/// Used by `NSWinRT.win32.import(dll)` to inject all DLL functions as globals.
pub(crate) fn list_exports(dll: &str) -> Result<Vec<String>, String> {
    let handle = load_dll(dll)?;
    let base = handle as *const u8;

    let names = unsafe {
        // DOS header: first 2 bytes must be 'MZ' (0x5A4D).
        let dos_magic = base.cast::<u16>().read_unaligned();
        if dos_magic != 0x5A4D {
            return Err(format!("{dll}: not a PE file"));
        }

        // e_lfanew at offset 0x3C: offset of the NT headers.
        let e_lfanew = base.add(0x3C).cast::<i32>().read_unaligned();
        if !(4..=1_048_576).contains(&e_lfanew) {
            return Err(format!("{dll}: invalid e_lfanew"));
        }

        let nt = base.add(e_lfanew as usize);

        // NT signature must be 'PE\0\0' (0x00004550).
        if nt.cast::<u32>().read_unaligned() != 0x0000_4550 {
            return Err(format!("{dll}: bad PE signature"));
        }

        // Optional header magic at nt+24: 0x10B = PE32, 0x20B = PE32+.
        let opt_magic = nt.add(24).cast::<u16>().read_unaligned();

        // Export directory RVA is the first data-directory entry.
        // Its offset within the optional header is 96 (PE32) or 112 (PE32+).
        let export_dir_off: usize = if opt_magic == 0x020B {
            24 + 112
        } else {
            24 + 96
        };
        let export_rva = nt.add(export_dir_off).cast::<u32>().read_unaligned();
        if export_rva == 0 {
            return Ok(vec![]);
        }

        let exp = base.add(export_rva as usize);

        // IMAGE_EXPORT_DIRECTORY layout (all u32 unless noted):
        //   +0  Characteristics
        //   +4  TimeDateStamp
        //   +8  MajorVersion (u16)
        //  +10  MinorVersion (u16)
        //  +12  Name (RVA of module name string)
        //  +16  Base
        //  +20  NumberOfFunctions
        //  +24  NumberOfNames
        //  +28  AddressOfFunctions
        //  +32  AddressOfNames (array of RVAs to name strings)
        //  +36  AddressOfNameOrdinals
        let number_of_names = exp.add(24).cast::<u32>().read_unaligned() as usize;
        let names_rva = exp.add(32).cast::<u32>().read_unaligned();

        if names_rva == 0 || number_of_names == 0 {
            return Ok(vec![]);
        }

        let name_table = base.add(names_rva as usize) as *const u32;

        (0..number_of_names)
            .filter_map(|i| {
                let name_rva: u32 = name_table.add(i).read_unaligned();
                let name_ptr = base.add(name_rva as usize) as *const i8;
                std::ffi::CStr::from_ptr(name_ptr)
                    .to_str()
                    .ok()
                    .map(|s| s.to_owned())
            })
            .collect::<Vec<_>>()
    };

    Ok(names)
}

fn json_arg(ty: &str, val: &Value) -> RawArgValue {
    match ty {
        "wstr" | "str" => RawArgValue::Str(val.as_str().unwrap_or("").to_owned()),
        "bool" => RawArgValue::Number(match val {
            Value::Bool(b) => *b as u8 as f64,
            other => other.as_f64().unwrap_or(0.0),
        }),
        _ => match val.as_u64() {
            Some(w) => RawArgValue::Word(w),
            None => match val.as_i64() {
                Some(w) => RawArgValue::Word(w as u64),
                None => RawArgValue::Number(val.as_f64().unwrap_or(0.0)),
            },
        },
    }
}

/// Parse and execute a Win32 call described by a JSON request string.
/// Returns `{"value":<result>}` or `{"error":"..."}`.
pub(crate) fn call_win32_json(json: &str) -> Result<String, String> {
    let req: Value = serde_json::from_str(json).map_err(|e| format!("JSON parse error: {e}"))?;

    let dll = req["dll"].as_str().ok_or("missing 'dll'")?;
    let func_name = req["fn"].as_str().ok_or("missing 'fn'")?;
    let ret_type = req["returnType"].as_str().unwrap_or("i32");
    let args_arr = req["args"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);

    let mut pairs: Vec<(&str, RawArgValue)> = Vec::with_capacity(args_arr.len());
    for a in args_arr {
        let ty = a["type"].as_str().ok_or("arg missing 'type'")?;
        pairs.push((ty, json_arg(ty, &a["value"])));
    }

    Ok(match call_win32_direct(dll, func_name, ret_type, &pairs)? {
        Win32Result::Null => r#"{"value":null}"#.to_string(),
        Win32Result::Bool(b) => format!(r#"{{"value":{b}}}"#),
        Win32Result::I64(v) => format!(r#"{{"value":{v}}}"#),
        Win32Result::U64(v) => format!(r#"{{"value":{v}}}"#),
        Win32Result::F64(v) => format!(r#"{{"value":{v}}}"#),
    })
}
