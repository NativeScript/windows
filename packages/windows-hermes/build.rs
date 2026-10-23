//! Links Microsoft's prebuilt Hermes (vendor/x64/<variant>/hermes.{dll,lib}) and makes the host
//! exe re-export Hermes's `napi_*` symbols so napi-sys's runtime lookup (GetProcAddress on the
//! exe, the only mode on Windows) resolves them — the same way Node/Bun expose napi to `.node`
//! modules. `jsr_*` are called directly (imported from hermes.lib). Also copies the runtime DLLs
//! next to the exe so it runs without PATH setup.
//!
//! Two vendored variants, picked by the `icu` feature (see vendor/README.md for provenance):
//!   - `icu` (default off): the `win32/x64` build + `hermes-icu.dll` (~36 MB of bundled ICU data).
//!   - no `icu` (default): the `uwp/x64` build, which uses the OS's built-in ICU and ships no
//!     separate ICU DLL — ~36 MB smaller, at the cost of requiring package identity to run.
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor");
    let variant = if cfg!(feature = "icu") { "icu" } else { "no-icu" };
    let libdir = vendor.join("x64").join(variant);

    // Import library for hermes.dll (provides jsr_* + napi_* import stubs).
    println!("cargo:rustc-link-search=native={}", libdir.display());
    println!("cargo:rustc-link-lib=dylib=hermes");

    // Forward-export every napi_* from the exe to hermes.dll, so napi-sys's
    // Library::this()+GetProcAddress finds them. napi_symbols.txt was generated from the DLL's
    // export table (llvm-objdump -p hermes.dll | grep napi_).
    let syms = std::fs::read_to_string(libdir.join("napi_symbols.txt"))
        .expect("vendor napi_symbols.txt missing");
    for name in syms.lines().map(str::trim).filter(|l| !l.is_empty()) {
        println!("cargo:rustc-link-arg=/EXPORT:{name}=hermes.{name}");
    }

    // Copy the runtime DLL(s) next to the produced exe (target/<profile>/).
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → three parents up is target/<profile>.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    if let Some(profile_dir) = out.ancestors().nth(3) {
        // hermes-icu.dll only exists in the `icu` variant; the uwp/no-icu build has no such file.
        let dlls: &[&str] = if cfg!(feature = "icu") {
            &["hermes.dll", "hermes-icu.dll"]
        } else {
            &["hermes.dll"]
        };
        for dll in dlls {
            let _ = std::fs::copy(libdir.join(dll), profile_dir.join(dll));
        }
        // The bytecode compiler the app build runs (template/framework/tools/hermes-precompile.ps1).
        let _ = std::fs::copy("vendor/x64/tools/hermesc.exe", profile_dir.join("hermesc.exe"));
    }

    println!("cargo:rerun-if-changed=vendor/x64/icu/napi_symbols.txt");
    println!("cargo:rerun-if-changed=vendor/x64/no-icu/napi_symbols.txt");
}
