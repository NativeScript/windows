//! Node-API binding for the NativeScript Windows (WinRT) runtime.
//!
//! Exposes the `runtime` crate to Node/Bun/Deno as a `.node` addon, providing the WinRT
//! interop surface (namespace proxies, value marshalling, delegates, the `interop.*` helpers)
//! to any Node-API-compatible host. See `docs/napi-consumption.md`.

use napi::Env;
use napi_derive::napi;

mod console_test;
mod delegate_test;
mod invoke_test;
mod proxy_test;
mod value_test;

/// Pump the calling thread's Win32 messages (WinRT async completions, cross-apartment
/// delegate invokes, `createWindow` input). Returns `true` if a message was dispatched.
#[napi]
pub fn pump_messages() -> bool {
    runtime::pump_messages()
}

/// The last JS error (message + stack), if any.
#[napi]
pub fn last_error() -> Option<String> {
    runtime::get_last_js_error()
}

/// Install the runtime's globals (`__time`, and `performance`/`console` where the host lacks
/// them) onto the host's global object.
#[napi]
pub fn install_globals(env: Env) -> napi::Result<()> {
    runtime::napi_engine::globals::install_globals(&env)
}

/// Resolve a WinRT namespace root (e.g. `getNamespace('Windows')`) as a lazy proxy: member
/// access walks metadata, classes come back as constructable proxies.
#[napi]
pub fn get_namespace(env: Env, name: String) -> napi::Result<napi::JsObject> {
    runtime::napi_engine::invoke::ensure_winrt_initialized();
    runtime::napi_engine::ns_proxy::create_namespace_proxy(&env, &name)
}

/// Generate a fresh UUID string (CoCreateGuid).
#[napi]
pub fn ns_uuid() -> String {
    runtime::napi_engine::interop::ns_uuid()
}

/// Create and show a plain Win32 top-level window; returns its `HWND` as an opaque handle.
/// `pumpMessages`/`enableAutoPump` (already used for WinRT async completions) pump its message
/// queue for free. They peek messages for the whole calling thread, not a specific window.
#[napi]
pub fn create_window(env: Env, title: String, width: i32, height: i32) -> napi::Result<napi::JsUnknown> {
    runtime::napi_engine::composition_window::create_window(&env, &title, width, height)
}

/// Attach a `Windows.UI.Composition.Compositor` instance to a window from `createWindow` and
/// return the resulting `DesktopWindowTarget` as a normal WinRT proxy. Set its `.Root` to a
/// visual to render into the window.
#[napi]
pub fn attach_compositor_to_window(
    env: Env,
    compositor: napi::JsUnknown,
    hwnd: napi::JsUnknown,
) -> napi::Result<napi::JsObject> {
    runtime::napi_engine::composition_window::attach_compositor_to_window(&env, &compositor, &hwnd)
}

/// Drain the input/lifecycle events queued for a `createWindow` window since the last poll:
/// `pointerdown {x, y, button}`, `pointerup {x, y}`, `pointermove {x, y}` (latest position only),
/// `resize {width, height}`, `close`. Coordinates are client-area pixels. Poll after `pumpMessages`.
#[napi]
pub fn poll_window_events(env: Env, hwnd: napi::JsUnknown) -> napi::Result<napi::JsObject> {
    runtime::napi_engine::composition_window::poll_window_events(&env, &hwnd)
}

/// Client-area size of a `createWindow` window as `{ width, height }`, or `null` once closed.
#[napi]
pub fn get_window_size(env: Env, hwnd: napi::JsUnknown) -> napi::Result<napi::JsUnknown> {
    runtime::napi_engine::composition_window::window_client_size(&env, &hwnd)
}

/// Set the title-bar text of a `createWindow` window.
#[napi]
pub fn set_window_title(env: Env, hwnd: napi::JsUnknown, title: String) -> napi::Result<()> {
    runtime::napi_engine::composition_window::set_window_title(&env, &hwnd, &title)
}

/// Whether the WinRT class `name` is sealed (metadata flag). Test hook: lets suites assert
/// they are really covering the composable (non-sealed, null-outer) constructor path.
#[napi]
pub fn class_is_sealed(name: String) -> Option<bool> {
    use metadata::declarations::class_declaration::ClassDeclaration;
    let declaration = metadata::meta_data_reader::MetadataReader::find_by_name(&name)?;
    let lock = declaration.read();
    lock.as_any()
        .downcast_ref::<ClassDeclaration>()
        .map(|c| c.is_sealed())
}

/// Register a third-party `.winmd` file for metadata resolution (WebView2, app types, …).
#[napi]
pub fn register_winmd(path: String) -> napi::Result<()> {
    runtime::napi_engine::interop::register_winmd(&path)
        .map_err(|e| napi::Error::from_reason(e))
}

/// Register every `.winmd` in a directory (non-recursive); returns the count registered.
#[napi]
pub fn scan_winmd_dir(dir: String) -> u32 {
    runtime::napi_engine::interop::scan_winmd_dir(&dir) as u32
}

/// Wrap a `Windows.Storage.Streams.IBuffer` as a (zero-copy where supported) ArrayBuffer.
#[napi]
pub fn array_buffer_from_buffer(env: Env, buffer: napi::JsUnknown) -> napi::Result<napi::JsUnknown> {
    runtime::napi_engine::interop::array_buffer_from_buffer(&env, &buffer)
}

/// Install the `__ns*` interop natives and the `NSWinRT.interop` JS surface (Pointer/OutParam,
/// `reference` / typed-value boxing, buffer + DateTime utilities) without touching any other
/// host global. Idempotent; `nswinrt.js` calls this on load.
#[napi]
pub fn install_interop(env: Env) -> napi::Result<()> {
    runtime::napi_engine::invoke::ensure_winrt_initialized();
    runtime::napi_engine::interop::install_interop(&env)
}

/// Install the `.NET`/BCL bridge natives and the `NSWinRT.dotnet` JS surface
/// (invoke/get/fromHandle/registerNamespace, taskToPromise/asDelegate, `NSWinRT.runOnUIThread`).
/// Idempotent; `nswinrt.js` calls this on load. A no-op at the JS layer until a
/// `dotnet-bridge/publish/DotNetBridge.dll` exists next to the app.
#[napi]
pub fn install_dotnet(env: Env) -> napi::Result<()> {
    runtime::napi_engine::dotnet::install_dotnet(&env)
}
