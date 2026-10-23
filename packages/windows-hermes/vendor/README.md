# Vendored Hermes engine (prebuilt)

These are **committed prebuilt binaries** (no git-LFS), mirroring how
[napi-android](https://github.com/NativeScript/napi-android) vendors its engine libs under
`test-app/runtime/src/main/libs/<engine>/<abi>/`. Here the layout is `<abi>/<variant>/` (the engine
is the crate).

## Two vendored variants, selected by the `icu` Cargo feature

`build.rs` picks `x64/icu/` when built with `--features icu`, otherwise `x64/no-icu/` (the
default). See "Note on size and the `icu` feature" below for why the default is the smaller one
and what that trades off.

### `x64/icu/` (`--features icu`) — from the NuGet package's `win32/x64` build
| File | Size | Purpose |
|---|---|---|
| `hermes.dll` | ~7 MB | Hermes engine + Node-API (`napi_*`) + JSR C API (`jsr_*`), linked against the OS-independent ICU build |
| `hermes-icu.dll` | ~36 MB | Bundled ICU data that this `hermes.dll` links against |
| `hermes.lib` | ~72 KB | Import library for `hermes.dll` (used at link time) |
| `napi_symbols.txt` | — | `napi_*` export list forwarded by `build.rs` (see below) |

### `x64/no-icu/` (default) — from the NuGet package's `uwp/x64` build
| File | Size | Purpose |
|---|---|---|
| `hermes.dll` | ~6.7 MB | Hermes engine + Node-API + JSR, built against the OS's built-in ICU (no bundled ICU data) |
| `hermes.lib` | ~72 KB | Import library for `hermes.dll` |
| `napi_symbols.txt` | — | Same export list as the `icu` variant (diffed identical, byte-for-byte after filtering the one regex false-positive below) |

### `x64/tools/` — from the same package's `tools/native/release/x64`
| File | Size | Purpose |
|---|---|---|
| `hermesc.exe` | ~3.6 MB | Hermes bytecode compiler. `build.rs` copies it beside the engine DLLs and `template/build.ps1 -Engine hermes` stages it in the framework's `tools/`, where Release app builds run it (`tools/hermes-precompile.ps1`) so a launch runs `<file>.hbc` instead of compiling the bundle. It must come from the same package version as `hermes.dll`: bytecode is version-specific. |

`include/` (shared by both variants) holds the matching headers (`hermes/js_runtime_api.h`,
`node-api/js_native_api.h`) for reference; the host declares the `jsr_*` FFI directly.

## Provenance / how to refresh

Source: NuGet package **`Microsoft.JavaScript.Hermes`** (microsoft/hermes-windows). Both variants
must be refreshed from the **same** package version (confirmed via `hermes.dll`'s embedded
`FileVersion`, e.g. `0.0.2607.6001`, matching the `VER` fetched below).

```sh
VER=$(curl -s https://api.nuget.org/v3-flatcontainer/microsoft.javascript.hermes/index.json \
  | grep -oE '0\.0\.0-[0-9]+\.[0-9]+-[a-f0-9]+' | tail -1)
curl -sL -o hermes.nupkg \
  "https://api.nuget.org/v3-flatcontainer/microsoft.javascript.hermes/$VER/microsoft.javascript.hermes.$VER.nupkg"

# icu variant (win32/x64):
unzip -o hermes.nupkg 'build/native/win32/x64/*' 'build/native/include/*'
cp build/native/win32/x64/{hermes.dll,hermes-icu.dll,hermes.lib} x64/icu/
cp -r build/native/include/{hermes,node-api} include/
llvm-objdump -p x64/icu/hermes.dll | grep -oE 'napi_[a-z0-9_]+' | sort -u > x64/icu/napi_symbols.txt

# no-icu variant (uwp/x64):
unzip -o hermes.nupkg 'build/native/uwp/x64/*'
cp build/native/uwp/x64/{hermes.dll,hermes.lib} x64/no-icu/
llvm-objdump -p x64/no-icu/hermes.dll | grep -oE 'napi_[a-z0-9_]+' | sort -u > x64/no-icu/napi_symbols.txt

# bytecode compiler:
unzip -o hermes.nupkg 'tools/native/release/x64/hermesc.exe'
cp tools/native/release/x64/hermesc.exe x64/tools/
```

`napi_symbols.txt` is the list of `napi_*` exports that `build.rs` forward-exports from the host exe
so napi-sys's runtime lookup resolves them (Node/Bun do the same to expose napi to `.node` modules).

> Both DLLs' export tables happen to contain `jsr_close_napi_env_scope` /
> `jsr_open_napi_env_scope` at the identical offset, and the `napi_[a-z0-9_]+` regex matches the
> substring `napi_env_scope` inside those names — a false positive (no such standalone export
> exists in either DLL). Drop that line after regenerating, or the `/EXPORT:` linker arg `build.rs`
> emits for it will fail to resolve.

## Note on size and the `icu` feature

The 36 MB `hermes-icu.dll` dominates the `icu` variant. The `uwp/x64` build (`no-icu`, default)
uses the OS's built-in ICU instead and needs no separate ICU DLL, shrinking the vendored payload
from ~41.8 MB to ~6.8 MB (measured: `43,799,566` → `7,145,446` bytes, a ~35 MB / 82% reduction).

**Verified outcome (2026-07-27): the `no-icu` variant does NOT load in a plain, unpackaged Win32
process.** Loading `x64/no-icu/hermes.dll` (directly via `LoadLibraryEx`, and via the standalone
`nativescript-windows.exe` host built with default features) fails with `ERROR_MOD_NOT_FOUND` /
`STATUS_DLL_NOT_FOUND` (`0xC0000135`). Root cause, confirmed by diffing the two DLLs' import
tables: the `uwp/x64` `hermes.dll` links `MSVCP140_APP.dll`, `VCRUNTIME140_APP.dll`, and
`VCRUNTIME140_1_APP.dll` — the "Store/UWP" flavor of the VC++ runtime, shipped only inside the
`Microsoft.VCLibs.140.00.UWPDesktop` **framework package**. Those DLLs are resolvable only for a
process that has **package identity** (MSIX/APPX) with a declared dependency on that framework
package — even when the framework package is installed system-wide (it was, on the machine this
was tested on: `Get-AppxPackage Microsoft.VCLibs.140.00.UWPDesktop` lists it), its DLLs live under
an ACL-protected path (`C:\Program Files\WindowsApps\...`) that isn't on the ordinary DLL search
path for an unpackaged `.exe`. The `icu`/`win32` variant has no such dependency (imports only
`KERNEL32`/`ADVAPI32`/`WINMM`/UCRT) and loads and runs standalone without issue — confirmed via the
full `nativescript-windows.exe` staged demo (engine bring-up, `Windows` namespace, WinRT
round-trips, IMap/subclass/composable-ctor suites, async event loop) all passing.

**Practical consequence:**
- `--features icu` (the `win32/x64` pair): works everywhere, including the standalone
  `nativescript-windows.exe` dev/bench host. Use this for local development and for any consumer
  that isn't a packaged MSIX app.
- default (no `icu`, the `uwp/x64` single DLL): only usable inside a **packaged (MSIX) host that
  declares the `Microsoft.VCLibs.140.00.UWPDesktop` framework package dependency** — i.e. the
  `nativescript.dll` adapter (`host_dll` feature) loaded by the WinUI 3 app template, which is
  deployed as MSIX. It is **not** usable via the standalone `nativescript-windows.exe` harness
  (confirmed failing, above) since that process has no package identity. Whether it actually works
  once loaded inside a real packaged WinUI 3 host is a reasonable inference from how Desktop Bridge
  framework packages resolve, but has not itself been verified end-to-end (that would require
  building and MSIX-deploying the framework, which is outside what was checked here).
