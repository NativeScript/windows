//! Compiles napi-android's V8 Node-API shim (`packages/windows-v8/vendor/shim/v8-api.cpp`, shared
//! with the `windows-v8` engine package) and `csrc/env_ext.cpp` against the `v8` crate's V8 14.7
//! headers, with the same settings `packages/windows-v8/build.rs` validated.
//!
//! Linked whole-archive: the shim's `napi_*` functions are `__declspec(dllexport)`, and the
//! native-addon loader needs every one of them in `nativescript.dll`'s export table even though
//! Rust never names most of them.
use std::path::{Path, PathBuf};

fn find_v8_include() -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap();
        Path::new(&home).join(".cargo")
    });
    let src = cargo_home.join("registry").join("src");
    if let Ok(indexes) = std::fs::read_dir(&src) {
        for idx in indexes.flatten() {
            if let Ok(crates) = std::fs::read_dir(idx.path()) {
                let mut hits: Vec<PathBuf> = crates
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("v8-147."))
                            .unwrap_or(false)
                            && p.join("v8/include/v8.h").exists()
                    })
                    .collect();
                hits.sort();
                if let Some(p) = hits.pop() {
                    return p.join("v8/include");
                }
            }
        }
    }
    panic!("could not locate the v8 crate's include dir (v8-147.x/v8/include) under {src:?}");
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("..").join("packages").join("windows-v8").join("vendor");

    let mut b = cc::Build::new();
    b.cpp(true)
        .include(find_v8_include())
        .include(vendor.join("shim"))
        .include(vendor.join("napi"))
        .include(vendor.join("compat"))
        .file(vendor.join("shim/v8-api.cpp"))
        .file(manifest.join("csrc/env_ext.cpp"))
        .define("NAPI_VERSION", "8")
        // V8 14.7 (>13): the shim's modern code paths (SetAccessorProperty etc.).
        .define("__V8_13__", None)
        .std("c++20")
        .warnings(false)
        .link_lib_modifier("+whole-archive");
    if b.get_compiler().is_like_msvc() {
        b.flag("/EHsc")
            .flag("/Zc:__cplusplus")
            .define("NOMINMAX", None)
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("_SILENCE_CXX17_CODECVT_HEADER_DEPRECATION_WARNING", None)
            .define("_CRT_SECURE_NO_WARNINGS", None);
    }
    b.compile("napi_v8_shim");

    println!("cargo:rerun-if-changed=csrc/env_ext.cpp");
    println!("cargo:rerun-if-changed=../packages/windows-v8/vendor/shim/v8-api.cpp");
    println!("cargo:rerun-if-changed=../packages/windows-v8/vendor/shim/v8-api.h");
}
