//! Last-chance native crash reporter for the napi engine. Same technique used to root-cause
//! the standalone QuickJS host (see `packages/demo/src/crash.rs`). Windows calls the top-level
//! unhandled-exception filter on the *faulting* thread with its stack still intact, so
//! `backtrace::Backtrace::new()` from inside the filter captures the frames that led to the
//! fault. Symbolizes well only against a `debug = true` build (`npm run build:debug`); release
//! builds strip symbols.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS;

type TopLevelFilter = Option<unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32>;
#[link(name = "kernel32")]
extern "system" {
    fn SetUnhandledExceptionFilter(f: TopLevelFilter) -> TopLevelFilter;
}

static IN_HANDLER: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn handler(info: *const EXCEPTION_POINTERS) -> i32 {
    if IN_HANDLER.swap(true, Ordering::SeqCst) {
        return 1; // EXCEPTION_EXECUTE_HANDLER -> terminate
    }
    let (code, addr, access, data_addr) = if !info.is_null() {
        let rec = (*info).ExceptionRecord;
        if !rec.is_null() {
            let (acc, da) = if (*rec).NumberParameters >= 2 {
                ((*rec).ExceptionInformation[0], (*rec).ExceptionInformation[1])
            } else {
                (usize::MAX, 0)
            };
            ((*rec).ExceptionCode.0 as u32, (*rec).ExceptionAddress as usize, acc, da)
        } else {
            (0, 0, usize::MAX, 0)
        }
    } else {
        (0, 0, usize::MAX, 0)
    };
    let kind = match access {
        0 => "READ",
        1 => "WRITE",
        8 => "DEP/EXEC",
        _ => "?",
    };
    eprintln!(
        "\n=== NATIVE CRASH: code=0x{code:08X} instr=0x{addr:X} {kind} of data_addr=0x{data_addr:X} ==="
    );
    eprintln!("faulting module: {}", module_containing(addr));
    let bt = backtrace::Backtrace::new();
    eprintln!("{bt:?}");
    use std::io::Write;
    std::io::stderr().flush().ok();
    1
}

/// Resolve the loaded module (DLL/EXE) containing `addr`, by file path: `backtrace`'s symbol
/// names fall back to "nearest export in some module" when the faulting DLL ships no PDB, which
/// reads as nonsense; the module path alone is unambiguous.
fn module_containing(addr: usize) -> String {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    };
    unsafe {
        let mut h = HMODULE::default();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(addr as *const u16),
            &mut h,
        );
        if ok.is_err() || h.is_invalid() {
            return "<unknown module>".to_string();
        }
        let mut buf = [0u16; 512];
        let len = GetModuleFileNameW(Some(h), &mut buf);
        if len == 0 {
            return "<GetModuleFileNameW failed>".to_string();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// Install the top-level filter. Idempotent; safe to call from every `init()`.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        SetUnhandledExceptionFilter(Some(handler));
    }
}
