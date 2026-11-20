//! Compiles the napi-android JSC shim (jsc-api.cpp + jsr.cpp) over JavaScriptCore's **public C API**
//! (no WebKit internals / WTF), so it builds against just the vendored `JavaScriptCore/*.h` headers.
//! The shim compile-verifies with no engine binary present (`cargo build --lib`). The runnable bin
//! (`--features jsc_link`) additionally links `vendor/x64/JavaScriptCore.lib` — drop the WinCairo
//! `JavaScriptCore.dll`/`.lib` there (see README) to build/run it.
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor");

    let mut shim = cc::Build::new();
    shim.cpp(true)
        .include(vendor.join("include")) // JavaScriptCore/*.h (public C API)
        .include(vendor.join("napi")) // js_native_api.h etc.
        .file(vendor.join("shim/jsc-api.cpp"))
        .file(vendor.join("shim/jsr.cpp"))
        .warnings(false);
    if shim.get_compiler().is_like_msvc() {
        shim.flag("/std:c++17")
            // jsc-api.cpp uses <codecvt> (deprecated in C++17, still provided by MSVC under this).
            .define("_SILENCE_CXX17_CODECVT_HEADER_DEPRECATION_WARNING", None)
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        shim.std("c++17");
    }
    shim.compile("jsc_napi_shim");

    // Runnable bin: link the (user-provided) JavaScriptCore import lib + copy the DLL beside the exe.
    if std::env::var("CARGO_FEATURE_JSC_LINK").is_ok() {
        let libdir = vendor.join("x64");
        println!("cargo:rustc-link-search=native={}", libdir.display());
        println!("cargo:rustc-link-lib=dylib=JavaScriptCore");
        if let Some(profile_dir) = profile_dir() {
            for dll in ["JavaScriptCore.dll", "icuin77.dll", "icuuc77.dll", "icudt77.dll"] {
                let _ = std::fs::copy(libdir.join(dll), profile_dir.join(dll));
            }
        }
    }

    println!("cargo:rerun-if-changed=vendor/shim/jsc-api.cpp");
    println!("cargo:rerun-if-changed=vendor/shim/jsr.cpp");
}

/// `target/<profile>`, where the exe is built: the parent of OUT_DIR's `build` ancestor. A fixed
/// depth breaks across Cargo versions: 1.100 moved OUT_DIR from `build/<pkg>-<hash>/out` to
/// `build/<pkg>/<hash>/out`.
fn profile_dir() -> Option<PathBuf> {
    let out = PathBuf::from(std::env::var("OUT_DIR").ok()?);
    let build = out.ancestors().find(|p| p.file_name() == Some("build".as_ref()))?;
    build.parent().map(PathBuf::from)
}
